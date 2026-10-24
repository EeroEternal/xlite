//! Upstream HTTP client.
//!
//! - Cleartext `http://` with `XLITE_UPSTREAM_H2C=1` and `https://` endpoints
//!   whose peer negotiates `ALPN: h2` use the native `xlite-h2-pool` (peer
//!   `SETTINGS`-aware HTTP/2 stream accounting and `RST_STREAM(CANCEL)`).
//! - HTTP/1.1 (`XLITE_UPSTREAM_H2C=0`) and `https://` endpoints that negotiate
//!   `http/1.1` automatically use the native LIFO HTTP/1.1 keep-alive pool
//!   (`H1Pool`) with non-blocking socket liveness probing and transparent 1x
//!   redial on stale keep-alive races.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use arc_swap::ArcSwap;
use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use http::{HeaderName, HeaderValue, Request};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use xlite_h2_pool::{BoxIo, Config as PoolConfig, Dial, Lease, Pool, PoolError};

use crate::catalog::{HeaderPair, UpstreamShape};
use crate::config::Config;

/// Bound on establishing a new TCP or TLS connection to an upstream.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum response header block size accepted from an upstream (64 KiB).
const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;

/// Maximum chunk-size line length in `Transfer-Encoding: chunked`.
const MAX_CHUNK_LINE_BYTES: usize = 4096;

/// Headers the gateway owns. Letting a catalog field set these would let operator
/// input decide protocol correctness (`host` drives the h2 `:authority`, the
/// gateway sets `content-type`, and the credential must go through the dedicated
/// auth fields so there is exactly one source for it).
pub const RESERVED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "connection",
    "te",
    "upgrade",
    "authorization",
];

/// Precomputed `Authorization`-style credential header for one channel.
///
/// `scheme` is `bearer` | `raw` | `none`; `header` is the header name (empty
/// means `authorization`). Returns `Err` with an operator-facing reason so the
/// control plane can reject the write instead of failing silently.
pub fn build_auth(
    scheme: &str,
    header: &str,
    api_key: &str,
) -> std::result::Result<Option<(HeaderName, HeaderValue)>, String> {
    let scheme = scheme.trim().to_ascii_lowercase();
    let scheme = if scheme.is_empty() {
        "bearer".to_string()
    } else {
        scheme
    };
    if !matches!(scheme.as_str(), "bearer" | "raw" | "none") {
        return Err(format!(
            "unknown auth_scheme `{scheme}` (expected bearer, raw or none)"
        ));
    }
    let name = if header.trim().is_empty() {
        "authorization".to_string()
    } else {
        header.trim().to_ascii_lowercase()
    };
    let name = valid_header_name(&name)?;
    if api_key.is_empty() || scheme == "none" {
        return Ok(None);
    }
    let value = match scheme.as_str() {
        "bearer" => authorization_header(api_key),
        _ => api_key.to_string(),
    };
    let value = HeaderValue::from_str(&value)
        .map_err(|_| "credential is not a valid header value".to_string())?;
    Ok(Some((name, value)))
}

/// Validate an upstream shape without building it. Used by every write path
/// (Admin API, YAML import) so a bad provider is rejected with a 400 instead of
/// silently doing nothing or failing later at snapshot build.
pub fn validate_upstream(shape: &UpstreamShape, api_key: &str) -> std::result::Result<(), String> {
    build_auth(&shape.auth_scheme, &shape.auth_header, api_key)?;
    build_extra_headers(&shape.extra_headers)?;
    Ok(())
}

/// Precompute the static extra headers for one channel.
pub fn build_extra_headers(
    pairs: &[HeaderPair],
) -> std::result::Result<Vec<(HeaderName, HeaderValue)>, String> {
    let mut out = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let raw = pair.name.trim().to_ascii_lowercase();
        if raw.is_empty() {
            return Err("extra header name must not be empty".to_string());
        }
        if RESERVED_HEADERS.contains(&raw.as_str()) {
            return Err(format!(
                "header `{raw}` is reserved by the gateway and cannot be set per provider"
            ));
        }
        let name = valid_header_name(&raw)?;
        let value = HeaderValue::from_str(pair.value.trim())
            .map_err(|_| format!("extra header `{}` has an invalid value", pair.name))?;
        out.push((name, value));
    }
    Ok(out)
}

fn valid_header_name(name: &str) -> std::result::Result<HeaderName, String> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| format!("`{name}` is not a valid header name"))
}

/// Enable `TCP_NODELAY` and OS-level TCP keepalive on an upstream socket so NAT
/// gateways, cloud load balancers, and firewalls do not silently drop long-context
/// prefill or reasoning streams during prolonged application-layer silence.
///
/// The probe budget is deliberately short (`15s + 3 × 5s = 30s` to declare a peer
/// dead). `TCP_KEEPCNT` must be set explicitly: without it the kernel default of 9
/// probes made a blackholed connection take `30s + 9 × 10s = 120s` to fail, i.e. the
/// kernel signal always lost the race against `XLITE_UPSTREAM_HEADER_TIMEOUT_SECS`
/// and every later request inherited the stall.
fn configure_tcp_socket(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(5))
        .with_retries(TCP_KEEPALIVE_RETRIES);
    let _ = SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

/// Failed probes before the kernel reports the peer dead (see `configure_tcp_socket`).
const TCP_KEEPALIVE_RETRIES: u32 = 3;

async fn dial_tcp(authority: &str) -> io::Result<TcpStream> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(authority))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "upstream connect timeout after {}s",
                    CONNECT_TIMEOUT.as_secs()
                ),
            )
        })??;
    configure_tcp_socket(&stream);
    Ok(stream)
}

fn build_tls_connector(
    alpn_protocols: Vec<Vec<u8>>,
    extra_roots: &[rustls_pki_types::CertificateDer<'static>],
) -> tokio_rustls::TlsConnector {
    let mut root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    for cert in extra_roots {
        let _ = root_store.add(cert.clone());
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls default protocol versions")
        .with_root_certificates(root_store)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn_protocols;
    // Opt-in only: with the default no-op key log, rustls skips the key derivation logging entirely.
    if let Some(key_log) = crate::keylog::FileKeyLog::from_env() {
        cfg.key_log = Arc::new(key_log);
    }
    tokio_rustls::TlsConnector::from(Arc::new(cfg))
}

fn shared_tls_connector() -> &'static tokio_rustls::TlsConnector {
    static TLS: OnceLock<tokio_rustls::TlsConnector> = OnceLock::new();
    TLS.get_or_init(|| build_tls_connector(vec![b"http/1.1".to_vec()], &[]))
}

fn shared_tls_alpn_connector() -> &'static tokio_rustls::TlsConnector {
    static TLS: OnceLock<tokio_rustls::TlsConnector> = OnceLock::new();
    TLS.get_or_init(|| build_tls_connector(vec![b"h2".to_vec(), b"http/1.1".to_vec()], &[]))
}

fn tls_host_from_authority(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[')
        && let Some((ipv6, _)) = rest.split_once(']')
    {
        return ipv6;
    }
    authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority)
}

