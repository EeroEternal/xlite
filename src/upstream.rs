//! Upstream HTTP client. h2c uses the native pool (Go-style stream accounting);
//! everything else uses reqwest.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use futures_util::Stream;
use http::{HeaderName, HeaderValue, Request};
use http_body_util::Full;
use hyper::body::Body;
use reqwest::Client;
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use xlite_h1_pool::{Config as H1PoolConfig, Lease as H1Lease, Pool as H1Pool, ResponseBody};
use xlite_h2_pool::{BoxIo, Config as PoolConfig, Dial, Lease, Pool};

use crate::catalog::{HeaderPair, UpstreamShape};
use crate::config::Config;

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
    // The credential header is validated even when it will not be sent, so a
    // typo is rejected at write time instead of silently doing nothing.
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
        // Identical to `authorization_header()` — the default path is unchanged.
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

/// Per-authority admission cap for the reqwest (HTTP/1.1) path.
///
/// The h2c pool bounds upstream connections with `max_conns_per_host`, but the
/// reqwest path had no such bound: HTTP/1.1 is one request per connection, so a
/// burst opened one upstream socket per in-flight request (measured: 20
/// concurrent requests ⇒ 20 upstream connections even with
/// `XLITE_UPSTREAM_MAX_CONNS_PER_HOST=4`). This gate gives that path the same
/// *shape* of bound the h2c pool already has — at most `permits` in-flight
/// requests per `host:port`, the rest wait here — rather than inventing a new
/// knob.
///
/// Applied **only to cleartext `http://`**, where the protocol is known to be
/// HTTP/1.1 (reqwest never speaks h2c without `http2_prior_knowledge`). `https`
/// is deliberately excluded: ALPN may negotiate h2, and an in-flight cap there
/// would throttle multiplexing instead of bounding connection count.
///
/// Waiting is the advertised behaviour (h2 pool: "at the cap it waits"); the
/// gateway still never fabricates a 429/503.
struct AdmissionGate {
    permits: usize,
    hosts: Mutex<HashMap<String, Arc<GateSlot>>>,
}

/// One authority's live gate state: the semaphore **plus** the visibility counters.
/// The queue on this path is real and bounded-only-by-clients, so it must be
/// readable (the #68/#70 lesson: a queue that cannot be seen cannot be reasoned about).
struct GateSlot {
    sem: Arc<Semaphore>,
    /// Acquirers currently parked on this authority.
    waiting: AtomicUsize,
    /// High-water mark of [`GateSlot::waiting`].
    waiting_max: AtomicUsize,
}

/// One authority's point-in-time gate snapshot (for `GET /api/pool`).
pub struct GateStat {
    pub authority: String,
    pub permits: usize,
    pub in_flight: usize,
    pub waiting: usize,
    pub waiting_max: usize,
}

