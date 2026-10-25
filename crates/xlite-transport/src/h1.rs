//! HTTP/1.1 keep-alive pool: LIFO checkout, non-blocking liveness probing,
//! lazy idle reaping, and the response-body framing state machine.
//!
//! Moved verbatim out of the gateway's `src/upstream.rs` (issue #96 item 0)
//! so both the standalone binary and `xlite-transport` run **one** copy. What
//! is *not* here — retries, redials, deadlines, auth, endpoint policy — stays
//! with the caller: [`send_and_read_h1_head`] performs exactly one exchange.
//!
//! The TLS connector is supplied by the caller (feature `tls-rustls` only
//! gates the [`H1Stream::Tls`] variant and the handshake in [`dial_h1`]); the
//! crate never reads env or disk.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

use crate::attempt::Attempt;
use crate::body::BodyStream;
use crate::config::{NEVER_RECLAIM, TransportConfig};
use crate::configure_tcp_socket;
use crate::error::{StaleKind, TransportError};
use crate::stats::{Backend, HostStats, TransportStats};
#[cfg(feature = "tls-rustls")]
use crate::tls_host_from_authority;
use crate::transport::{AttemptOutcome, AttemptTelemetry, Transport};

/// Maximum response header block size accepted from an upstream (64 KiB).
const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;

/// Maximum chunk-size line length in `Transfer-Encoding: chunked`.
const MAX_CHUNK_LINE_BYTES: usize = 4096;

