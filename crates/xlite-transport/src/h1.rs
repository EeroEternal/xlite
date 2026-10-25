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
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use bytes::{Bytes, BytesMut};
use http::{HeaderMap, Method};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

use crate::configure_tcp_socket;

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

pub enum H1Stream {
    Plain(TcpStream),
    #[cfg(feature = "tls-rustls")]
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl H1Stream {
    fn tcp_stream(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
            #[cfg(feature = "tls-rustls")]
            Self::Tls(s) => s.get_ref().0,
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

impl AsyncRead for H1Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "tls-rustls")]
            Self::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for H1Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "tls-rustls")]
            Self::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            #[cfg(feature = "tls-rustls")]
            Self::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "tls-rustls")]
            Self::Tls(s) => Pin::new(s).poll_shutdown(cx),
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
}

impl HostPool {
    fn new(max_idle: usize, idle_timeout: Duration) -> Self {
        Self {
            max_idle,
            idle_timeout,
            idle: Mutex::new(VecDeque::new()),
        }
    }

    /// Checkout the warmest healthy idle connection (LIFO), reaping expired
    /// connections from the front first.
    pub fn take_idle(&self) -> Option<H1Stream> {
        loop {
            let candidate = {
                let mut guard = self.idle.lock().expect("h1 pool mutex poisoned");
                while guard
                    .front()
                    .is_some_and(|c| c.since.elapsed() >= self.idle_timeout)
                {
                    guard.pop_front();
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
            while guard
                .front()
                .is_some_and(|c| c.since.elapsed() >= self.idle_timeout)
            {
                guard.pop_front();
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
    plain_hosts: ArcSwap<HashMap<String, Arc<HostPool>>>,
    tls_hosts: ArcSwap<HashMap<String, Arc<HostPool>>>,
    mutate: Mutex<()>,
}

impl H1Pool {
    pub fn new(max_idle_per_host: usize, idle_timeout: Duration) -> Self {
        Self {
            max_idle_per_host,
            idle_timeout,
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
        let pool = Arc::new(HostPool::new(self.max_idle_per_host, self.idle_timeout));
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
        return Ok(H1Stream::Plain(tcp));
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
        Ok(H1Stream::Tls(Box::new(tls_stream)))
    }
    #[cfg(not(feature = "tls-rustls"))]
    {
        let _ = tls_host;
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tls host supplied but built without tls-rustls",
        ))
    }
}

#[derive(Debug)]
pub struct ParsedResponseHead {
    pub status: u16,
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
    } else if let H1Stream::Plain(tcp) = stream {
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

    for h in resp.headers.iter() {
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