enum H1Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl H1Stream {
    fn tcp_stream(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
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
            Self::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

struct IdleH1Conn {
    stream: H1Stream,
    since: Instant,
}

struct HostPool {
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
    fn take_idle(&self) -> Option<H1Stream> {
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
    fn put(&self, stream: H1Stream) {
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

struct H1Pool {
    max_idle_per_host: usize,
    idle_timeout: Duration,
    plain_hosts: ArcSwap<HashMap<String, Arc<HostPool>>>,
    tls_hosts: ArcSwap<HashMap<String, Arc<HostPool>>>,
    mutate: Mutex<()>,
}

impl H1Pool {
    fn new(max_idle_per_host: usize, idle_timeout: Duration) -> Self {
        Self {
            max_idle_per_host,
            idle_timeout,
            plain_hosts: ArcSwap::from_pointee(HashMap::new()),
            tls_hosts: ArcSwap::from_pointee(HashMap::new()),
            mutate: Mutex::new(()),
        }
    }

    fn host_pool(&self, is_tls: bool, authority: &str) -> Arc<HostPool> {
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

async fn dial_h1(
    authority: &str,
    tls_host: Option<&str>,
    tls_connector: &tokio_rustls::TlsConnector,
) -> io::Result<H1Stream> {
    let tcp = dial_tcp(authority).await?;
    let Some(host) = tls_host else {
        return Ok(H1Stream::Plain(tcp));
    };
    let clean_host = host.trim_start_matches('[').trim_end_matches(']');
    let server_name =
        rustls_pki_types::ServerName::try_from(clean_host.to_owned()).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid TLS host `{clean_host}`: {e}"),
            )
        })?;
    let tls_stream = tokio::time::timeout(CONNECT_TIMEOUT, tls_connector.connect(server_name, tcp))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "upstream TLS handshake timeout after {}s",
                    CONNECT_TIMEOUT.as_secs()
                ),
            )
        })??;
    Ok(H1Stream::Tls(Box::new(tls_stream)))
}

#[derive(Clone)]
pub struct Upstream {
    h2c: bool,
    pool: Arc<Pool>,
    tls_pool: Arc<Pool>,
    h1: Arc<H1Pool>,
    tls_h1_connector: tokio_rustls::TlsConnector,
    /// `https://` authorities whose TLS handshake negotiated `http/1.1` (or no
    /// ALPN) rather than `h2`, so subsequent requests go straight to `H1Pool`.
    h1_tls_hosts: Arc<ArcSwap<HashSet<String>>>,
    /// Bound on the request→response-headers wait (see
    /// [`Config::upstream_header_timeout`]). `Duration::ZERO` = unbounded.
    header_timeout: Duration,
}

pub struct UpstreamBody {
    inner: BodyKind,
    _lease: Option<Lease>,
}

enum BodyKind {
    H2(h2::RecvStream, usize),
    Pump(tokio::sync::mpsc::Receiver<Result<Bytes, String>>),
    H1(H1Body),
}

pub struct UpstreamResponse {
    pub status: u16,
    pub content_type: Option<String>,
    /// Upstream `Retry-After` in seconds, when the provider sent one. Feeds the
    /// cooldown length so a rate-limited key is parked for as long as it asked.
    pub retry_after: Option<u64>,
    pub body: UpstreamBody,
}

/// One line per upstream phase, for diagnosing "the gateway hangs" without a
/// debugger. Enable with `RUST_LOG=xlite.timing=debug`; the macro is a no-op
/// (one atomic level check) when that target is off.
fn log_phase(transport: &str, host: &str, phase: &str, started: std::time::Instant) {
    tracing::debug!(
        target: "xlite.timing",
        transport,
        host,
        phase,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "upstream phase"
    );
}

impl Upstream {
    pub fn new(cfg: &Config) -> Self {
        Self::with_tls_connectors(
            cfg,
            shared_tls_alpn_connector().clone(),
            shared_tls_connector().clone(),
        )
    }