/// Render one HTTP/1.1 request head from the caller's method, request-target
/// and headers — verbatim, in `HeaderMap` order, with no synthesized or
/// injected headers. The caller decides policy (the gateway adds its
/// `content-type`/`connection` fields; the transport layer synthesizes only
/// `host`/`content-length` per issue #96 and reports that in its telemetry).
pub fn render_h1_head(method: &Method, path: &str, headers: &HeaderMap) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + headers.len() * 48);
    out.extend_from_slice(method.as_str().as_bytes());
    out.push(b' ');
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");
    for (name, value) in headers.iter() {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

pub struct H1Stream {
    kind: H1Kind,
    /// Gauge handle: incremented when attached, decremented when this stream
    /// is dropped — one mechanism covers every death path (idle reaping,
    /// liveness-probe rejection, mid-body close, caller discard).
    live: Option<Arc<AtomicUsize>>,
}

enum H1Kind {
    Plain(TcpStream),
    #[cfg(feature = "tls-rustls")]
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl H1Stream {
    /// A cleartext connection, not yet accounted against any pool gauge.
    pub fn plain(tcp: TcpStream) -> Self {
        Self {
            kind: H1Kind::Plain(tcp),
            live: None,
        }
    }

    /// A TLS connection, not yet accounted against any pool gauge.
    #[cfg(feature = "tls-rustls")]
    pub fn tls(stream: Box<tokio_rustls::client::TlsStream<TcpStream>>) -> Self {
        Self {
            kind: H1Kind::Tls(stream),
            live: None,
        }
    }

    /// Count this connection as live on `gauge` for the rest of its life.
    pub(crate) fn attach_live(&mut self, gauge: &Arc<AtomicUsize>) {
        gauge.fetch_add(1, Ordering::AcqRel);
        self.live = Some(Arc::clone(gauge));
    }

    fn tcp_stream(&self) -> &TcpStream {
        match &self.kind {
            H1Kind::Plain(s) => s,
            #[cfg(feature = "tls-rustls")]
            H1Kind::Tls(s) => s.get_ref().0,
        }
    }

    /// The cleartext TCP stream, when this connection is not TLS — used to
    /// write head+body with a vectored write.
    fn plain_mut(&mut self) -> Option<&mut TcpStream> {
        match &mut self.kind {
            H1Kind::Plain(tcp) => Some(tcp),
            #[cfg(feature = "tls-rustls")]
            H1Kind::Tls(_) => None,
        }
    }

    /// Non-blocking liveness probe on an idle keep-alive socket at a clean
    /// HTTP/1.1 message boundary. Any readable byte (FIN `Ok(0)`, TLS
    /// `close_notify`, or stray trailing data `Ok(n)`) or socket error other
    /// than `WouldBlock` means the connection is no longer reusable.
    fn is_healthy_idle(&self) -> bool {
        let mut probe = [0u8; 1];
        match self.tcp_stream().try_read(&mut probe) {
            Ok(0) => false,
            Ok(_) => false,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => true,
            Err(_) => false,
        }
    }
}

impl Drop for H1Stream {
    fn drop(&mut self) {
        if let Some(gauge) = &self.live {
            gauge.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl AsyncRead for H1Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.get_mut().kind {
            H1Kind::Plain(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "tls-rustls")]
            H1Kind::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for H1Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.get_mut().kind {
            H1Kind::Plain(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "tls-rustls")]
            H1Kind::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().kind {
            H1Kind::Plain(s) => Pin::new(s).poll_flush(cx),
            #[cfg(feature = "tls-rustls")]
            H1Kind::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().kind {
            H1Kind::Plain(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "tls-rustls")]
            H1Kind::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

pub struct IdleH1Conn {
    stream: H1Stream,
    since: Instant,
}

pub struct HostPool {
    max_idle: usize,
    idle_timeout: Duration,
    idle: Mutex<VecDeque<IdleH1Conn>>,
    /// Live connections (idle + checked out): +1 when a dial attaches, -1 on
    /// every `H1Stream` drop — see `H1Stream::attach_live`.
    live: Arc<AtomicUsize>,
    /// Idle connections dropped because they expired (issue #96: h1 does
    /// count reclamation, so `reaped_idle` is `Some`).
    reaped: AtomicU64,
    /// Per-host dial gate: bounds how many SYNs a cold pool can fire at this
    /// host at once (see [`HostPool::acquire_dial`]).
    dial_gate: Arc<tokio::sync::Semaphore>,
}

impl HostPool {
    fn new(max_idle: usize, idle_timeout: Duration, max_parallel_dials: usize) -> Self {
        Self {
            max_idle,
            idle_timeout,
            idle: Mutex::new(VecDeque::new()),
            live: Arc::new(AtomicUsize::new(0)),
            reaped: AtomicU64::new(0),
            dial_gate: Arc::new(tokio::sync::Semaphore::new(max_parallel_dials.max(1))),
        }
    }

    /// Hold one dial slot for this host while connecting (TCP + TLS).
    ///
    /// A cold pool can otherwise fire every waiting request's SYN at once;
    /// the kernel silently drops whatever exceeds the server's accept queue
    /// and each dropped SYN costs ~1s in retransmit delay (xlite#96: a
    /// `listen(5)` mock turned a 16-way burst into 1–2s of first-wave
    /// latency). The gate is **per host** — different authorities have
    /// different accept queues and must not throttle each other. Holders
    /// release as soon as `connect()` returns, so on a LAN the ramp is
    /// sub-millisecond.
    pub async fn acquire_dial(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.dial_gate
            .clone()
            .acquire_owned()
            .await
            .expect("dial gate is never closed")
    }

    /// Checkout the warmest healthy idle connection (LIFO), reaping expired
    /// connections from the front first.
    pub fn take_idle(&self) -> Option<H1Stream> {
        loop {
            let candidate = {
                let mut guard = self.idle.lock().expect("h1 pool mutex poisoned");
                let mut expired = 0usize;
                while guard
                    .front()
                    .is_some_and(|c| c.since.elapsed() >= self.idle_timeout)
                {
                    guard.pop_front();
                    expired += 1;
                }
                if expired > 0 {
                    self.reaped.fetch_add(expired as u64, Ordering::Relaxed);
                }
                guard.pop_back()
            }?;
            if candidate.stream.is_healthy_idle() {
                return Some(candidate.stream);
            }
        }
    }

    /// Return a connection that completed a clean HTTP/1.1 response body back
    /// to the LIFO idle pool.
    pub fn put(&self, stream: H1Stream) {
        if self.max_idle == 0 || !stream.is_healthy_idle() {
            return;
        }
        if let Ok(mut guard) = self.idle.lock() {
            let mut expired = 0usize;
            while guard
                .front()
                .is_some_and(|c| c.since.elapsed() >= self.idle_timeout)
            {
                guard.pop_front();
                expired += 1;
            }
            if expired > 0 {
                self.reaped.fetch_add(expired as u64, Ordering::Relaxed);
            }
            if guard.len() >= self.max_idle {
                guard.pop_front();
            }
            guard.push_back(IdleH1Conn {
                stream,
                since: Instant::now(),
            });
        }
    }
}

pub struct H1Pool {
    max_idle_per_host: usize,
    idle_timeout: Duration,
    max_parallel_dials: usize,
    plain_hosts: ArcSwap<HashMap<String, Arc<HostPool>>>,
    tls_hosts: ArcSwap<HashMap<String, Arc<HostPool>>>,
    mutate: Mutex<()>,
}

impl H1Pool {
    pub fn new(
        max_idle_per_host: usize,
        idle_timeout: Duration,
        max_parallel_dials: usize,
    ) -> Self {
        Self {
            max_idle_per_host,
            idle_timeout,
            max_parallel_dials,
            plain_hosts: ArcSwap::from_pointee(HashMap::new()),
            tls_hosts: ArcSwap::from_pointee(HashMap::new()),
            mutate: Mutex::new(()),
        }
    }

    pub fn host_pool(&self, is_tls: bool, authority: &str) -> Arc<HostPool> {
        let map = if is_tls {
            &self.tls_hosts
        } else {
            &self.plain_hosts
        };
        if let Some(pool) = map.load().get(authority) {
            return Arc::clone(pool);
        }
        let _guard = self.mutate.lock().expect("h1 hosts mutex poisoned");
        let current = map.load();
        if let Some(pool) = current.get(authority) {
            return Arc::clone(pool);
        }
        let pool = Arc::new(HostPool::new(
            self.max_idle_per_host,
            self.idle_timeout,
            self.max_parallel_dials,
        ));
        let mut next = (**current).clone();
        next.insert(authority.to_string(), Arc::clone(&pool));
        map.store(Arc::new(next));
        pool
    }
}

/// Open one upstream connection: TCP connect (bounded by `connect_timeout`)
/// and, when `tls_host` is given, a TLS handshake on top.
///
/// Failures are tagged by `io::ErrorKind` so the transport layer can report
/// `Dns` / `Connect` / `Tls` without string matching:
/// `AddrNotAvailable` = name resolution, `InvalidData` = TLS handshake,
/// everything else = connect.
pub async fn dial_h1(
    authority: &str,
    tls_host: Option<&str>,
    #[cfg(feature = "tls-rustls")] tls_connector: Option<&tokio_rustls::TlsConnector>,
    connect_timeout: Duration,
) -> io::Result<H1Stream> {
    let tcp = tokio::time::timeout(connect_timeout, TcpStream::connect(authority))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connect timeout after {}s", connect_timeout.as_secs()),
            )
        })?
        .map_err(|e| io::Error::new(e.kind(), format!("connect: {e}")))?;
    configure_tcp_socket(&tcp);

    #[cfg(feature = "tls-rustls")]
    let Some(host) = tls_host else {
        return Ok(H1Stream::plain(tcp));
    };
    #[cfg(feature = "tls-rustls")]
    {
        let tls_connector = tls_connector.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tls host supplied but no tls connector configured",
            )
        })?;
        let clean_host = host.trim_start_matches('[').trim_end_matches(']');
        let server_name =
            rustls_pki_types::ServerName::try_from(clean_host.to_owned()).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid TLS host `{clean_host}`: {e}"),
                )
            })?;
        let tls_stream =
            tokio::time::timeout(connect_timeout, tls_connector.connect(server_name, tcp))
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "upstream TLS handshake timeout after {}s",
                            connect_timeout.as_secs()
                        ),
                    )
                })?
                // Tagged InvalidData so callers report `Tls`.
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("tls: {e}")))?;
        Ok(H1Stream::tls(Box::new(tls_stream)))
    }
    #[cfg(not(feature = "tls-rustls"))]
    {
        match tls_host {
            // Plain HTTP/1.1 — the whole point of a `["h1"]`-only build.
            None => Ok(H1Stream::plain(tcp)),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tls host supplied but built without tls-rustls",
            )),
        }
    }
}

