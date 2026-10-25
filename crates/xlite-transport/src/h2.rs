//! HTTP/2 backend: `xlite-h2-pool` behind the [`Transport`](crate::Transport)
//! contract. One attempt per call; the stale-keep-alive redial lives in
//! [`Transport::execute_redial_once`](crate::Transport::execute_redial_once),
//! never in here.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use http::{HeaderMap, HeaderValue, StatusCode};
use xlite_h2_pool::{BoxIo, Config as PoolConfig, Dial, Lease, Pool, PoolError, PumpError};

use crate::attempt::{Attempt, CONNECTION_SPECIFIC};
use crate::body::BodyStream;
use crate::config::TransportConfig;
use crate::error::{StaleKind, TransportError};
use crate::stats::{Backend, HostStats, TransportStats, bucket_percentile};
use crate::transport::{AttemptOutcome, AttemptTelemetry, Transport};

/// How long a `None` [`TransportConfig::pool_idle_reclaim`] keeps idle
/// connections: practically never, without overflowing the pool's ms counters.
const NEVER_RECLAIM: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

/// HTTP/2 upstream transport, pooled per `scheme://authority`.
pub struct H2Transport {
    pool: Pool,
    header_timeout: Option<Duration>,
    stall_timeout: Option<Duration>,
    /// Stale-class evictions per pool key (`scheme://authority`).
    evicted_stale: Arc<Mutex<HashMap<String, u64>>>,
}

impl H2Transport {
    /// Build a transport whose dials open TCP (and, for `https`, TLS with the
    /// caller's [`rustls::ClientConfig`]).
    pub fn new(cfg: TransportConfig) -> Self {
        let connect_timeout = cfg.connect_timeout;
        #[cfg(feature = "tls-rustls")]
        let tls = cfg
            .tls
            .as_ref()
            .map(|c| tokio_rustls::TlsConnector::from(Arc::clone(c)));
        let dial: Dial = Arc::new(move |key: String| {
            #[cfg(feature = "tls-rustls")]
            let tls = tls.clone();
            Box::pin(async move {
                dial_key(
                    &key,
                    connect_timeout,
                    #[cfg(feature = "tls-rustls")]
                    tls.as_ref(),
                )
                .await
            })
        });
        Self::with_pool_config(cfg, dial)
    }

    /// Build a transport over a caller-supplied dial (test doubles, unix
    /// sockets, custom proxies). The dial receives the pool key
    /// `scheme://authority`.
    pub fn with_dial(cfg: TransportConfig, dial: Dial) -> Self {
        Self::with_pool_config(cfg, dial)
    }