    fn with_tls_connectors(
        cfg: &Config,
        tls_alpn_connector: tokio_rustls::TlsConnector,
        tls_h1_connector: tokio_rustls::TlsConnector,
    ) -> Self {
        let dial: Dial = Arc::new(|host: String| {
            Box::pin(async move {
                let stream = dial_tcp(host.as_str()).await?;
                Ok(Box::new(stream) as BoxIo)
            })
        });
        let pool = Pool::new(PoolConfig {
            max_conns_per_host: cfg.max_conns_per_host.max(1),
            stream_budget_per_conn: cfg.upstream_stream_cap,
            pinned_poll: true,
            dial,
            idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
        });
        let h1 = Arc::new(H1Pool::new(
            cfg.max_conns_per_host.max(64),
            xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
        ));
        let h1_tls_hosts = Arc::new(ArcSwap::from_pointee(HashSet::new()));
        let h1_tls_mutate = Arc::new(Mutex::new(()));

        let h1_for_alpn = Arc::clone(&h1);
        let h1_tls_hosts_for_alpn = Arc::clone(&h1_tls_hosts);
        let h1_tls_mutate_for_alpn = Arc::clone(&h1_tls_mutate);
        let dial_tls: Dial = Arc::new(move |authority: String| {
            let tls = tls_alpn_connector.clone();
            let h1 = Arc::clone(&h1_for_alpn);
            let h1_tls_hosts = Arc::clone(&h1_tls_hosts_for_alpn);
            let h1_tls_mutate = Arc::clone(&h1_tls_mutate_for_alpn);
            Box::pin(async move {
                let tcp = dial_tcp(authority.as_str()).await?;
                let host = tls_host_from_authority(&authority);
                let server_name =
                    rustls_pki_types::ServerName::try_from(host.to_owned()).map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid TLS host `{host}`: {e}"),
                        )
                    })?;
                let tls_stream =
                    tokio::time::timeout(CONNECT_TIMEOUT, tls.connect(server_name, tcp))
                        .await
                        .map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::TimedOut,
                                format!(
                                    "upstream TLS handshake timeout after {}s",
                                    CONNECT_TIMEOUT.as_secs()
                                ),
                            )
                        })??;
                if tls_stream.get_ref().1.alpn_protocol() == Some(b"h2") {
                    Ok(Box::new(tls_stream) as BoxIo)
                } else {
                    if !h1_tls_hosts.load().contains(&authority)
                        && let Ok(_guard) = h1_tls_mutate.lock()
                    {
                        let current = h1_tls_hosts.load();
                        if !current.contains(&authority) {
                            let mut next = (**current).clone();
                            next.insert(authority.clone());
                            h1_tls_hosts.store(Arc::new(next));
                        }
                    }
                    h1.host_pool(true, &authority)
                        .put(H1Stream::Tls(Box::new(tls_stream)));
                    Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "upstream did not negotiate HTTP/2 (ALPN h2)",
                    ))
                }
            })
        });
        let tls_pool = Pool::new(PoolConfig {
            max_conns_per_host: cfg.max_conns_per_host.max(1),
            stream_budget_per_conn: cfg.upstream_stream_cap,
            pinned_poll: true,
            dial: dial_tls,
            idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
        });
        Self {
            h2c: cfg.h2c,
            pool: Arc::new(pool),
            tls_pool: Arc::new(tls_pool),
            h1,
            tls_h1_connector,
            h1_tls_hosts,
            header_timeout: cfg.upstream_header_timeout,
        }
    }

    /// Bound the "request sent → response headers received" wait by
    /// [`Config::upstream_header_timeout`]; a pass-through when it is
    /// `Duration::ZERO`.
    ///
    /// Only the headers wait is wrapped: the dial and the pool-queue wait happen
    /// before this point, so legitimate backpressure is not turned into a 502.
    async fn await_headers<T, E>(
        &self,
        fut: impl Future<Output = Result<T, E>>,
        what: &str,
    ) -> Result<T>
    where
        E: std::fmt::Display,
    {
        if self.header_timeout.is_zero() {
            return fut.await.map_err(|e| anyhow::anyhow!("{what}: {e}"));
        }
        match tokio::time::timeout(self.header_timeout, fut).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(anyhow::anyhow!("{what}: {e}")),
            Err(_) => Err(anyhow::anyhow!(
                "upstream returned no response headers within {}s",
                self.header_timeout.as_secs()
            )),
        }
    }

    /// Pool stats for the authority this channel targets, if that pool exists.
    ///
    /// Allocation-free apart from the returned copy; used on the request path to
    /// decide whether a bulk request should shed instead of queueing.
    pub fn pool_pressure(&self, authority: &str) -> Option<xlite_h2_pool::PoolStats> {
        self.pool
            .stats_for(authority)
            .or_else(|| self.tls_pool.stats_for(authority))
    }

    /// Read-only pool snapshot for the control plane (`GET /api/pool`).
    pub fn pool_stats(&self) -> Vec<(String, xlite_h2_pool::PoolStats)> {
        let mut out = self.pool.all_stats();
        let tls_stats = self.tls_pool.all_stats();
        if !tls_stats.is_empty() {
            out.extend(tls_stats);
            out.sort_by(|a, b| a.0.cmp(&b.0));
        }
        out
    }

    pub async fn post_stream(
        &self,
        endpoint: &str,
        authority: &str,
        path: &str,
        auth: Option<&(HeaderName, HeaderValue)>,
        extra: &[(HeaderName, HeaderValue)],
        body: Bytes,
    ) -> Result<UpstreamResponse> {
        if endpoint.starts_with("https://") {
            let resolved_auth;
            let auth_key = if authority.is_empty() {
                resolved_auth = h2c_parts(endpoint).map(|(a, _)| a).unwrap_or_default();
                resolved_auth.as_str()
            } else {
                authority
            };
            if !auth_key.is_empty() && !self.is_h1_only_tls(auth_key) {
                match self
                    .post_h2_tls(endpoint, auth_key, auth, extra, body.clone())
                    .await?
                {
                    Some(resp) => return Ok(resp),
                    None => {
                        // Peer negotiated HTTP/1.1 during ALPN and its warm TLS
                        // socket is already parked in `self.h1`; fall through.
                    }
                }
            }
            return self
                .post_h1(endpoint, authority, path, auth, extra, body)
                .await;
        }
        if self.h2c && endpoint.starts_with("http://") {
            return self.post_h2c(authority, path, auth, extra, body).await;
        }
        self.post_h1(endpoint, authority, path, auth, extra, body)
            .await
    }

    fn is_h1_only_tls(&self, authority: &str) -> bool {
        self.h1_tls_hosts.load().contains(authority)
    }

    async fn post_h2_tls(
        &self,
        endpoint: &str,
        authority: &str,
        auth: Option<&(HeaderName, HeaderValue)>,
        extra: &[(HeaderName, HeaderValue)],
        body: Bytes,
    ) -> Result<Option<UpstreamResponse>> {
        let timing = Instant::now();
        let mut lease = match self.tls_pool.acquire(authority).await {
            Ok(lease) => lease,
            Err(PoolError::Dial(e)) if e.kind() == io::ErrorKind::Unsupported => {
                return Ok(None);
            }
            Err(err) => return Err(err).context("h2 pool acquire"),
        };
        log_phase("h2-tls", authority, "acquired", timing);
        let build_req = || -> Result<Request<()>> {
            let mut builder = Request::builder()
                .method("POST")
                .uri(endpoint)
                .header("content-type", "application/json")
                .header("content-length", body.len());
            if let Some((name, value)) = auth {
                builder = builder.header(name.clone(), value.clone());
            }
            for (name, value) in extra {
                builder = builder.header(name.clone(), value.clone());
            }
            builder.body(()).context("build h2 request")
        };
        let request = build_req()?;
        match self
            .send_h2_request(&mut lease, request, body.clone())
            .await
        {
            Ok((status, content_type, retry_after, inner)) => {
                log_phase("h2-tls", authority, "headers", timing);
                Ok(Some(UpstreamResponse {
                    status,
                    content_type,
                    retry_after,
                    body: UpstreamBody {
                        inner,
                        _lease: Some(lease),
                    },
                }))
            }
            Err(H2SendError::Transport(_)) if lease.is_reused() => {
                // The pooled HTTP/2 connection was closed by the upstream or an
                // intermediate NAT/firewall right as we sent the request. Evict
                // the stale connection and transparently redial once.
                lease.evict();
                drop(lease);
                let mut fresh = match self.tls_pool.acquire(authority).await {
                    Ok(lease) => lease,
                    Err(PoolError::Dial(e)) if e.kind() == io::ErrorKind::Unsupported => {
                        return Ok(None);
                    }
                    Err(err) => return Err(err).context("h2 pool acquire"),
                };
                let retry_req = build_req()?;
                let (status, content_type, retry_after, inner) = self
                    .send_h2_request(&mut fresh, retry_req, body)
                    .await
                    .map_err(H2SendError::into_anyhow)?;
                Ok(Some(UpstreamResponse {
                    status,
                    content_type,
                    retry_after,
                    body: UpstreamBody {
                        inner,
                        _lease: Some(fresh),
                    },
                }))
            }
            Err(err) => Err(err.into_anyhow()),
        }
    }

    async fn post_h2c(
        &self,
        authority: &str,
        path: &str,
        auth: Option<&(HeaderName, HeaderValue)>,
        extra: &[(HeaderName, HeaderValue)],
        body: Bytes,
    ) -> Result<UpstreamResponse> {
        if authority.is_empty() {
            anyhow::bail!("upstream url missing host");
        }
        let timing = Instant::now();
        let mut lease = self
            .pool
            .acquire(authority)
            .await
            .context("h2 pool acquire")?;
        log_phase("h2c", authority, "acquired", timing);
        let build_req = || -> Result<Request<()>> {
            let mut builder = Request::builder()
                .method("POST")
                .uri(path)
                .header("host", authority)
                .header("content-type", "application/json")
                .header("content-length", body.len());
            if let Some((name, value)) = auth {
                builder = builder.header(name.clone(), value.clone());
            }
            for (name, value) in extra {
                builder = builder.header(name.clone(), value.clone());
            }
            builder.body(()).context("build h2 request")
        };
        let request = build_req()?;
        match self
            .send_h2_request(&mut lease, request, body.clone())
            .await
        {
            Ok((status, content_type, retry_after, inner)) => {
                log_phase("h2c", authority, "headers", timing);
                Ok(UpstreamResponse {
                    status,
                    content_type,
                    retry_after,
                    body: UpstreamBody {
                        inner,
                        _lease: Some(lease),
                    },
                })
            }
            Err(H2SendError::Transport(_)) if lease.is_reused() => {
                lease.evict();
                drop(lease);
                let mut fresh = self
                    .pool
                    .acquire(authority)
                    .await
                    .context("h2 pool acquire")?;
                let retry_req = build_req()?;
                let (status, content_type, retry_after, inner) = self
                    .send_h2_request(&mut fresh, retry_req, body)
                    .await
                    .map_err(H2SendError::into_anyhow)?;
                Ok(UpstreamResponse {
                    status,
                    content_type,
                    retry_after,
                    body: UpstreamBody {
                        inner,
                        _lease: Some(fresh),
                    },
                })
            }
            Err(err) => Err(err.into_anyhow()),
        }
    }

    async fn send_h2_request(
        &self,
        lease: &mut Lease,
        request: Request<()>,
        body: Bytes,
    ) -> std::result::Result<(u16, Option<String>, Option<u64>, BodyKind), H2SendError> {
        let eos = body.is_empty();
        let (response_future, mut send_stream) = lease
            .send_request(request, eos)
            .map_err(|e| H2SendError::Transport(anyhow::anyhow!("h2 send_request: {e}")))?;
        if !eos {
            send_stream
                .send_data(body, true)
                .map_err(|e| H2SendError::Transport(anyhow::anyhow!("h2 send_data: {e}")))?;
        }
        let response = if self.header_timeout.is_zero() {
            response_future
                .await
                .map_err(|e| H2SendError::Transport(anyhow::anyhow!("h2 response: {e}")))?
        } else {
            match tokio::time::timeout(self.header_timeout, response_future).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    return Err(H2SendError::Transport(anyhow::anyhow!("h2 response: {e}")));
                }
                Err(_) => {
                    // The connection just proved it cannot deliver headers. Keep it out of
                    // the pool so the next acquire dials a fresh one instead of inheriting
                    // the stall; in-flight streams on it keep their own lease and finish.
                    lease.evict();
                    tracing::debug!(
                        target: "xlite.timing",
                        timeout_secs = self.header_timeout.as_secs(),
                        "evicted upstream connection after header timeout"
                    );
                    return Err(H2SendError::HeaderTimeout(anyhow::anyhow!(
                        "upstream returned no response headers within {}s",
                        self.header_timeout.as_secs()
                    )));
                }
            }
        };
        drop(send_stream);
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let retry_after = response
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let recv = response.into_body();
        let inner = if lease.pinned_poll() {
            BodyKind::Pump(lease.register(recv).await)
        } else {
            BodyKind::H2(recv, 0)
        };
        Ok((status, content_type, retry_after, inner))
    }

    async fn post_h1(
        &self,
        endpoint: &str,
        authority: &str,
        path: &str,
        auth: Option<&(HeaderName, HeaderValue)>,
        extra: &[(HeaderName, HeaderValue)],
        body: Bytes,
    ) -> Result<UpstreamResponse> {
        let target = resolve_h1_target(endpoint, authority, path)?;
        let host_pool = self.h1.host_pool(target.is_tls, &target.authority);
        let req_head =
            build_h1_request_head(&target.path, &target.authority, auth, extra, body.len());

        // Acquire a warm idle connection if available; otherwise dial before
        // entering `await_headers` so connect time is bounded by `CONNECT_TIMEOUT`
        // while `await_headers` bounds the request->response-headers wait.
        let timing = Instant::now();
        let (mut stream, reused) = match host_pool.take_idle() {
            Some(s) => (s, true),
            None => {
                let s = dial_h1(
                    &target.authority,
                    target.tls_host.as_deref(),
                    &self.tls_h1_connector,
                )
                .await
                .context("upstream connect")?;
                (s, false)
            }
        };

        let (stream, head, buf) = self
            .await_headers(
                async {
                    let mut buf = BytesMut::with_capacity(4096);
                    match send_and_read_h1_head(&mut stream, &req_head, &body, &mut buf).await {
                        Ok(head) => Ok::<_, io::Error>((stream, head, buf)),
                        Err(err) if reused && buf.is_empty() => {
                            // The idle keep-alive socket was closed by the upstream
                            // right as we wrote the request or waited for the first
                            // response byte. Transparently redial a fresh socket once.
                            let mut fresh = dial_h1(
                                &target.authority,
                                target.tls_host.as_deref(),
                                &self.tls_h1_connector,
                            )
                            .await?;
                            buf.clear();
                            let head =
                                send_and_read_h1_head(&mut fresh, &req_head, &body, &mut buf)
                                    .await?;
                            Ok((fresh, head, buf))
                        }
                        Err(err) => Err(err),
                    }
                },
                "upstream request",
            )
            .await?;

        log_phase("h1", authority, "headers", timing);
        let can_reuse = !head.connection_close && !matches!(head.framing, BodyFraming::UntilClose);
        let mut h1_body = H1Body {
            stream: Some(stream),
            buf,
            framing: head.framing,
            can_reuse,
            host_pool,
        };
        h1_body.try_eager_recycle();

        Ok(UpstreamResponse {
            status: head.status,
            content_type: head.content_type,
            retry_after: head.retry_after,
            body: UpstreamBody {
                inner: BodyKind::H1(h1_body),
                _lease: None,
            },
        })
    }

    /// Fire-and-forget JSON `POST` over the HTTP/1.1 keep-alive pool, draining
    /// the response body so the underlying socket is recycled into `H1Pool`.
    pub async fn post_json_h1(
        &self,
        endpoint: &str,
        auth: Option<&(HeaderName, HeaderValue)>,
        body: Bytes,
    ) -> Result<u16> {
        use futures_util::StreamExt;
        let mut resp = self.post_h1(endpoint, "", "", auth, &[], body).await?;
        while let Some(chunk) = resp.body.next().await {
            if chunk.is_err() {
                break;
            }
        }
        Ok(resp.status)
    }
}