#[derive(Debug)]
pub struct ParsedResponseHead {
    pub status: u16,
    /// `0` = HTTP/1.0, `1` = HTTP/1.1 (what the status line declared).
    pub http_version: u8,
    /// Response headers verbatim, duplicates preserved (append order within a
    /// name matches the wire; `HeaderMap` iteration order is internal).
    pub headers: HeaderMap,
    pub content_type: Option<String>,
    pub retry_after: Option<u64>,
    pub framing: BodyFraming,
    pub connection_close: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFraming {
    Empty,
    ContentLength(u64),
    Chunked(ChunkState),
    UntilClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkState {
    Size,
    Data(u64),
    DataCrlf,
    Trailers,
}

/// Write the head (+ body, coalesced where it helps) and read the response
/// head, skipping 1xx informational responses.
///
/// Exactly one request→response-headers exchange: no redial, no retry — that
/// policy belongs to the caller (see `execute_redial_once` on the transport).
pub async fn send_and_read_h1_head(
    stream: &mut H1Stream,
    req_head: &[u8],
    body: &Bytes,
    buf: &mut BytesMut,
) -> io::Result<ParsedResponseHead> {
    if body.is_empty() {
        stream.write_all(req_head).await?;
    } else if let Some(tcp) = stream.plain_mut() {
        write_vectored_all(tcp, req_head, body).await?;
    } else if req_head.len() + body.len() <= 16 * 1024 {
        let mut combined = Vec::with_capacity(req_head.len() + body.len());
        combined.extend_from_slice(req_head);
        combined.extend_from_slice(body);
        stream.write_all(&combined).await?;
    } else {
        stream.write_all(req_head).await?;
        stream.write_all(body).await?;
    }
    stream.flush().await?;

    loop {
        while find_header_end(buf).is_none() {
            if buf.len() >= MAX_RESPONSE_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "upstream response headers exceeded 64 KiB",
                ));
            }
            let n = stream.read_buf(buf).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "upstream closed connection before sending response headers",
                ));
            }
        }
        let head = parse_h1_response_head(buf)?;
        // Skip 1xx informational responses (e.g. `100 Continue`) and wait for
        // the final response header block.
        if (100..200).contains(&head.status) && head.status != 101 {
            continue;
        }
        return Ok(head);
    }
}