    fn with_pool_config(cfg: TransportConfig, dial: Dial) -> Self {
        let pool = Pool::new(PoolConfig {
            max_conns_per_host: cfg.h2_max_conns_per_host.max(1),
            stream_budget_per_conn: cfg.h2_max_streams_per_conn.max(1),
            pinned_poll: true,
            dial,
            idle_timeout: cfg.pool_idle_reclaim.unwrap_or(NEVER_RECLAIM),
        });
        Self {
            pool,
            header_timeout: cfg.header_timeout,
            stall_timeout: cfg.stall_timeout,
            evicted_stale: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Pre-headers failures on a **reused** connection are the redial-eligible
    /// class: evict the socket, count it, and say so with a typed error.
    fn map_pre_headers(
        &self,
        err: h2::Error,
        reused: bool,
        key: &str,
        lease: &Lease,
    ) -> TransportError {
        if reused {
            lease.evict();
            if let Ok(mut guard) = self.evicted_stale.lock() {
                *guard.entry(key.to_string()).or_insert(0) += 1;
            }
            return TransportError::StaleConnection {
                kind: stale_kind(&err),
            };
        }
        // A fresh connection dying before the first byte is a connect-level
        // failure, never redial-eligible.
        match stale_kind(&err) {
            StaleKind::Reset => TransportError::Reset { code: None },
            _ => match err.into_io() {
                Some(io) => TransportError::Connect(io),
                None => TransportError::Io(io::Error::other("h2 connection ended before headers")),
            },
        }
    }
}

impl Transport for H2Transport {
    async fn execute(&self, attempt: Attempt) -> Result<AttemptOutcome, TransportError> {
        // 1. Everything checkable without a socket is checked here, first.
        attempt.validate()?;
        let scheme = attempt
            .uri
            .scheme_str()
            .ok_or(TransportError::InvalidRequest("uri has no scheme"))?;
        if scheme != "http" && scheme != "https" {
            return Err(TransportError::InvalidRequest(
                "only http (h2c) and https (h2) authorities are served by the h2 backend",
            ));
        }
        #[cfg(not(feature = "tls-rustls"))]
        if scheme == "https" {
            return Err(TransportError::InvalidRequest(
                "https authority but the transport was built without tls-rustls",
            ));
        }
        let authority = attempt
            .uri
            .authority()
            .map(|a| a.as_str().to_string())
            .ok_or(TransportError::InvalidRequest("uri has no authority"))?;
        let key = format!("{scheme}://{authority}");

        // 2. Acquire one stream slot (per-acquire wait/dial telemetry rides on
        // the lease; the pool never retries a failed execute for us).
        let mut lease = self.pool.acquire(&key).await.map_err(|e| match e {
            PoolError::Dial(io) => map_dial_error(io),
            PoolError::Handshake(e) => {
                TransportError::Io(io::Error::other(format!("h2 handshake: {e}")))
            }
            PoolError::Closed => TransportError::Closed,
        })?;
        let reused = lease.is_reused();
        let telemetry = AttemptTelemetry {
            dial: lease.dial_time(),
            reused,
            pool_wait: lease.wait(),
            in_flight_at_acquire: lease.in_flight(),
            redials: 0,
            redial_reason: None,
            headers_rewritten: Vec::new(),
        };

        // 3. Build the wire request: caller headers verbatim, plus the
        // mechanical rewrites reported in the telemetry.
        let (request, rewritten) = build_request(&attempt);

        // 4. Send and await headers, with the caller's optional budget.
        let eos = attempt.body.is_empty();
        let (response, mut send_stream) = lease
            .send_request(request, eos)
            .map_err(|e| self.map_pre_headers(e, reused, &key, &lease))?;
        if !eos && let Err(e) = send_stream.send_data(attempt.body.clone(), true) {
            return Err(self.map_pre_headers(e, reused, &key, &lease));
        }
        drop(send_stream);

        let response = match self.header_timeout {
            Some(budget) => match tokio::time::timeout(budget, response).await {
                Ok(result) => result,
                Err(_) => {
                    // The socket proved it cannot answer in the caller's
                    // budget: keep it out of the pool, exactly like the
                    // standalone gateway does.
                    lease.evict();
                    return Err(TransportError::HeaderTimeout { waited: budget });
                }
            },
            None => response.await,
        };
        let response = response.map_err(|e| self.map_pre_headers(e, reused, &key, &lease))?;

        let status =
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let headers = response.headers().clone();
        let version = response.version();
        let announced_len = headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());

        // 5. Hand the body to the connection's own task; dropping the stream
        // cancels upstream (RST_STREAM(CANCEL) on the wire).
        let recv = response.into_body();
        let rx = lease.register(recv).await;
        let body = H2Body {
            rx,
            _lease: lease,
            announced_len,
            received: 0,
        };

        Ok(AttemptOutcome {
            status,
            headers,
            version,
            body: BodyStream::new(body, self.stall_timeout),
            telemetry: AttemptTelemetry {
                headers_rewritten: rewritten,
                ..telemetry
            },
        })
    }

    fn stats(&self) -> TransportStats {
        let evicted_stale = self.evicted_stale.lock().ok();
        let mut hosts: Vec<HostStats> = self
            .pool
            .all_stats()
            .into_iter()
            .map(|(key, s)| {
                let authority = key
                    .split_once("://")
                    .map(|(_, a)| a.to_string())
                    .unwrap_or_else(|| key.clone());
                HostStats {
                    authority,
                    backend: Backend::H2,
                    connections: s.connections as u64,
                    in_flight: s.in_flight as u64,
                    dials: s.dials,
                    reused: s.reused,
                    failed_dials: s.failed_dials,
                    evicted: s.evicted,
                    evicted_stale: evicted_stale
                        .as_ref()
                        .and_then(|g| g.get(&key).copied())
                        .unwrap_or(0),
                    reaped_idle: Some(s.reclaimed),
                    waiting: s.waiting as u64,
                    wait_ms_p50: bucket_percentile(&s.wait_buckets, 50),
                    wait_ms_p99: bucket_percentile(&s.wait_buckets, 99),
                    wait_samples: s.queued_total,
                }
            })
            .collect();
        hosts.sort_by(|a, b| a.authority.cmp(&b.authority));
        TransportStats { hosts }
    }
}

/// The pump's typed stream errors, with the truncation rule: headers arrived,
/// so a connection dying mid-body means the body is **incomplete**, never a
/// clean end.
struct H2Body {
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, PumpError>>,
    /// Keeps the stream slot reserved (and the connection referenced) for the
    /// body's lifetime; dropping it is what triggers the upstream cancel.
    _lease: Lease,
    announced_len: Option<u64>,
    received: u64,
}

impl Stream for H2Body {
    type Item = Result<Bytes, TransportError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(Ok(bytes))) => {
                this.received += bytes.len() as u64;
                std::task::Poll::Ready(Some(Ok(bytes)))
            }
            std::task::Poll::Ready(Some(Err(err))) => {
                std::task::Poll::Ready(Some(Err(match err {
                    PumpError::Reset { .. } if matches!(this.announced_len, Some(len) if this.received < len) =>
                    {
                        // The `h2` client library resets a stream whose DATA
                        // ends short of its announced content-length
                        // (PROTOCOL_ERROR). That is an abnormal EOF, not a
                        // peer-initiated reset.
                        TransportError::BodyIncomplete
                    }
                    PumpError::Reset { code } => TransportError::Reset { code },
                    _ => TransportError::BodyIncomplete,
                })))
            }
            std::task::Poll::Ready(None) => {
                // Clean end of stream: an announced length that was not met is
                // an abnormal EOF, not end-of-body.
                if matches!(this.announced_len, Some(len) if len != this.received) {
                    return std::task::Poll::Ready(Some(Err(TransportError::BodyIncomplete)));
                }
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Caller headers verbatim, plus the mechanical rewrites. The returned static
/// slice names every header the transport touched.
fn build_request(attempt: &Attempt) -> (http::Request<()>, Vec<&'static str>) {
    let mut rewritten: Vec<&'static str> = Vec::new();
    let mut headers = HeaderMap::new();
    for (name, value) in attempt.headers.iter() {
        let raw = name.as_str();
        if let Some(static_name) = CONNECTION_SPECIFIC.iter().find(|c| **c == raw) {
            // RFC 9113 §8.2.2: connection-specific fields must not appear on
            // HTTP/2. Stripping is mandatory, not policy.
            rewritten.push(static_name);
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
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

    let mut request = http::Request::builder()
        .method(attempt.method.clone())
        .uri(attempt.uri.clone())
        .body(())
        .expect("method and uri came from a validated Attempt");
    *request.headers_mut() = headers;
    (request, rewritten)
}

/// Stale classification from what `h2` observed. Never inferred from display
/// strings.
fn stale_kind(err: &h2::Error) -> StaleKind {
    if err.is_reset() {
        StaleKind::Reset
    } else if err.is_go_away() {
        // The peer is tearing the connection down (GOAWAY / clean close).
        StaleKind::PeerClosed
    } else {
        StaleKind::Io
    }
}

/// The dial tags its failures by `io::ErrorKind` (see [`dial_key`]); the two
/// live next to each other so the mapping cannot drift.
fn map_dial_error(e: io::Error) -> TransportError {
    match e.kind() {
        io::ErrorKind::AddrNotAvailable => TransportError::Dns(e),
        io::ErrorKind::InvalidData => TransportError::Tls(e),
        _ => TransportError::Connect(e),
    }
}

/// Open one upstream connection for a `scheme://authority` pool key.
#[cfg_attr(not(feature = "tls-rustls"), allow(unused_variables))]
async fn dial_key(
    key: &str,
    connect_timeout: Duration,
    #[cfg(feature = "tls-rustls")] tls: Option<&tokio_rustls::TlsConnector>,
) -> io::Result<BoxIo> {
    let (scheme, authority) = key.split_once("://").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "pool key must be scheme://authority",
        )
    })?;
    let auth: http::uri::Authority = authority
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("authority: {e}")))?;
    let host = auth.host().to_string();
    let port = auth
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });

    // DNS first, tagged distinctly: `TransportError::Dns` must not be
    // confused with a refused connection.
    let addr = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::AddrNotAvailable, format!("dns: {e}")))?
        .next()
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, "dns returned no address")
        })?;
    let tcp = tokio::time::timeout(connect_timeout, tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "tcp connect timeout"))?
        .map_err(|e| io::Error::new(e.kind(), format!("tcp connect: {e}")))?;
    let _ = tcp.set_nodelay(true);

    match scheme {
        "http" => Ok(Box::new(tcp) as BoxIo),
        "https" => {
            #[cfg(feature = "tls-rustls")]
            {
                let tls = tls.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "https authority but no tls config supplied",
                    )
                })?;
                let server_name =
                    rustls_pki_types::ServerName::try_from(host.clone()).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidInput, format!("tls host: {e}"))
                    })?;
                let tls_stream =
                    tokio::time::timeout(connect_timeout, tls.connect(server_name, tcp))
                        .await
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::TimedOut, "tls handshake timeout")
                        })?
                        // Tagged InvalidData so `map_dial_error` reports Tls.
                        .map_err(|e| {
                            io::Error::new(io::ErrorKind::InvalidData, format!("tls: {e}"))
                        })?;
                if tls_stream.get_ref().1.alpn_protocol().map(|p| p == b"h2") != Some(true) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "peer did not negotiate h2; http/1.1 upstreams belong to the h1 backend",
                    ));
                }
                Ok(Box::new(tls_stream) as BoxIo)
            }
            #[cfg(not(feature = "tls-rustls"))]
            {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "https authority but built without tls-rustls",
                ))
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported scheme",
        )),
    }
}