impl AdmissionGate {
    fn new(permits: usize) -> Self {
        Self {
            permits: permits.max(1),
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// The gate slot for one authority, created on first use and shared for the
    /// process lifetime (so a catalog reload does not reset live permits).
    fn slot(&self, authority: &str) -> Arc<GateSlot> {
        let mut hosts = self.hosts.lock().expect("admission gate poisoned");
        Arc::clone(hosts.entry(authority.to_string()).or_insert_with(|| {
            Arc::new(GateSlot {
                sem: Arc::new(Semaphore::new(self.permits)),
                waiting: AtomicUsize::new(0),
                waiting_max: AtomicUsize::new(0),
            })
        }))
    }

    /// Wait for a slot on `authority`. The returned permit must be held for as
    /// long as the request is in flight, including its (possibly streaming)
    /// response body.
    async fn acquire(&self, authority: &str) -> OwnedSemaphorePermit {
        let slot = self.slot(authority);
        // Fast path: a free slot and nobody ahead of us (tokio's semaphore is
        // fair, so a waiter would have been served first) ⇒ this request never
        // parks, so it must NOT touch the waiting counters. Counting it would
        // make `waiting_max` report "queued" for a request that never queued —
        // the exact false number the #68/#70 visibility work exists to prevent.
        match Arc::clone(&slot.sem).try_acquire_owned() {
            Ok(permit) => return permit,
            Err(TryAcquireError::NoPermits) => {}
            Err(TryAcquireError::Closed) => unreachable!("gate semaphore is never closed"),
        }
        // Slow path: we are actually parking; only now are the counters meaningful.
        let depth = slot.waiting.fetch_add(1, Ordering::Relaxed) + 1;
        slot.waiting_max.fetch_max(depth, Ordering::Relaxed);
        let permit = Arc::clone(&slot.sem)
            .acquire_owned()
            .await
            .expect("admission semaphore is never closed");
        slot.waiting.fetch_sub(1, Ordering::Relaxed);
        permit
    }

    /// Point-in-time snapshot of every authority that has ever been gated, sorted
    /// by authority. Read-only and off the data path — this is what keeps the
    /// queue on this route from repeating the "queue that cannot be seen" mistake.
    fn snapshot(&self) -> Vec<GateStat> {
        let hosts = self.hosts.lock().expect("admission gate poisoned");
        let mut out: Vec<GateStat> = hosts
            .iter()
            .map(|(authority, slot)| GateStat {
                authority: authority.clone(),
                permits: self.permits,
                in_flight: self.permits.saturating_sub(slot.sem.available_permits()),
                waiting: slot.waiting.load(Ordering::Relaxed),
                waiting_max: slot.waiting_max.load(Ordering::Relaxed),
            })
            .collect();
        out.sort_by(|a, b| a.authority.cmp(&b.authority));
        out
    }
}

#[derive(Clone)]
pub struct Upstream {
    h2c: bool,
    pool: Arc<Pool>,
    /// Opt-in native HTTP/1.1 pool (`XLITE_UPSTREAM_H1_POOL=1`). When present it
    /// owns cleartext `http://` upstreams, replacing reqwest + [`AdmissionGate`].
    h1: Option<Arc<H1Pool>>,
    http: Client,
    /// Bounds in-flight requests per cleartext HTTP/1.1 upstream (see
    /// [`AdmissionGate`]). `https` never touches it. Unused when [`Self::h1`] is set.
    admission: Arc<AdmissionGate>,
}

pub struct UpstreamBody {
    inner: BodyKind,
    _lease: Option<Lease>,
    /// Holds the native HTTP/1.1 pool lease for the whole response body (same
    /// reason as `_permit`: the bound must cover the full request lifetime).
    _h1: Option<H1Lease>,
    /// Holds the HTTP/1.1 admission slot for the whole response body so the
    /// bound covers the request's full lifetime, not just the header exchange.
    _permit: Option<OwnedSemaphorePermit>,
}

enum BodyKind {
    H1(ResponseBody),
    H2(h2::RecvStream, usize),
    Pump(tokio::sync::mpsc::Receiver<Result<Bytes, String>>),
    Reqwest(Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>),
}

pub struct UpstreamResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: UpstreamBody,
}

impl Upstream {
    pub fn new(cfg: &Config) -> Self {
        let dial: Dial = Arc::new(|host: String| {
            Box::pin(async move {
                let stream = TcpStream::connect(host.as_str()).await?;
                let _ = stream.set_nodelay(true);
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
        // The h1 pool is a different crate with its own `Dial`/`BoxIo` types, so
        // it gets its own dial closure (same shape, same TCP+nodelay setup).
        let h1 = cfg.h1_pool.then(|| {
            let dial: xlite_h1_pool::Dial = Arc::new(|host: String| {
                Box::pin(async move {
                    let stream = TcpStream::connect(host.as_str()).await?;
                    let _ = stream.set_nodelay(true);
                    Ok(Box::new(stream) as Box<dyn xlite_h1_pool::BoxIo>)
                })
            });
            Arc::new(H1Pool::new(H1PoolConfig {
                max_conns_per_host: cfg.max_conns_per_host.max(1),
                dial,
            }))
        });
        let http = Client::builder()
            .pool_max_idle_per_host(64)
            .tcp_nodelay(true)
            .timeout(Duration::from_secs(600))
            .build()
            .expect("reqwest client");
        Self {
            h2c: cfg.h2c,
            pool: Arc::new(pool),
            h1,
            http,
            admission: Arc::new(AdmissionGate::new(cfg.max_conns_per_host)),
        }
    }

    /// Read-only pool snapshot for the control plane (`GET /api/pool`).
    ///
    /// Deliberately **not** on the data path: nothing in `proxy.rs` calls this.
    /// It exists so a load-test probe can read the real per-host connection and
    /// in-flight counts instead of inferring them from `ss` (issue #49), and so
    /// "what was the pool doing when those 502s happened?" is answerable.
    pub fn pool_stats(&self) -> Vec<(String, xlite_h2_pool::PoolStats)> {
        self.pool.all_stats()
    }

    /// Point-in-time snapshot of the HTTP/1.1 admission gates (`GET /api/pool`).
    ///
    /// Same reasoning as [`Self::pool_stats`]: this gate parks requests in the
    /// gateway, and a queue that cannot be seen cannot be reasoned about — the
    /// probe and the operator get `waiting` / `waiting_max` / `in_flight` per
    /// authority instead of a silent, unbounded wait.
    pub fn http1_gate_stats(&self) -> Vec<GateStat> {
        self.admission.snapshot()
    }

    /// Connection-level stats for the opt-in native HTTP/1.1 pool
    /// (`GET /api/pool`). Empty when the pool is off.
    pub fn h1_pool_stats(&self) -> Vec<(String, xlite_h1_pool::PoolStats)> {
        self.h1
            .as_ref()
            .map(|pool| pool.all_stats())
            .unwrap_or_default()
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
        // Auth header, extra headers, authority and path are all computed once
        // per channel at snapshot build — the request path parses nothing.
        // Cleartext `http://` prefers the native h1 pool when it is enabled;
        // `https` and the h2c pool are unaffected by that switch.
        if endpoint.starts_with("http://") {
            if self.h2c {
                return self.post_h2c(authority, path, auth, extra, body).await;
            }
            if let Some(h1) = &self.h1 {
                return self.post_h1(h1, authority, path, auth, extra, body).await;
            }
            // No native pool: bound the reqwest path at the request level.
            let permit = if authority.is_empty() {
                None
            } else {
                Some(self.admission.acquire(authority).await)
            };
            return self.post_reqwest(endpoint, auth, extra, body, permit).await;
        }
        // `https://` keeps reqwest's default behaviour; see [`AdmissionGate`].
        self.post_reqwest(endpoint, auth, extra, body, None).await
    }

    /// Send one request through the native HTTP/1.1 pool. The returned lease is
    /// held by the response body so the connection stays reserved until the body
    /// is fully consumed. (`&self` is unused on purpose: connection selection and
    /// dialing are entirely the pool's job.)
    async fn post_h1(
        &self,
        h1: &Arc<H1Pool>,
        authority: &str,
        path: &str,
        auth: Option<&(HeaderName, HeaderValue)>,
        extra: &[(HeaderName, HeaderValue)],
        body: Bytes,
    ) -> Result<UpstreamResponse> {
        if authority.is_empty() {
            anyhow::bail!("upstream url missing host");
        }
        let lease = h1
            .acquire(authority)
            .await
            .map_err(|e| anyhow::anyhow!("h1 pool acquire: {e}"))?;
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header("host", authority)
            .header("content-type", "application/json");
        if let Some((name, value)) = auth {
            builder = builder.header(name.clone(), value.clone());
        }
        for (name, value) in extra {
            builder = builder.header(name.clone(), value.clone());
        }
        let request = builder.body(Full::new(body)).context("build h1 request")?;
        let response = lease
            .send_request(request)
            .await
            .map_err(|e| anyhow::anyhow!("h1 send_request: {e}"))?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok(UpstreamResponse {
            status,
            content_type,
            body: UpstreamBody {
                inner: BodyKind::H1(response.into_body()),
                _lease: None,
                _h1: Some(lease),
                _permit: None,
            },
        })
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
        let mut lease = self
            .pool
            .acquire(authority)
            .await
            .context("h2 pool acquire")?;
        let eos = body.is_empty();
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header("host", authority)
            .header("content-type", "application/json");
        if let Some((name, value)) = auth {
            builder = builder.header(name.clone(), value.clone());
        }
        for (name, value) in extra {
            builder = builder.header(name.clone(), value.clone());
        }
        let request = builder.body(()).context("build h2 request")?;
        let (response_future, mut send_stream) = lease
            .send_request(request, eos)
            .map_err(|e| anyhow::anyhow!("h2 send_request: {e}"))?;
        if !eos {
            send_stream
                .send_data(body, true)
                .map_err(|e| anyhow::anyhow!("h2 send_data: {e}"))?;
        }
        drop(send_stream);
        let response = response_future
            .await
            .map_err(|e| anyhow::anyhow!("h2 response: {e}"))?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let recv = response.into_body();
        let inner = if lease.pinned_poll() {
            BodyKind::Pump(lease.register(recv).await)
        } else {
            BodyKind::H2(recv, 0)
        };
        Ok(UpstreamResponse {
            status,
            content_type,
            body: UpstreamBody {
                inner,
                _lease: Some(lease),
                _h1: None,
                _permit: None,
            },
        })
    }

    async fn post_reqwest(
        &self,
        url: &str,
        auth: Option<&(HeaderName, HeaderValue)>,
        extra: &[(HeaderName, HeaderValue)],
        body: Bytes,
        permit: Option<OwnedSemaphorePermit>,
    ) -> Result<UpstreamResponse> {
        let mut req = self
            .http
            .post(url)
            .header("content-type", "application/json");
        if let Some((name, value)) = auth {
            req = req.header(name.clone(), value.clone());
        }
        for (name, value) in extra {
            req = req.header(name.clone(), value.clone());
        }
        let response = req.body(body).send().await.context("upstream request")?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok(UpstreamResponse {
            status,
            content_type,
            body: UpstreamBody {
                inner: BodyKind::Reqwest(Box::pin(response.bytes_stream())),
                _lease: None,
                _h1: None,
                _permit: permit,
            },
        })
    }
}

impl Stream for UpstreamBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match &mut this.inner {
            BodyKind::H1(body) => loop {
                match Pin::new(&mut *body).poll_frame(cx) {
                    Poll::Ready(Some(Ok(frame))) => {
                        if let Ok(data) = frame.into_data() {
                            return Poll::Ready(Some(Ok(data)));
                        }
                        // trailers: keep polling for the next data frame
                    }
                    Poll::Ready(Some(Err(err))) => {
                        return Poll::Ready(Some(Err(std::io::Error::other(err))));
                    }
                    // Reaching end-of-stream is marked complete by `ResponseBody`.
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Pending => return Poll::Pending,
                }
            },
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
            BodyKind::Reqwest(stream) => match Pin::new(stream).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(bytes))),
                Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(std::io::Error::other(err)))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
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
pub fn should_failover(status: u16, bytes_sent: bool) -> bool {
    !bytes_sent && (status >= 500 || status == 429)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `GET /api/pool`'s data source: a fresh gateway has dialed nothing, so it
    /// must report nothing. If it invented hosts, the field probe would size
    /// connections against capacity that does not exist.
    #[test]
    fn pool_stats_start_empty_for_a_fresh_upstream() {
        let cfg = Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: true,
            h1_pool: false,
            admin_password: "x".into(),
            session_secret: None,
            max_conns_per_host: 3,
            upstream_stream_cap: 250,
            catalog_reload_secs: 0,
            upstream_ttft_header: false,
        };
        let up = Upstream::new(&cfg);
        assert!(up.pool_stats().is_empty());
    }

    #[test]
    fn h2c_parts_splits_host_port_and_path() {
        let (authority, path) = h2c_parts("http://10.99.255.102:4101/v1/chat/completions").unwrap();
        assert_eq!(authority, "10.99.255.102:4101");
        assert_eq!(path, "/v1/chat/completions");
    }

    #[test]
    fn authorization_header_empty_when_no_key() {
        assert_eq!(authorization_header(""), "");
        assert_eq!(authorization_header("sk-test"), "Bearer sk-test");
    }

    #[test]
    fn failover_only_before_first_byte_on_5xx() {
        assert!(should_failover(502, false));
        assert!(should_failover(429, false));
        assert!(!should_failover(400, false));
        assert!(!should_failover(502, true));
        assert!(!should_failover(200, false));
    }

    fn pair(name: &str, value: &str) -> HeaderPair {
        HeaderPair {
            name: name.into(),
            value: value.into(),
        }
    }

    /// The whole point of the feature: an untouched provider must produce exactly
    /// the header the gateway produced before upstream shapes existed.
    #[test]
    fn default_auth_is_byte_identical_to_the_old_bearer_header() {
        let (name, value) = build_auth("bearer", "", "sk-test")
            .expect("default auth")
            .expect("credential present");
        assert_eq!(name.as_str(), "authorization");
        assert_eq!(value.to_str().unwrap(), authorization_header("sk-test"));
        assert_eq!(value.to_str().unwrap(), "Bearer sk-test");

        // An empty `auth_scheme` is the bearer default, not an error.
        let (_, value) = build_auth("", "", "sk-test").unwrap().unwrap();
        assert_eq!(value.to_str().unwrap(), "Bearer sk-test");
    }

    #[test]
    fn auth_scheme_none_and_empty_key_send_no_credential() {
        assert!(build_auth("none", "", "sk-test").unwrap().is_none());
        assert!(build_auth("bearer", "", "").unwrap().is_none());
        // A custom header name is still validated even when nothing is sent.
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

    /// The HTTP/1.1 admission gate: a host is capped at `permits` in-flight
    /// requests (a second acquire must wait), each host is bounded independently,
    /// and releasing a slot wakes the waiter. The snapshot must make all of that
    /// visible (`in_flight` / `waiting` / `waiting_max` per authority) — a gate
    /// that parks requests but reports nothing would repeat the
    /// "queue that cannot be seen" mistake.
    #[tokio::test]
    async fn admission_gate_bounds_each_host_independently() {
        let gate = std::sync::Arc::new(AdmissionGate::new(1));
        assert!(
            gate.snapshot().is_empty(),
            "nothing gated yet ⇒ empty snapshot"
        );

        let held = gate.acquire("h:1").await;
        let snap = gate.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].authority, "h:1");
        assert_eq!(snap[0].permits, 1);
        assert_eq!(snap[0].in_flight, 1, "the held permit is in flight");
        assert_eq!(snap[0].waiting, 0);
        assert_eq!(
            snap[0].waiting_max, 0,
            "a request that never queued must not raise the high-water mark"
        );

        // Park a second acquirer on the same authority; it must show up as
        // `waiting` (and the high-water mark has to record it).
        let blocked = {
            let gate = std::sync::Arc::clone(&gate);
            tokio::spawn(async move { gate.acquire("h:1").await })
        };
        for _ in 0..50 {
            if gate.snapshot()[0].waiting == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let snap = gate.snapshot();
        assert_eq!(snap[0].waiting, 1, "a parked acquirer must show up");
        assert_eq!(snap[0].waiting_max, 1, "the high-water mark has to see it");
        assert_eq!(snap[0].in_flight, 1, "the held permit still counts");

        drop(held);
        let reacquired = tokio::time::timeout(Duration::from_millis(50), blocked)
            .await
            .expect("releasing a slot must wake the waiter")
            .unwrap();
        let snap = gate.snapshot();
        assert_eq!(snap[0].waiting, 0, "the queue has to drain");
        assert_eq!(snap[0].in_flight, 1, "the woken acquirer now holds it");
        assert_eq!(snap[0].waiting_max, 1, "the high-water mark stays");
        drop(reacquired);
        assert_eq!(gate.snapshot()[0].in_flight, 0, "dropped ⇒ released");

        // A different host is bounded independently and shows up as its own entry.
        let other = gate.acquire("h:2").await;
        let snap = gate.snapshot();
        assert_eq!(snap.len(), 2, "each authority gets its own entry");
        assert_eq!(snap[0].authority, "h:1");
        assert_eq!(snap[1].authority, "h:2");
        assert_eq!(snap[1].in_flight, 1);
        drop(other);
    }
}