async fn write_vectored_all(stream: &mut TcpStream, mut a: &[u8], mut b: &[u8]) -> io::Result<()> {
    while !a.is_empty() || !b.is_empty() {
        let n = if !a.is_empty() {
            let slices = [io::IoSlice::new(a), io::IoSlice::new(b)];
            stream.write_vectored(&slices).await?
        } else {
            return stream.write_all(b).await;
        };
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write whole request buffer",
            ));
        }
        if n <= a.len() {
            a = &a[n..];
        } else {
            let rem = n - a.len();
            a = &[];
            b = &b[rem..];
        }
    }
    Ok(())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn parse_h1_response_head(buf: &mut BytesMut) -> io::Result<ParsedResponseHead> {
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut resp = httparse::Response::new(&mut headers);
    let head_len = match resp.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete HTTP/1.1 response header block",
            ));
        }
        Err(e) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed HTTP/1.1 response headers: {e}"),
            ));
        }
    };
    let status = resp.code.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing HTTP response status")
    })?;
    let version = resp.version.unwrap_or(1);

    let mut content_type = None;
    let mut retry_after: Option<u64> = None;
    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    let mut conn_close = false;
    let mut conn_keep_alive = false;
    let mut response_headers = HeaderMap::with_capacity(resp.headers.len());

    for h in resp.headers.iter() {
        if let Ok(name) = HeaderName::from_bytes(h.name.as_bytes())
            && let Ok(value) = HeaderValue::from_bytes(h.value)
        {
            response_headers.append(name, value);
        }
        if h.name.eq_ignore_ascii_case("content-type") {
            if let Ok(v) = std::str::from_utf8(h.value) {
                content_type = Some(v.trim().to_string());
            }
        } else if h.name.eq_ignore_ascii_case("retry-after") {
            retry_after = std::str::from_utf8(h.value)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok());
        } else if h.name.eq_ignore_ascii_case("content-length") {
            let s = std::str::from_utf8(h.value).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length header")
            })?;
            let parsed = s.trim().parse::<u64>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length value")
            })?;
            content_length = Some(parsed);
        } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
            if let Ok(v) = std::str::from_utf8(h.value)
                && v.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("chunked"))
            {
                chunked = true;
            }
        } else if h.name.eq_ignore_ascii_case("connection")
            && let Ok(v) = std::str::from_utf8(h.value)
        {
            for token in v.split(',') {
                let t = token.trim();
                if t.eq_ignore_ascii_case("close") {
                    conn_close = true;
                } else if t.eq_ignore_ascii_case("keep-alive") {
                    conn_keep_alive = true;
                }
            }
        }
    }

    let _ = buf.split_to(head_len);
    let connection_close = conn_close || (version == 0 && !conn_keep_alive);
    let framing = if (100..200).contains(&status) || status == 204 || status == 304 {
        BodyFraming::Empty
    } else if chunked {
        BodyFraming::Chunked(ChunkState::Size)
    } else if let Some(len) = content_length {
        if len == 0 {
            BodyFraming::Empty
        } else {
            BodyFraming::ContentLength(len)
        }
    } else {
        BodyFraming::UntilClose
    };

    Ok(ParsedResponseHead {
        status,
        http_version: version,
        headers: response_headers,
        content_type,
        retry_after,
        framing,
        connection_close,
    })
}

/// One response body over a pooled keep-alive socket. Recycles the socket
/// into the pool only after a **complete** body; any truncation surfaces as
/// `Err(io::ErrorKind::UnexpectedEof)`, never as a clean end.
pub struct H1Body {
    pub stream: Option<H1Stream>,
    pub buf: BytesMut,
    pub framing: BodyFraming,
    pub can_reuse: bool,
    pub host_pool: Arc<HostPool>,
}

impl H1Body {
    /// If the response body is already complete (e.g. `Content-Length: 0` or
    /// `204 No Content`), return the socket to the idle pool immediately.
    pub fn try_eager_recycle(&mut self) {
        if matches!(self.framing, BodyFraming::Empty) {
            self.finish_and_recycle();
        }
    }

    fn finish_and_recycle(&mut self) {
        self.framing = BodyFraming::Empty;
        if let Some(stream) = self.stream.take()
            && self.can_reuse
            && self.buf.is_empty()
        {
            self.host_pool.put(stream);
        }
    }

    /// Opportunistically consume the final `\r\n0\r\n\r\n` tail when it was
    /// already buffered alongside the last chunk data, allowing the connection
    /// to return to the pool without waiting for an extra poll cycle.
    fn try_consume_buffered_chunked_tail(&mut self) {
        if matches!(self.framing, BodyFraming::Chunked(ChunkState::DataCrlf))
            && self.buf.starts_with(b"\r\n0\r\n\r\n")
        {
            let _ = self.buf.split_to(7);
            self.finish_and_recycle();
        }
    }