struct H1Target {
    is_tls: bool,
    authority: String,
    path: String,
    tls_host: Option<String>,
}

fn resolve_h1_target(endpoint: &str, authority: &str, path: &str) -> Result<H1Target> {
    if endpoint.starts_with("http://") && !authority.is_empty() && !path.is_empty() {
        return Ok(H1Target {
            is_tls: false,
            authority: authority.to_string(),
            path: path.to_string(),
            tls_host: None,
        });
    }
    let parsed = url::Url::parse(endpoint).context("invalid upstream url")?;
    let is_tls = match parsed.scheme() {
        "http" => false,
        "https" => true,
        other => anyhow::bail!("unsupported upstream scheme `{other}`"),
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("upstream url missing host"))?;
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if is_tls { 443 } else { 80 });
    let resolved_authority = if authority.is_empty() {
        format!("{host}:{port}")
    } else {
        authority.to_string()
    };
    let resolved_path = if !path.is_empty() {
        path.to_string()
    } else {
        match parsed.query() {
            Some(q) => format!("{}?{q}", parsed.path()),
            None => parsed.path().to_string(),
        }
    };
    Ok(H1Target {
        is_tls,
        authority: resolved_authority,
        path: resolved_path,
        tls_host: is_tls.then(|| host.to_string()),
    })
}

