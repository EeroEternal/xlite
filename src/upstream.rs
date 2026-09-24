//! Upstream HTTP client. h2c uses the native pool (Go-style stream accounting);
//! everything else uses reqwest.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use futures_util::Stream;
use http::Request;
use reqwest::Client;
use tokio::net::TcpStream;
use xlite_h2_pool::{BoxIo, Config as PoolConfig, Dial, Lease, Pool};

use crate::config::Config;

#[derive(Clone)]
pub struct Upstream {
    h2c: bool,
    pool: Arc<Pool>,
    http: Client,
}

pub struct UpstreamBody {
    inner: BodyKind,
    _lease: Option<Lease>,
}

enum BodyKind {
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
            stream_budget_per_conn: usize::MAX,
            pinned_poll: true,
            dial,
            idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
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
            http,
        }
    }

    pub async fn post_stream(
        &self,
        endpoint: &str,
        authority: &str,
        path: &str,
        authorization: &str,
        body: Bytes,
    ) -> Result<UpstreamResponse> {
        // Authority / path / Authorization are computed once per channel at
        // snapshot build — the request path does not parse the URL again.
        if self.h2c && endpoint.starts_with("http://") {
            self.post_h2c(authority, path, authorization, body).await
        } else {
            self.post_reqwest(endpoint, authorization, body).await
        }
    }

    async fn post_h2c(
        &self,
        authority: &str,
        path: &str,
        authorization: &str,
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
        if !authorization.is_empty() {
            builder = builder.header("authorization", authorization);
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
            },
        })
    }

    async fn post_reqwest(
        &self,
        url: &str,
        authorization: &str,
        body: Bytes,
    ) -> Result<UpstreamResponse> {
        let mut req = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .body(body);
        if !authorization.is_empty() {
            req = req.header("authorization", authorization);
        }
        let response = req.send().await.context("upstream request")?;
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
            },
        })
    }
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
}