    fn poll_read_more(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let Some(stream) = self.stream.as_mut() else {
            return Poll::Ready(Ok(0));
        };
        let mut temp = [0u8; 16 * 1024];
        let mut read_buf = ReadBuf::new(&mut temp);
        match Pin::new(stream).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled();
                if filled.is_empty() {
                    Poll::Ready(Ok(0))
                } else {
                    self.buf.extend_from_slice(filled);
                    Poll::Ready(Ok(filled.len()))
                }
            }
            Poll::Ready(Err(e)) => {
                self.stream = None;
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    pub fn poll_next_inner(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Result<Bytes>>> {
        loop {
            match self.framing {
                BodyFraming::Empty => {
                    // Nothing more is expected; an EOF before the framing
                    // completed was already turned into an error above.
                    self.finish_and_recycle();
                    return Poll::Ready(None);
                }
                BodyFraming::ContentLength(remaining) => {
                    if !self.buf.is_empty() {
                        let take = (remaining as usize).min(self.buf.len());
                        let chunk = self.buf.split_to(take).freeze();
                        let next_rem = remaining - take as u64;
                        if next_rem == 0 {
                            self.finish_and_recycle();
                        } else {
                            self.framing = BodyFraming::ContentLength(next_rem);
                        }
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    match self.poll_read_more(cx) {
                        Poll::Ready(Ok(0)) => {
                            self.stream = None;
                            self.framing = BodyFraming::Empty;
                            return Poll::Ready(Some(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "upstream closed before Content-Length completed",
                            ))));
                        }
                        Poll::Ready(Ok(_)) => continue,
                        Poll::Ready(Err(e)) => {
                            self.framing = BodyFraming::Empty;
                            return Poll::Ready(Some(Err(e)));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                BodyFraming::UntilClose => {
                    if !self.buf.is_empty() {
                        return Poll::Ready(Some(Ok(self.buf.split().freeze())));
                    }
                    match self.poll_read_more(cx) {
                        Poll::Ready(Ok(0)) => {
                            self.stream = None;
                            self.framing = BodyFraming::Empty;
                            return Poll::Ready(None);
                        }
                        Poll::Ready(Ok(_)) => continue,
                        Poll::Ready(Err(e)) => {
                            self.framing = BodyFraming::Empty;
                            return Poll::Ready(Some(Err(e)));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                BodyFraming::Chunked(state) => match state {
                    ChunkState::Size => {
                        if let Some(line_end) = self.buf.windows(2).position(|w| w == b"\r\n") {
                            let size = match parse_chunk_size(&self.buf[..line_end]) {
                                Ok(s) => s,
                                Err(e) => {
                                    self.stream = None;
                                    self.framing = BodyFraming::Empty;
                                    return Poll::Ready(Some(Err(e)));
                                }
                            };
                            let _ = self.buf.split_to(line_end + 2);
                            if size == 0 {
                                self.framing = BodyFraming::Chunked(ChunkState::Trailers);
                            } else {
                                self.framing = BodyFraming::Chunked(ChunkState::Data(size));
                            }
                            continue;
                        }
                        if self.buf.len() > MAX_CHUNK_LINE_BYTES {
                            self.stream = None;
                            self.framing = BodyFraming::Empty;
                            return Poll::Ready(Some(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "upstream chunk size line exceeded limit",
                            ))));
                        }
                        match self.poll_read_more(cx) {
                            Poll::Ready(Ok(0)) => {
                                self.stream = None;
                                self.framing = BodyFraming::Empty;
                                return Poll::Ready(Some(Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "upstream closed mid-chunked stream",
                                ))));
                            }
                            Poll::Ready(Ok(_)) => continue,
                            Poll::Ready(Err(e)) => {
                                self.framing = BodyFraming::Empty;
                                return Poll::Ready(Some(Err(e)));
                            }
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                    ChunkState::Data(remaining) => {
                        if !self.buf.is_empty() {
                            let take = (remaining as usize).min(self.buf.len());
                            let chunk = self.buf.split_to(take).freeze();
                            let next_rem = remaining - take as u64;
                            if next_rem == 0 {
                                self.framing = BodyFraming::Chunked(ChunkState::DataCrlf);
                                self.try_consume_buffered_chunked_tail();
                            } else {
                                self.framing = BodyFraming::Chunked(ChunkState::Data(next_rem));
                            }
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        match self.poll_read_more(cx) {
                            Poll::Ready(Ok(0)) => {
                                self.stream = None;
                                self.framing = BodyFraming::Empty;
                                return Poll::Ready(Some(Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "upstream closed mid-chunk data",
                                ))));
                            }
                            Poll::Ready(Ok(_)) => continue,
                            Poll::Ready(Err(e)) => {
                                self.framing = BodyFraming::Empty;
                                return Poll::Ready(Some(Err(e)));
                            }
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                    ChunkState::DataCrlf => {
                        if self.buf.len() >= 2 {
                            if &self.buf[..2] != b"\r\n" {
                                self.stream = None;
                                self.framing = BodyFraming::Empty;
                                return Poll::Ready(Some(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "missing CRLF after chunk payload",
                                ))));
                            }
                            let _ = self.buf.split_to(2);
                            self.framing = BodyFraming::Chunked(ChunkState::Size);
                            continue;
                        }
                        match self.poll_read_more(cx) {
                            Poll::Ready(Ok(0)) => {
                                self.stream = None;
                                self.framing = BodyFraming::Empty;
                                return Poll::Ready(Some(Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "upstream closed before chunk trailing CRLF",
                                ))));
                            }
                            Poll::Ready(Ok(_)) => continue,
                            Poll::Ready(Err(e)) => {
                                self.framing = BodyFraming::Empty;
                                return Poll::Ready(Some(Err(e)));
                            }
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                    ChunkState::Trailers => {
                        if self.buf.starts_with(b"\r\n") {
                            let _ = self.buf.split_to(2);
                            self.finish_and_recycle();
                            return Poll::Ready(None);
                        }
                        if let Some(end) = find_header_end(&self.buf) {
                            let _ = self.buf.split_to(end);
                            self.finish_and_recycle();
                            return Poll::Ready(None);
                        }
                        if self.buf.len() > MAX_RESPONSE_HEADER_BYTES {
                            self.stream = None;
                            self.framing = BodyFraming::Empty;
                            return Poll::Ready(Some(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "upstream chunk trailers exceeded limit",
                            ))));
                        }
                        match self.poll_read_more(cx) {
                            Poll::Ready(Ok(0)) => {
                                self.stream = None;
                                self.framing = BodyFraming::Empty;
                                return Poll::Ready(Some(Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "upstream closed before final chunk CRLF",
                                ))));
                            }
                            Poll::Ready(Ok(_)) => continue,
                            Poll::Ready(Err(e)) => {
                                self.framing = BodyFraming::Empty;
                                return Poll::Ready(Some(Err(e)));
                            }
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                },
            }
        }
    }
}

fn parse_chunk_size(line: &[u8]) -> io::Result<u64> {
    let raw = match line.iter().position(|&b| b == b';') {
        Some(idx) => &line[..idx],
        None => line,
    };
    let s = std::str::from_utf8(raw)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-ASCII chunk size"))?
        .trim();
    if s.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty chunk size",
        ));
    }
    u64::from_str_radix(s, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid hex chunk size"))
}