fn build_h1_request_head(
    path: &str,
    authority: &str,
    auth: Option<&(HeaderName, HeaderValue)>,
    extra: &[(HeaderName, HeaderValue)],
    content_length: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + extra.len() * 48);
    out.extend_from_slice(b"POST ");
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    out.extend_from_slice(authority.as_bytes());
    out.extend_from_slice(b"\r\nContent-Type: application/json\r\nContent-Length: ");
    let mut len_buf = itoa::Buffer::new();
    out.extend_from_slice(len_buf.format(content_length).as_bytes());
    out.extend_from_slice(b"\r\nConnection: keep-alive\r\n");
    if let Some((name, value)) = auth {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    for (name, value) in extra {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

#[derive(Debug)]
struct ParsedResponseHead {
    status: u16,
    content_type: Option<String>,
    retry_after: Option<u64>,
    framing: BodyFraming,
    connection_close: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFraming {
    Empty,
    ContentLength(u64),
    Chunked(ChunkState),
    UntilClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    Size,
    Data(u64),
    DataCrlf,
    Trailers,
}

enum H2SendError {
    Transport(anyhow::Error),
    HeaderTimeout(anyhow::Error),
}

impl H2SendError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Transport(e) | Self::HeaderTimeout(e) => e,
        }
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

async fn send_and_read_h1_head(
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

struct H1Body {
    stream: Option<H1Stream>,
    buf: BytesMut,
    framing: BodyFraming,
    can_reuse: bool,
    host_pool: Arc<HostPool>,
}

impl H1Body {
    /// If the response body is already complete (e.g. `Content-Length: 0` or
    /// `204 No Content`), return the socket to the idle pool immediately.
    fn try_eager_recycle(&mut self) {
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
        if self.framing == BodyFraming::Chunked(ChunkState::DataCrlf)
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

    fn poll_next_inner(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Result<Bytes>>> {
        loop {
            match self.framing {
                BodyFraming::Empty => {
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

impl Stream for UpstreamBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match &mut this.inner {
            BodyKind::H2(recv, released) => match recv.poll_data(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    *released += bytes.len();
                    if *released >= 8192 {
                        let _ = recv.flow_control().release_capacity(*released);
                        *released = 0;
                    }
                    Poll::Ready(Some(Ok(bytes)))
                }
                Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(std::io::Error::other(err)))),
                Poll::Ready(None) => {
                    if *released > 0 {
                        let _ = recv.flow_control().release_capacity(*released);
                        *released = 0;
                    }
                    Poll::Ready(None)
                }
                Poll::Pending => Poll::Pending,
            },
            BodyKind::Pump(rx) => match rx.poll_recv(cx) {
                Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(bytes))),
                Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(std::io::Error::other(err)))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
            BodyKind::H1(h1) => h1.poll_next_inner(cx),
        }
    }
}

pub fn chat_completions_url(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.ends_with("/v1") {
        format!("{b}/chat/completions")
    } else {
        format!("{b}/v1/chat/completions")
    }
}

pub fn responses_url(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.ends_with("/v1") {
        format!("{b}/responses")
    } else {
        format!("{b}/v1/responses")
    }
}

pub fn embeddings_url(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.ends_with("/v1") {
        format!("{b}/embeddings")
    } else {
        format!("{b}/v1/embeddings")
    }
}

/// `base_url` + an explicit provider path (the `chat_path` override).
pub fn upstream_url(base: &str, path: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if path.starts_with('/') {
        format!("{b}{path}")
    } else {
        format!("{b}/{path}")
    }
}

/// `(authority, path[+query])` for an already-built chat/completions URL.
pub fn h2c_parts(endpoint: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(endpoint).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default().unwrap_or(80);
    let path = match parsed.query() {
        Some(q) => format!("{}?{q}", parsed.path()),
        None => parsed.path().to_string(),
    };
    Some((format!("{host}:{port}"), path))
}

pub fn authorization_header(api_key: &str) -> String {
    if api_key.is_empty() {
        String::new()
    } else {
        format!("Bearer {api_key}")
    }
}

/// Fail over only before the first byte has been written to the client.
///
/// Covers server errors (`5xx`), rate limits (`429 Too Many Requests`),
/// request timeouts (`408 Request Timeout`), quota/balance exhaustion
/// (`402 Payment Required`), credential rejection on a single provider key
/// (`401 Unauthorized`, `403 Forbidden`), and missing upstream model routes
/// (`404 Not Found`) so requests transparently fall back to the next candidate
/// channel. Client payload validation errors (`400`, `422`) are not retried.
pub fn should_failover(status: u16, bytes_sent: bool) -> bool {
    !bytes_sent && (status >= 500 || matches!(status, 401 | 402 | 403 | 404 | 408 | 429))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[test]
    fn completions_path_appends_v1_when_missing() {
        assert_eq!(
            chat_completions_url("http://127.0.0.1:8000"),
            "http://127.0.0.1:8000/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("http://127.0.0.1:8000/v1/"),
            "http://127.0.0.1:8000/v1/chat/completions"
        );
    }

    #[test]
    fn pool_stats_start_empty_for_a_fresh_upstream() {
        let cfg = Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: true,
            admin_password: "x".into(),
            session_secret: None,
            max_conns_per_host: 3,
            upstream_stream_cap: 250,
            catalog_reload_secs: 0,
            upstream_stall_timeout: Duration::ZERO,
            upstream_header_timeout: Duration::ZERO,
            sse_keepalive_interval: Duration::ZERO,
            max_body_bytes: crate::config::DEFAULT_MAX_BODY_BYTES,
        };
        let up = Upstream::new(&cfg);
        assert!(up.pool_stats().is_empty());
    }

    #[test]
    fn h2c_parts_splits_host_port_and_path() {
        let (authority, path) = h2c_parts("http://127.0.0.1:4101/v1/chat/completions").unwrap();
        assert_eq!(authority, "127.0.0.1:4101");
        assert_eq!(path, "/v1/chat/completions");
    }

    #[test]
    fn authorization_header_empty_when_no_key() {
        assert_eq!(authorization_header(""), "");
        assert_eq!(authorization_header("sk-test"), "Bearer sk-test");
    }

    #[test]
    fn failover_only_before_first_byte_on_5xx_429_408_and_401_to_404() {
        assert!(should_failover(502, false));
        assert!(should_failover(429, false));
        assert!(should_failover(408, false));
        assert!(should_failover(404, false));
        assert!(should_failover(403, false));
        assert!(should_failover(402, false));
        assert!(should_failover(401, false));
        assert!(!should_failover(400, false));
        assert!(!should_failover(422, false));
        assert!(!should_failover(502, true));
        assert!(!should_failover(402, true));
        assert!(!should_failover(401, true));
        assert!(!should_failover(200, false));
    }

    fn pair(name: &str, value: &str) -> HeaderPair {
        HeaderPair {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn default_auth_is_byte_identical_to_the_old_bearer_header() {
        let (name, value) = build_auth("bearer", "", "sk-test")
            .expect("default auth")
            .expect("credential present");
        assert_eq!(name.as_str(), "authorization");
        assert_eq!(value.to_str().unwrap(), authorization_header("sk-test"));
        assert_eq!(value.to_str().unwrap(), "Bearer sk-test");

        let (_, value) = build_auth("", "", "sk-test").unwrap().unwrap();
        assert_eq!(value.to_str().unwrap(), "Bearer sk-test");
    }

    #[test]
    fn auth_scheme_none_and_empty_key_send_no_credential() {
        assert!(build_auth("none", "", "sk-test").unwrap().is_none());
        assert!(build_auth("bearer", "", "").unwrap().is_none());
        assert!(build_auth("none", "bad header", "").is_err());
    }

    #[test]
    fn raw_auth_sends_the_key_verbatim() {
        let (name, value) = build_auth("raw", "X-Api-Key", "sk-test").unwrap().unwrap();
        assert_eq!(name.as_str(), "x-api-key");
        assert_eq!(value.to_str().unwrap(), "sk-test");
    }

    #[test]
    fn unknown_auth_scheme_is_rejected() {
        let err = build_auth("oauth", "", "sk-test").unwrap_err();
        assert!(err.contains("unknown auth_scheme"), "{err}");
    }

    #[test]
    fn reserved_extra_headers_are_rejected() {
        for reserved in RESERVED_HEADERS {
            let err = build_extra_headers(&[pair(reserved, "x")]).unwrap_err();
            assert!(err.contains("reserved"), "{reserved}: {err}");
        }
    }

    #[test]
    fn extra_headers_are_parsed_once_and_lowercased() {
        let parsed = build_extra_headers(&[
            pair("X-Title", "xlite"),
            pair("HTTP-Referer", "https://e.com"),
        ])
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0.as_str(), "x-title");
        assert_eq!(parsed[0].1.to_str().unwrap(), "xlite");
        assert_eq!(parsed[1].0.as_str(), "http-referer");
    }

    #[test]
    fn extra_header_with_a_bad_value_is_rejected() {
        assert!(build_extra_headers(&[pair("X-Title", "line\nbreak")]).is_err());
        assert!(build_extra_headers(&[pair("", "v")]).is_err());
        assert!(build_extra_headers(&[pair("bad header", "v")]).is_err());
    }

    #[test]
    fn validate_upstream_accepts_the_default_shape() {
        assert!(validate_upstream(&UpstreamShape::default(), "sk-test").is_ok());
        assert!(validate_upstream(&UpstreamShape::default(), "").is_ok());
    }

    #[test]
    fn upstream_url_joins_with_exactly_one_slash() {
        assert_eq!(
            upstream_url("http://h:1/", "/api/v1/chat"),
            "http://h:1/api/v1/chat"
        );
        assert_eq!(
            upstream_url("http://h:1", "api/v1/chat"),
            "http://h:1/api/v1/chat"
        );
    }

    #[tokio::test]
    async fn header_timeout_evicts_the_connection_so_the_next_request_redials() {
        // h2c upstream that accepts every stream and never answers: the request can only
        // end by header timeout.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let accepts_bg = Arc::clone(&accepts);
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                accepts_bg.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let Ok(mut conn) = h2::server::handshake(socket).await else {
                        return;
                    };
                    let mut parked = Vec::new();
                    while let Some(Ok((_req, respond))) = conn.accept().await {
                        parked.push(respond);
                    }
                });
            }
        });

        let cfg = Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: true,
            admin_password: "x".into(),
            session_secret: None,
            max_conns_per_host: 3,
            upstream_stream_cap: 250,
            catalog_reload_secs: 0,
            upstream_stall_timeout: Duration::ZERO,
            upstream_header_timeout: Duration::from_secs(1),
            sse_keepalive_interval: Duration::ZERO,
            max_body_bytes: crate::config::DEFAULT_MAX_BODY_BYTES,
        };
        let up = Upstream::new(&cfg);
        let authority = format!("127.0.0.1:{}", addr.port());
        let endpoint = format!("http://{authority}/v1/chat/completions");
        let body = Bytes::from_static(b"{\"model\":\"m\"}");

        let err = up
            .post_stream(
                &endpoint,
                &authority,
                "/v1/chat/completions",
                None,
                &[],
                body.clone(),
            )
            .await
            .err()
            .expect("silent upstream must end in a header timeout");
        assert!(
            err.to_string()
                .contains("upstream returned no response headers within 1s"),
            "{err}"
        );
        let conns = up
            .pool_stats()
            .into_iter()
            .find(|(host, _)| host == &authority)
            .map(|(_, stats)| stats.connections)
            .unwrap_or(0);
        assert_eq!(conns, 0, "timed-out connection must leave the pool");

        // The point of the eviction: the next attempt dials instead of reusing the corpse.
        let _ = up
            .post_stream(
                &endpoint,
                &authority,
                "/v1/chat/completions",
                None,
                &[],
                body,
            )
            .await
            .err()
            .expect("second attempt times out too");
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            2,
            "second request must dial again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_headers_times_out_pending_future_and_passes_ready_through() {
        let mut cfg = Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: true,
            admin_password: "x".into(),
            session_secret: None,
            max_conns_per_host: 3,
            upstream_stream_cap: 250,
            catalog_reload_secs: 0,
            upstream_stall_timeout: Duration::ZERO,
            upstream_header_timeout: Duration::from_secs(5),
            sse_keepalive_interval: Duration::ZERO,
            max_body_bytes: crate::config::DEFAULT_MAX_BODY_BYTES,
        };
        let up = Upstream::new(&cfg);

        let ok = up
            .await_headers(std::future::ready(Ok::<u32, &str>(42)), "h2 response")
            .await
            .unwrap();
        assert_eq!(ok, 42);
        let err = up
            .await_headers(std::future::ready(Err::<u32, &str>("boom")), "h2 response")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("h2 response: boom"), "{err}");

        let t0 = tokio::time::Instant::now();
        let timed_out = up
            .await_headers(std::future::pending::<Result<u32, &str>>(), "h2 response")
            .await
            .unwrap_err();
        assert_eq!(t0.elapsed(), Duration::from_secs(5));
        assert!(
            timed_out
                .to_string()
                .contains("upstream returned no response headers within 5s"),
            "{timed_out}"
        );

        cfg.upstream_header_timeout = Duration::ZERO;
        let up_disabled = Upstream::new(&cfg);
        assert!(
            tokio::time::timeout(
                Duration::from_secs(3600),
                up_disabled
                    .await_headers(std::future::pending::<Result<u32, &str>>(), "h2 response"),
            )
            .await
            .is_err(),
            "disabled header_timeout must keep waiting"
        );
    }

    fn h1_test_cfg() -> Config {
        Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: false,
            admin_password: "x".into(),
            session_secret: None,
            max_conns_per_host: 4,
            upstream_stream_cap: 250,
            catalog_reload_secs: 0,
            upstream_stall_timeout: Duration::ZERO,
            upstream_header_timeout: Duration::ZERO,
            sse_keepalive_interval: Duration::ZERO,
            max_body_bytes: crate::config::DEFAULT_MAX_BODY_BYTES,
        }
    }

    async fn read_http_request<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<()> {
        let mut pending = Vec::new();
        let mut buf = [0u8; 4096];
        while !pending.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
            }
            pending.extend_from_slice(&buf[..n]);
        }
        let head_end = pending.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&pending[..head_end]).to_ascii_lowercase();
        let cl = head
            .split("\r\n")
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let body_have = pending.len() - (head_end + 4);
        let mut rem = cl.saturating_sub(body_have);
        while rem > 0 {
            let cap = rem.min(buf.len());
            let n = stream.read(&mut buf[..cap]).await?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof body"));
            }
            rem -= n;
        }
        Ok(())
    }

    #[tokio::test]
    async fn h1_pool_reuses_keepalive_and_transparently_redials_on_stale_close_race() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let accepts_bg = Arc::clone(&accepts);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let conn_idx = accepts_bg.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    // Connection 0 serves the first request, then waits until the
                    // client starts sending the second request on the warm socket
                    // and immediately closes without replying (simulating the
                    // keep-alive idle timeout race).
                    if conn_idx == 0 {
                        if read_http_request(&mut socket).await.is_err() {
                            return;
                        }
                        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
                        let _ = socket.write_all(resp).await;
                        let _ = socket.flush().await;

                        // Wait for first byte of request 2 before closing to trigger
                        // the exact post-checkout race condition.
                        let mut one = [0u8; 1];
                        let _ = socket.read(&mut one).await;
                        drop(socket);
                        return;
                    }

                    // Connection 1+ serves multiple keep-alive requests normally,
                    // using chunked transfer-encoding for SSE streaming.
                    while read_http_request(&mut socket).await.is_ok() {
                        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n12\r\ndata: {\"chunk\":1}\n\r\ne\r\ndata: [DONE]\n\n\r\n0\r\n\r\n";
                        if socket.write_all(resp).await.is_err() {
                            break;
                        }
                        let _ = socket.flush().await;
                    }
                });
            }
        });

        let up = Upstream::new(&h1_test_cfg());
        let authority = format!("127.0.0.1:{}", addr.port());
        let endpoint = format!("http://{authority}/v1/chat/completions");

        // Request 1: dials connection 0 and returns it to the LIFO pool.
        let r1 = up
            .post_stream(
                &endpoint,
                &authority,
                "/v1/chat/completions",
                None,
                &[],
                Bytes::from_static(b"{\"model\":\"m\"}"),
            )
            .await
            .expect("req 1 succeeds");
        assert_eq!(r1.status, 200);
        let mut b1 = Vec::new();
        let mut s1 = r1.body;
        while let Some(c) = s1.next().await {
            b1.extend_from_slice(&c.expect("b1 chunk"));
        }
        assert_eq!(&b1[..], b"{\"ok\":true}");
        assert_eq!(accepts.load(Ordering::SeqCst), 1);

        // Request 2: checks out connection 0 (still alive at checkout time), hits
        // EOF after sending the POST, transparently redials connection 1, and succeeds!
        let r2 = up
            .post_stream(
                &endpoint,
                &authority,
                "/v1/chat/completions",
                None,
                &[],
                Bytes::from_static(b"{\"model\":\"m\"}"),
            )
            .await
            .expect("req 2 transparently redials stale socket");
        assert_eq!(r2.status, 200);
        assert_eq!(r2.content_type.as_deref(), Some("text/event-stream"));
        let mut b2 = Vec::new();
        let mut s2 = r2.body;
        while let Some(c) = s2.next().await {
            b2.extend_from_slice(&c.expect("b2 chunk"));
        }
        assert_eq!(&b2[..], b"data: {\"chunk\":1}\ndata: [DONE]\n\n");
        assert_eq!(accepts.load(Ordering::SeqCst), 2);

        // Request 3: reuses connection 1 from the LIFO pool without dialing a 3rd socket.
        let r3 = up
            .post_stream(
                &endpoint,
                &authority,
                "/v1/chat/completions",
                None,
                &[],
                Bytes::from_static(b"{\"model\":\"m\"}"),
            )
            .await
            .expect("req 3 reuses connection 1");
        assert_eq!(r3.status, 200);
        let mut b3 = Vec::new();
        let mut s3 = r3.body;
        while let Some(c) = s3.next().await {
            b3.extend_from_slice(&c.expect("b3 chunk"));
        }
        assert_eq!(&b3[..], b"data: {\"chunk\":1}\ndata: [DONE]\n\n");
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            2,
            "connection 1 must be reused for request 3"
        );
    }

    #[tokio::test]
    async fn h1_handles_100_continue_and_connection_close_eof_framing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let accepts_bg = Arc::clone(&accepts);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                accepts_bg.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    if read_http_request(&mut socket).await.is_err() {
                        return;
                    }
                    // Send a 100 Continue block followed by a 200 OK with no
                    // Content-Length or Transfer-Encoding (EOF-delimited).
                    let resp = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"eof\":true}";
                    let _ = socket.write_all(resp).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let up = Upstream::new(&h1_test_cfg());
        let authority = format!("127.0.0.1:{}", addr.port());
        let endpoint = format!("http://{authority}/v1/chat/completions");

        for expected_accepts in 1..=2 {
            let resp = up
                .post_stream(
                    &endpoint,
                    &authority,
                    "/v1/chat/completions",
                    None,
                    &[],
                    Bytes::from_static(b"{\"model\":\"m\"}"),
                )
                .await
                .expect("request succeeds");
            assert_eq!(resp.status, 200);
            let mut body = Vec::new();
            let mut stream = resp.body;
            while let Some(c) = stream.next().await {
                body.extend_from_slice(&c.expect("chunk"));
            }
            assert_eq!(&body[..], b"{\"eof\":true}");
            assert_eq!(
                accepts.load(Ordering::SeqCst),
                expected_accepts,
                "Connection: close / UntilClose socket must not be reused"
            );
        }
    }

    const TEST_CERT_DER: &[u8] = &[
        48, 130, 1, 149, 48, 130, 1, 59, 160, 3, 2, 1, 2, 2, 20, 25, 12, 191, 172, 208, 75, 168,
        104, 18, 72, 233, 180, 175, 224, 50, 53, 4, 239, 127, 163, 48, 10, 6, 8, 42, 134, 72, 206,
        61, 4, 3, 2, 48, 20, 49, 18, 48, 16, 6, 3, 85, 4, 3, 12, 9, 108, 111, 99, 97, 108, 104,
        111, 115, 116, 48, 32, 23, 13, 50, 54, 49, 48, 49, 55, 48, 57, 48, 50, 52, 49, 90, 24, 15,
        50, 49, 50, 54, 48, 57, 50, 51, 48, 57, 48, 50, 52, 49, 90, 48, 20, 49, 18, 48, 16, 6, 3,
        85, 4, 3, 12, 9, 108, 111, 99, 97, 108, 104, 111, 115, 116, 48, 89, 48, 19, 6, 7, 42, 134,
        72, 206, 61, 2, 1, 6, 8, 42, 134, 72, 206, 61, 3, 1, 7, 3, 66, 0, 4, 183, 232, 130, 207,
        171, 112, 7, 29, 251, 205, 242, 176, 43, 146, 32, 93, 212, 117, 134, 255, 170, 176, 81,
        114, 157, 179, 153, 10, 236, 84, 15, 195, 130, 112, 151, 146, 188, 36, 240, 97, 224, 192,
        150, 240, 90, 229, 141, 66, 107, 26, 221, 230, 195, 184, 60, 128, 248, 144, 158, 55, 126,
        17, 240, 40, 163, 105, 48, 103, 48, 29, 6, 3, 85, 29, 14, 4, 22, 4, 20, 241, 51, 241, 146,
        229, 131, 30, 114, 192, 131, 200, 237, 181, 128, 181, 129, 168, 103, 245, 142, 48, 31, 6,
        3, 85, 29, 35, 4, 24, 48, 22, 128, 20, 241, 51, 241, 146, 229, 131, 30, 114, 192, 131, 200,
        237, 181, 128, 181, 129, 168, 103, 245, 142, 48, 9, 6, 3, 85, 29, 19, 4, 2, 48, 0, 48, 26,
        6, 3, 85, 29, 17, 4, 19, 48, 17, 130, 9, 108, 111, 99, 97, 108, 104, 111, 115, 116, 135, 4,
        127, 0, 0, 1, 48, 10, 6, 8, 42, 134, 72, 206, 61, 4, 3, 2, 3, 72, 0, 48, 69, 2, 33, 0, 166,
        73, 217, 91, 116, 205, 164, 132, 103, 44, 20, 61, 220, 80, 223, 68, 184, 135, 147, 158, 83,
        57, 238, 148, 147, 231, 124, 39, 202, 63, 218, 254, 2, 32, 57, 13, 205, 93, 116, 171, 53,
        104, 33, 3, 195, 106, 211, 149, 132, 230, 164, 216, 139, 159, 244, 48, 96, 101, 68, 246,
        46, 4, 240, 145, 65, 31,
    ];

    const TEST_KEY_DER: &[u8] = &[
        48, 129, 135, 2, 1, 0, 48, 19, 6, 7, 42, 134, 72, 206, 61, 2, 1, 6, 8, 42, 134, 72, 206,
        61, 3, 1, 7, 4, 109, 48, 107, 2, 1, 1, 4, 32, 39, 48, 169, 252, 153, 124, 0, 56, 94, 66,
        150, 237, 57, 40, 33, 133, 196, 184, 208, 89, 164, 234, 120, 252, 101, 139, 237, 17, 233,
        103, 242, 186, 161, 68, 3, 66, 0, 4, 183, 232, 130, 207, 171, 112, 7, 29, 251, 205, 242,
        176, 43, 146, 32, 93, 212, 117, 134, 255, 170, 176, 81, 114, 157, 179, 153, 10, 236, 84,
        15, 195, 130, 112, 151, 146, 188, 36, 240, 97, 224, 192, 150, 240, 90, 229, 141, 66, 107,
        26, 221, 230, 195, 184, 60, 128, 248, 144, 158, 55, 126, 17, 240, 40,
    ];

    fn test_tls_acceptor(alpn: Vec<Vec<u8>>) -> tokio_rustls::TlsAcceptor {
        let cert = rustls_pki_types::CertificateDer::from(TEST_CERT_DER);
        let key = rustls_pki_types::PrivateKeyDer::Pkcs8(
            rustls_pki_types::PrivatePkcs8KeyDer::from(TEST_KEY_DER),
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut server_cfg = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        server_cfg.alpn_protocols = alpn;
        tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg))
    }

    fn test_tls_upstream() -> Upstream {
        let cert = rustls_pki_types::CertificateDer::from(TEST_CERT_DER);
        let alpn = build_tls_connector(
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            std::slice::from_ref(&cert),
        );
        let h1 = build_tls_connector(vec![b"http/1.1".to_vec()], std::slice::from_ref(&cert));
        Upstream::with_tls_connectors(&h1_test_cfg(), alpn, h1)
    }

    #[tokio::test]
    async fn https_negotiates_alpn_h2_and_multiplexes_via_tls_pool() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = test_tls_acceptor(vec![b"h2".to_vec()]);
        let accepts = Arc::new(AtomicUsize::new(0));
        let accepts_bg = Arc::clone(&accepts);

        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                accepts_bg.fetch_add(1, Ordering::SeqCst);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let Ok(mut h2_conn) = h2::server::handshake(tls).await else {
                        return;
                    };
                    while let Some(Ok((req, mut respond))) = h2_conn.accept().await {
                        assert_eq!(req.uri().scheme_str(), Some("https"));
                        assert_eq!(req.uri().path(), "/v1/chat/completions");
                        assert_eq!(
                            req.headers()
                                .get("content-length")
                                .and_then(|v| v.to_str().ok()),
                            Some("13")
                        );
                        tokio::spawn(async move {
                            let resp = http::Response::builder()
                                .status(200)
                                .header("content-type", "text/event-stream")
                                .body(())
                                .unwrap();
                            let mut send = respond.send_response(resp, false).unwrap();
                            send.send_data(Bytes::from_static(b"data: {\"h2\":true}\n\n"), true)
                                .unwrap();
                        });
                    }
                });
            }
        });

        let up = test_tls_upstream();
        let authority = format!("127.0.0.1:{}", addr.port());
        let endpoint = format!("https://{authority}/v1/chat/completions");

        for _ in 0..3 {
            let resp = up
                .post_stream(
                    &endpoint,
                    &authority,
                    "/v1/chat/completions",
                    None,
                    &[],
                    Bytes::from_static(b"{\"model\":\"m\"}"),
                )
                .await
                .expect("TLS h2 request succeeds");
            assert_eq!(resp.status, 200);
            assert_eq!(resp.content_type.as_deref(), Some("text/event-stream"));
            let mut body = Vec::new();
            let mut stream = resp.body;
            while let Some(c) = stream.next().await {
                body.extend_from_slice(&c.expect("chunk"));
            }
            assert_eq!(&body[..], b"data: {\"h2\":true}\n\n");
        }

        assert_eq!(
            accepts.load(Ordering::SeqCst),
            1,
            "all 3 HTTPS requests must multiplex over a single TLS HTTP/2 connection"
        );
        let stats = up.pool_stats();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].0, authority);
        assert_eq!(stats[0].1.connections, 1);
    }

    #[tokio::test]
    async fn https_falls_back_to_h1_pool_with_zero_extra_handshake_when_peer_speaks_http1() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = test_tls_acceptor(vec![b"http/1.1".to_vec()]);
        let accepts = Arc::new(AtomicUsize::new(0));
        let accepts_bg = Arc::clone(&accepts);

        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                accepts_bg.fetch_add(1, Ordering::SeqCst);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    while read_http_request(&mut tls).await.is_ok() {
                        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"h1\":true}";
                        if tls.write_all(resp).await.is_err() {
                            break;
                        }
                        let _ = tls.flush().await;
                    }
                });
            }
        });

        let up = test_tls_upstream();
        let authority = format!("127.0.0.1:{}", addr.port());
        let endpoint = format!("https://{authority}/v1/chat/completions");

        for _ in 0..2 {
            let resp = up
                .post_stream(
                    &endpoint,
                    &authority,
                    "/v1/chat/completions",
                    None,
                    &[],
                    Bytes::from_static(b"{\"model\":\"m\"}"),
                )
                .await
                .expect("TLS h1 fallback request succeeds");
            assert_eq!(resp.status, 200);
            let mut body = Vec::new();
            let mut stream = resp.body;
            while let Some(c) = stream.next().await {
                body.extend_from_slice(&c.expect("chunk"));
            }
            assert_eq!(&body[..], b"{\"h1\":true}");
        }

        assert_eq!(
            accepts.load(Ordering::SeqCst),
            1,
            "ALPN fallback must park the first TLS socket into H1Pool and reuse it with 0 extra handshakes"
        );
    }

    #[tokio::test]
    async fn h2_transparently_redials_once_when_reused_connection_drops_before_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = test_tls_acceptor(vec![b"h2".to_vec()]);
        let accepts = Arc::new(AtomicUsize::new(0));
        let accepts_bg = Arc::clone(&accepts);

        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                let conn_no = accepts_bg.fetch_add(1, Ordering::SeqCst) + 1;
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let Ok(mut h2_conn) = h2::server::handshake(tls).await else {
                        return;
                    };
                    let mut stream_no = 0usize;
                    while let Some(Ok((_req, mut respond))) = h2_conn.accept().await {
                        stream_no += 1;
                        if conn_no == 1 && stream_no == 2 {
                            // Simulate a silent NAT/GOAWAY drop right as the 2nd
                            // request arrives on the warm connection: drop the
                            // server connection before sending response headers.
                            drop(respond);
                            drop(h2_conn);
                            return;
                        }
                        let resp = http::Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(())
                            .unwrap();
                        let mut send = respond.send_response(resp, false).unwrap();
                        send.send_data(Bytes::from(format!("{{\"conn\":{conn_no}}}")), true)
                            .unwrap();
                    }
                });
            }
        });

        let up = test_tls_upstream();
        let authority = format!("127.0.0.1:{}", addr.port());
        let endpoint = format!("https://{authority}/v1/chat/completions");

        // Request 1 warms up connection #1.
        let r1 = up
            .post_stream(
                &endpoint,
                &authority,
                "/v1/chat/completions",
                None,
                &[],
                Bytes::from_static(b"{\"model\":\"m\"}"),
            )
            .await
            .expect("request 1 succeeds");
        assert_eq!(r1.status, 200);
        drop(r1);

        // Request 2 checks out the warm connection #1 (`lease.is_reused() == true`),
        // which drops before sending headers; `post_h2_tls` evicts connection #1
        // and transparently redials connection #2.
        let r2 = up
            .post_stream(
                &endpoint,
                &authority,
                "/v1/chat/completions",
                None,
                &[],
                Bytes::from_static(b"{\"model\":\"m\"}"),
            )
            .await
            .expect("request 2 must transparently redial and succeed");
        assert_eq!(r2.status, 200);
        let mut body = Vec::new();
        let mut stream = r2.body;
        while let Some(c) = stream.next().await {
            body.extend_from_slice(&c.expect("chunk"));
        }
        assert_eq!(&body[..], b"{\"conn\":2}");
        assert_eq!(accepts.load(Ordering::SeqCst), 2);
    }
}