// ---------------------------------------------------------------------------
// H1Transport: the `Transport` contract over the pool above.
// ---------------------------------------------------------------------------

/// Per-authority counters (cold map insert; the read path is an `ArcSwap`
/// load, like the pool's own host maps).
#[derive(Default)]
struct HostCounters {
    /// Successful dials (attempts that failed count as `failed_dials`).
    dials: AtomicU64,
    failed_dials: AtomicU64,
    /// Checkouts served from a warm idle connection.
    reused: AtomicU64,
    /// Sockets discarded because the pool stopped trusting them.
    evicted: AtomicU64,
    /// Subset: a **reused** socket failed before the first response byte —
    /// the class `execute_redial_once` may redial.
    evicted_stale: AtomicU64,
}

/// HTTP/1.1 upstream transport, pooled per `scheme://authority`.
///
/// Exactly one request→response exchange per `execute()`: no redial, no
/// retry. The stale-keep-alive race is surfaced as
/// [`TransportError::StaleConnection`] for the caller's helper to decide on.
pub struct H1Transport {
    pool: H1Pool,
    counters: ArcSwap<HashMap<String, Arc<HostCounters>>>,
    mutate: Mutex<()>,
    header_timeout: Option<Duration>,
    stall_timeout: Option<Duration>,
    connect_timeout: Duration,
    #[cfg(feature = "tls-rustls")]
    tls: Option<tokio_rustls::TlsConnector>,
}

impl H1Transport {
    /// Build a transport over TCP (and, for `https`, TLS with the caller's
    /// [`rustls::ClientConfig`]).
    pub fn new(cfg: TransportConfig) -> Self {
        let pool = H1Pool::new(
            cfg.h1_max_conns_per_host.max(1),
            cfg.pool_idle_reclaim.unwrap_or(NEVER_RECLAIM),
            cfg.max_parallel_dials,
        );
        #[cfg(feature = "tls-rustls")]
        let tls = cfg
            .tls
            .as_ref()
            .map(|c| tokio_rustls::TlsConnector::from(Arc::clone(c)));
        Self {
            pool,
            counters: ArcSwap::from_pointee(HashMap::new()),
            mutate: Mutex::new(()),
            header_timeout: cfg.header_timeout,
            stall_timeout: cfg.stall_timeout,
            connect_timeout: cfg.connect_timeout,
            #[cfg(feature = "tls-rustls")]
            tls,
        }
    }

    fn counters_for(&self, key: &str) -> Arc<HostCounters> {
        if let Some(found) = self.counters.load().get(key) {
            return Arc::clone(found);
        }
        let _guard = self.mutate.lock().expect("h1 counters mutex poisoned");
        let current = self.counters.load();
        if let Some(found) = current.get(key) {
            return Arc::clone(found);
        }
        let fresh = Arc::new(HostCounters::default());
        let mut next = (**current).clone();
        next.insert(key.to_string(), Arc::clone(&fresh));
        self.counters.store(Arc::new(next));
        fresh
    }
}

impl Transport for H1Transport {
    async fn execute(&self, attempt: Attempt) -> Result<AttemptOutcome, TransportError> {
        // 1. Everything checkable without a socket, checked before any I/O.
        attempt.validate()?;
        let scheme = attempt
            .uri
            .scheme_str()
            .ok_or(TransportError::InvalidRequest("uri has no scheme"))?;
        if scheme != "http" && scheme != "https" {
            return Err(TransportError::InvalidRequest(
                "only http/https authorities are served by the h1 backend",
            ));
        }
        #[cfg(not(feature = "tls-rustls"))]
        if scheme == "https" {
            return Err(TransportError::InvalidRequest(
                "https authority but the transport was built without tls-rustls",
            ));
        }
        if attempt
            .headers
            .contains_key(http::header::TRANSFER_ENCODING)
        {
            return Err(TransportError::InvalidRequest(
                "the h1 backend sends `Bytes` with content-length; transfer-encoding is caller-encoded or unsupported",
            ));
        }
        let authority = attempt
            .uri
            .authority()
            .map(|a| a.as_str().to_string())
            .ok_or(TransportError::InvalidRequest("uri has no authority"))?;
        let is_tls = scheme == "https";
        let key = format!("{scheme}://{authority}");
        let counters = self.counters_for(&key);
        let host_pool = self.pool.host_pool(is_tls, &authority);

        // 2. Warm checkout or dial.
        let dial_started = std::time::Instant::now();
        let (mut stream, reused) = match host_pool.take_idle() {
            Some(stream) => {
                counters.reused.fetch_add(1, Ordering::Relaxed);
                (stream, true)
            }
            None => {
                #[cfg(feature = "tls-rustls")]
                let tls_host = if is_tls {
                    Some(tls_host_from_authority(&authority))
                } else {
                    None
                };
                #[cfg(not(feature = "tls-rustls"))]
                let tls_host: Option<&str> = None;
                // One dial slot per host: the SYN burst of a cold pool must fit
                // the peer's accept queue (xlite#96).
                let _dial_permit = host_pool.acquire_dial().await;
                match dial_h1(
                    &authority,
                    tls_host,
                    #[cfg(feature = "tls-rustls")]
                    self.tls.as_ref(),
                    self.connect_timeout,
                )
                .await
                {
                    Ok(mut stream) => {
                        stream.attach_live(&host_pool.live);
                        counters.dials.fetch_add(1, Ordering::Relaxed);
                        (stream, false)
                    }
                    Err(err) => {
                        counters.failed_dials.fetch_add(1, Ordering::Relaxed);
                        return Err(map_dial_error(err));
                    }
                }
            }
        };
        let dial_time = if reused {
            None
        } else {
            Some(dial_started.elapsed())
        };

        // 3. One exchange: caller headers verbatim plus the two mechanical
        // rewrites reported in the telemetry.
        let (req_head, rewritten) = render_attempt_head(&attempt);
        let mut buf = BytesMut::new();
        let head = match self.header_timeout {
            Some(budget) => {
                match tokio::time::timeout(
                    budget,
                    send_and_read_h1_head(&mut stream, &req_head, &attempt.body, &mut buf),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        // The socket proved it cannot answer within the
                        // caller's budget: drop it (live gauge follows) and
                        // say so once — never a second clock.
                        drop(stream);
                        counters.evicted.fetch_add(1, Ordering::Relaxed);
                        return Err(TransportError::HeaderTimeout { waited: budget });
                    }
                }
            }
            None => send_and_read_h1_head(&mut stream, &req_head, &attempt.body, &mut buf).await,
        };
        let head = match head {
            Ok(head) => head,
            Err(err) => {
                drop(stream);
                if reused {
                    // Pre-headers failure on a warm socket: the classic
                    // stale-keep-alive race. Count it, type it, let the
                    // caller's helper decide about one redial.
                    counters.evicted.fetch_add(1, Ordering::Relaxed);
                    counters.evicted_stale.fetch_add(1, Ordering::Relaxed);
                    return Err(TransportError::StaleConnection {
                        kind: stale_kind_of(&err),
                    });
                }
                // A fresh connection dying before the first byte is a
                // connect-level failure: never redial-eligible.
                return Err(TransportError::Io(err));
            }
        };

        // 4. Hand the body over; dropping it closes the socket (no recycle),
        // which is exactly the upstream-visible cancellation.
        let status = StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY);
        let version = if head.http_version == 0 {
            Version::HTTP_10
        } else {
            Version::HTTP_11
        };
        let headers = head.headers;
        let can_reuse = !head.connection_close && !matches!(head.framing, BodyFraming::UntilClose);
        let mut body = H1Body {
            stream: Some(stream),
            buf,
            framing: head.framing,
            can_reuse,
            host_pool,
        };
        body.try_eager_recycle();

        Ok(AttemptOutcome {
            status,
            headers,
            version,
            body: BodyStream::new(H1BodyStream { inner: body }, self.stall_timeout),
            telemetry: AttemptTelemetry {
                dial: dial_time,
                reused,
                // HTTP/1.1 has no admission queue: there is nothing to park
                // behind, so wait is "not measured" by construction.
                pool_wait: None,
                // One exchange per connection; nothing is multiplexed.
                in_flight_at_acquire: 0,
                redials: 0,
                redial_reason: None,
                headers_rewritten: rewritten,
            },
        })
    }

    fn stats(&self) -> TransportStats {
        let counters = self.counters.load();
        let mut hosts: Vec<HostStats> = counters
            .iter()
            .map(|(key, c)| {
                let (scheme, authority) = key.split_once("://").unwrap_or(("http", key.as_str()));
                let host_pool = self.pool.host_pool(scheme == "https", authority);
                HostStats {
                    authority: authority.to_string(),
                    backend: Backend::H1,
                    connections: host_pool.live.load(Ordering::Acquire) as u64,
                    in_flight: 0,
                    dials: c.dials.load(Ordering::Relaxed),
                    failed_dials: c.failed_dials.load(Ordering::Relaxed),
                    reused: c.reused.load(Ordering::Relaxed),
                    evicted: c.evicted.load(Ordering::Relaxed),
                    evicted_stale: c.evicted_stale.load(Ordering::Relaxed),
                    reaped_idle: Some(host_pool.reaped.load(Ordering::Relaxed)),
                    waiting: 0,
                    wait_ms_p50: None,
                    wait_ms_p99: None,
                    wait_samples: 0,
                }
            })
            .collect();
        hosts.sort_by(|a, b| a.authority.cmp(&b.authority));
        TransportStats { hosts }
    }
}

/// The response body of one h1 attempt: every I/O error after the head means
/// the body was cut short, so it surfaces as `BodyIncomplete` — an abnormal
/// EOF must never look like a clean end (issue #96 rule 5). A *clean* end of
/// an `UntilClose` body is `None`, by definition of that framing.
struct H1BodyStream {
    inner: H1Body,
}

impl Stream for H1BodyStream {
    type Item = Result<Bytes, TransportError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.poll_next_inner(cx) {
            Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(Some(Err(_))) => Poll::Ready(Some(Err(TransportError::BodyIncomplete))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Caller headers verbatim (HTTP/1.1 carries connection-specific fields
/// legally — nothing is stripped), plus the mechanical rewrites: synthesized
/// `content-length` and `host`, each named in the returned list.
fn render_attempt_head(attempt: &Attempt) -> (Vec<u8>, Vec<&'static str>) {
    let mut rewritten = Vec::new();
    let mut headers = attempt.headers.clone();
    if !attempt.body.is_empty() && !headers.contains_key(http::header::CONTENT_LENGTH) {
        headers.insert(
            http::header::CONTENT_LENGTH,
            HeaderValue::from(attempt.body.len()),
        );
        rewritten.push("content-length");
    }
    if !headers.contains_key(http::header::HOST)
        && let Some(auth) = attempt.uri.authority()
        && let Ok(value) = HeaderValue::from_str(auth.as_str())
    {
        headers.insert(http::header::HOST, value);
        rewritten.push("host");
    }
    let path = match attempt.uri.path_and_query() {
        Some(pq) => pq.as_str(),
        None => "/",
    };
    (render_h1_head(&attempt.method, path, &headers), rewritten)
}

/// Stale classification from the `io::ErrorKind` the exchange failed with.
/// Never inferred from display strings.
fn stale_kind_of(err: &io::Error) -> StaleKind {
    match err.kind() {
        io::ErrorKind::UnexpectedEof => StaleKind::PeerClosed,
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => StaleKind::Reset,
        _ => StaleKind::Io,
    }
}

/// See [`crate::h1::dial_h1`]: the dial tags failures by `io::ErrorKind`.
fn map_dial_error(err: io::Error) -> TransportError {
    match err.kind() {
        io::ErrorKind::AddrNotAvailable => TransportError::Dns(err),
        io::ErrorKind::InvalidData => TransportError::Tls(err),
        _ => TransportError::Connect(err),
    }
}
