//! OpenAI-compatible data plane: authenticate, pick a channel, copy bytes.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::Stream;
use futures_util::StreamExt;
use futures_util::stream;
use serde::Deserialize;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::admin::AppState;
use crate::store::Channel;
use crate::upstream::{UpstreamResponse, should_failover};

/// Borrowed view of the only field the gateway needs from the request body.
/// Deserializing this scans (and validates) the whole JSON without building a
/// `Value` DOM — no allocation proportional to the messages array.
#[derive(Deserialize)]
struct ModelRef<'a> {
    model: Option<&'a str>,
}

/// Bytes to send upstream: when the channel's upstream model name equals the
/// requested one (the common case) the original request body is passed through
/// untouched — no DOM, no re-serialization, no copy. A rename lazily builds the
/// DOM once per request and re-serializes it per attempt.
fn outbound_body(
    model: &str,
    upstream_model: &str,
    original: &Bytes,
    dom: &mut Option<Value>,
) -> Result<Bytes, String> {
    if upstream_model == model {
        return Ok(original.clone());
    }
    let value = match dom {
        Some(v) => v,
        None => match serde_json::from_slice(original) {
            Ok(v) => dom.insert(v),
            Err(e) => return Err(e.to_string()),
        },
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert("model".into(), Value::String(upstream_model.to_string()));
    }
    serde_json::to_vec(value)
        .map(Bytes::from)
        .map_err(|e| e.to_string())
}

/// `/health` is the unauthenticated probe HAProxy and the load-test rigs hit, so it
/// also carries the build version: "which xlite is actually deployed here?" must be
/// answerable with one curl, without an Admin login (`/api/overview` needs one).
fn health_payload() -> Value {
    json!({
        "status": "ok",
        "gateway": "xlite",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

pub async fn health() -> Json<Value> {
    Json(health_payload())
}

pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    match state.store.catalog().ping().await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ready" }))).into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "not_ready", "error": err.to_string() })),
        )
            .into_response(),
    }
}

pub async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let Some(key) = bearer_key(&headers).and_then(|token| state.store.authenticate(token)) else {
        return unauthorized();
    };
    let data: Vec<Value> = state
        .store
        .model_names_for(&key)
        .into_iter()
        .map(|id| json!({ "id": id, "object": "model", "owned_by": "xlite" }))
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> impl IntoResponse {
    let Some(token) = bearer_key(&headers) else {
        return unauthorized();
    };
    let Some(key) = state.store.authenticate(token) else {
        return unauthorized();
    };

    let model_ref: ModelRef = match serde_json::from_slice(&body) {
        Ok(v) => v,
        // Preserve the historical error split: a body that is not JSON at all
        // says "must be JSON"; valid JSON without a usable `model` field says
        // "missing model".
        Err(_) => {
            return match serde_json::from_slice::<Value>(&body) {
                Err(_) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": { "message": "request body must be JSON", "type": "invalid_request_error" }
                    })),
                )
                    .into_response(),
                Ok(_) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": { "message": "missing model", "type": "invalid_request_error" }
                    })),
                )
                    .into_response(),
            };
        }
    };
    let Some(model) = model_ref.model else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": { "message": "missing model", "type": "invalid_request_error" }
            })),
        )
            .into_response();
    };
    let Some(channels) = state.store.channels_for(&key, model) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": { "message": format!("model {model} is not available"), "type": "invalid_request_error" }
            })),
        )
            .into_response();
    };

    let mut last_error = "no channel available".to_string();
    let mut dom: Option<Value> = None;
    for channel in channels {
        match try_channel(&state, &channel, model, &body, &mut dom).await {
            Ok(response) => return response,
            Err(err) => last_error = err,
        }
    }
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({
            "error": { "message": last_error, "type": "upstream_error" }
        })),
    )
        .into_response()
}

/// Render an upstream error together with its full cause chain.
///
/// The 502 body carries `try_channel`'s `Err(String)`, and `anyhow::Error`'s
/// `Display` prints **only the outermost context** — so `map_err(|e|
/// e.to_string())` threw the real reason away: `failed to open transport to
/// upstream: …` (fd exhaustion), `HTTP/2 handshake failed: …` and
/// `h2 send_request: …` all collapsed into the one label `h2 pool acquire`,
/// indistinguishable from each other both in the client body and in the
/// load-test log. Report `{:#}` instead — anyhow's chain format, layers joined
/// by `: `.
fn err_chain(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

async fn try_channel(
    state: &AppState,
    channel: &Channel,
    model: &str,
    original: &Bytes,
    dom: &mut Option<Value>,
) -> Result<Response, String> {
    let body = outbound_body(model, &channel.upstream_model, original, dom)?;
    // Read the clock only when the diagnostic header is asked for: with the gate
    // off this path stays exactly what it was.
    let started = state.config.upstream_ttft_header.then(Instant::now);
    let upstream = state
        .upstream
        .post_stream(
            &channel.endpoint,
            &channel.authority,
            &channel.path,
            channel.auth.as_ref(),
            &channel.extra_headers,
            body,
        )
        .await
        .map_err(|e| err_chain(&e))?;
    if should_failover(upstream.status, false) {
        // Read the status before draining: `drain_error_body` consumes `upstream`.
        let status = upstream.status;
        let _ = drain_error_body(upstream, state.config.upstream_stall_timeout).await;
        return Err(format!("upstream {} returned {}", channel.base_url, status));
    }
    Ok(forward_response(upstream, started, state.config.upstream_stall_timeout).await)
}

/// Response header stamped when `XLITE_UPSTREAM_TTFT_HEADER` is on.
const UPSTREAM_TTFT_HEADER: &str = "x-xlite-upstream-ttft-ms";

/// Put back a chunk pulled early to time the upstream's first byte, in front of
/// the rest of the stream. `None` yields the stream unchanged.
fn with_head<S: Stream>(head: Option<S::Item>, rest: S) -> impl Stream<Item = S::Item> {
    stream::iter(head).chain(rest)
}

async fn forward_response(
    upstream: UpstreamResponse,
    started: Option<Instant>,
    stall_timeout: Duration,
) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY));
    let content_type = upstream
        .content_type
        .unwrap_or_else(|| "application/json".into());
    builder = builder.header(axum::http::header::CONTENT_TYPE, content_type);

    // Diagnostic gate (`XLITE_UPSTREAM_TTFT_HEADER=1`). Response headers leave
    // before the body, so the upstream's first-byte time can only be reported by
    // waiting for that byte first. An upstream that writes headers early would
    // see its client-visible first byte move later, which is why this is off by
    // default.
    let mut body = StallGuard::new(upstream.body, stall_timeout);
    let mut head: Option<Result<Bytes, std::io::Error>> = None;
    if let Some(t0) = started {
        head = body.next().await;
        if head.is_some() {
            builder = builder.header(UPSTREAM_TTFT_HEADER, t0.elapsed().as_millis().to_string());
        }
    }

    builder
        .body(Body::from_stream(with_head(head, body)))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "message": "failed to build response" } })),
            )
                .into_response()
        })
}

/// End a response body with a `TimedOut` error when the upstream has produced
/// no bytes for `timeout`, so a hung upstream cannot park a gateway stream (and
/// its pool lease) until the client gives up.
///
/// The deadline is re-armed on **every** item, so it only fires on true
/// silence — a slow but progressing stream keeps resetting it. `Duration::ZERO`
/// disables the guard entirely (the pre-existing "wait as long as the upstream
/// does" behaviour). See [`crate::config::Config::upstream_stall_timeout`].
struct StallGuard<S> {
    inner: S,
    timeout: Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    fired: bool,
}

impl<S> StallGuard<S> {
    fn new(inner: S, timeout: Duration) -> Self {
        let sleep = (!timeout.is_zero()).then(|| Box::pin(tokio::time::sleep(timeout)));
        Self {
            inner,
            timeout,
            sleep,
            fired: false,
        }
    }
}

impl<S> Stream for StallGuard<S>
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.fired {
            return Poll::Ready(None);
        }
        match Stream::poll_next(Pin::new(&mut this.inner), cx) {
            Poll::Ready(Some(item)) => {
                if let Some(sleep) = this.sleep.as_mut() {
                    sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + this.timeout);
                }
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                // Polling the timer with the caller's `cx` after `inner` yields
                // `Pending` is what makes a silent upstream wake us — and polling
                // `inner` first ensures data ready at the deadline is delivered
                // instead of being truncated by the timer.
                if let Some(sleep) = this.sleep.as_mut()
                    && sleep.as_mut().poll(cx).is_ready()
                {
                    this.fired = true;
                    this.sleep = None;
                    return Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("upstream produced no data for {}s", this.timeout.as_secs()),
                    ))));
                }
                Poll::Pending
            }
        }
    }
}

/// Read (and discard) an error body before failing over, capped in both size and
/// per-chunk silence so a broken upstream streaming an unbounded or hung error
/// body cannot stall the failover.
async fn drain_error_body(upstream: UpstreamResponse, stall_timeout: Duration) {
    drain_stream(upstream.body, stall_timeout).await;
}

async fn drain_stream<S>(stream: S, stall_timeout: Duration)
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    const MAX_DRAIN: usize = 64 * 1024;
    let mut body = StallGuard::new(stream, stall_timeout);
    let mut seen = 0usize;
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(bytes) => {
                seen += bytes.len();
                if seen >= MAX_DRAIN {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn bearer_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": { "message": "invalid api key", "type": "invalid_request_error" }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_reports_gateway_and_version() {
        let body = health_payload();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["gateway"], "xlite");
        assert_eq!(
            body["version"],
            env!("CARGO_PKG_VERSION"),
            "/health must carry the build version so a deployed binary can be \
             identified with one unauthenticated curl"
        );
    }

    /// Guard against regressing to `to_string()` (issue #49): the cause has to
    /// survive, otherwise the 502 body carries one label for four different
    /// failures — fd exhaustion, h2 handshake failure, pool closed, stream error.
    #[test]
    fn err_chain_keeps_the_underlying_cause() {
        let e = anyhow::anyhow!("failed to open transport to upstream: Too many open files")
            .context("h2 pool acquire");
        let rendered = err_chain(&e);
        assert!(
            rendered.contains("h2 pool acquire"),
            "the outermost context has to stay: {rendered}"
        );
        assert!(
            rendered.contains("Too many open files"),
            "the cause chain has to stay, or the 502 body turns four different \
             failures into one label: {rendered}"
        );
    }

    #[tokio::test]
    async fn with_head_prepends_without_reordering_or_dropping() {
        let out: Vec<u8> = with_head(Some(1u8), stream::iter(vec![2u8, 3, 4]))
            .collect()
            .await;
        assert_eq!(out, vec![1, 2, 3, 4], "the early chunk must come first");

        let out: Vec<u8> = with_head(None, stream::iter(vec![2u8, 3])).collect().await;
        assert_eq!(out, vec![2, 3], "gate off must leave the stream untouched");
    }

    /// The whole point of the guard: a silent upstream must not park the stream
    /// forever. It fires once, ends the stream, and does not error repeatedly.
    #[tokio::test(start_paused = true)]
    async fn stall_guard_fires_after_silence_and_ends_the_stream() {
        let inner = stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from_static(b"a"))])
            .chain(stream::pending::<Result<Bytes, std::io::Error>>());
        let mut guarded = StallGuard::new(inner, Duration::from_secs(5));

        assert_eq!(
            &guarded.next().await.expect("first chunk").unwrap()[..],
            b"a",
            "data before the deadline passes through untouched"
        );
        let err = guarded
            .next()
            .await
            .expect("the guard has to end the stream")
            .expect_err("after a silent gap the body must carry the timeout error");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            guarded.next().await.is_none(),
            "the stream must be finished, not erroring on every poll"
        );
    }

    /// Every arriving chunk must re-arm the deadline: a stream that emits at
    /// t=0s, t=4s, t=8s under a 5s stall timeout must deliver all three chunks
    /// and only time out at t=13s (not at the initial t=5s deadline).
    #[tokio::test(start_paused = true)]
    async fn stall_guard_resets_deadline_on_every_chunk() {
        let t0 = tokio::time::Instant::now();
        let inner = stream::unfold(0u32, |step| async move {
            match step {
                0 => Some((Ok(Bytes::from_static(b"a")), 1)),
                1 => {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    Some((Ok(Bytes::from_static(b"b")), 2))
                }
                2 => {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    Some((Ok(Bytes::from_static(b"c")), 3))
                }
                _ => std::future::pending().await,
            }
        });
        let mut guarded = StallGuard::new(Box::pin(inner), Duration::from_secs(5));

        assert_eq!(&guarded.next().await.unwrap().unwrap()[..], b"a");
        assert_eq!(&guarded.next().await.unwrap().unwrap()[..], b"b");
        assert_eq!(&guarded.next().await.unwrap().unwrap()[..], b"c");
        assert_eq!(t0.elapsed(), Duration::from_secs(8));

        let err = guarded
            .next()
            .await
            .expect("silence after third chunk must fire")
            .expect_err("must be TimedOut");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            t0.elapsed(),
            Duration::from_secs(13),
            "timer must fire 5s after the last chunk (t=8s + 5s), not at t=5s"
        );
    }

    /// `Duration::ZERO` is the default and must reproduce the old behaviour:
    /// wait as long as the upstream does, with no timer at all.
    #[tokio::test(start_paused = true)]
    async fn stall_guard_disabled_never_fires() {
        let inner = stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from_static(b"a"))])
            .chain(stream::pending::<Result<Bytes, std::io::Error>>());
        let mut guarded = StallGuard::new(inner, Duration::ZERO);
        assert!(guarded.next().await.is_some());
        assert!(
            tokio::time::timeout(Duration::from_secs(3600), guarded.next())
                .await
                .is_err(),
            "a disabled guard must keep waiting, like the pre-change code did"
        );
    }

    /// An upstream that returns 5xx headers and then stalls mid-error-body must
    /// not block failover forever when `upstream_stall_timeout` is configured.
    #[tokio::test(start_paused = true)]
    async fn drain_stream_stops_when_error_body_stalls() {
        let t0 = tokio::time::Instant::now();
        let stalled_err_body = stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from_static(
            b"err",
        ))])
        .chain(stream::pending::<Result<Bytes, std::io::Error>>());
        drain_stream(stalled_err_body, Duration::from_secs(5)).await;
        assert_eq!(t0.elapsed(), Duration::from_secs(5));
    }

    #[test]
    fn bearer_strips_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer sk-test"),
        );
        assert_eq!(bearer_key(&headers), Some("sk-test"));
    }

    #[test]
    fn same_model_name_passes_the_original_bytes_through() {
        let original = Bytes::from_static(
            br#"{"model":"gpt","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        let mut dom = None;
        let out = outbound_body("gpt", "gpt", &original, &mut dom).expect("passthrough");
        assert!(
            std::ptr::eq(out.as_ptr(), original.as_ptr()),
            "must be the same underlying buffer"
        );
        assert!(dom.is_none(), "no DOM may be built for the fast path");
    }

    #[test]
    fn rename_builds_the_dom_once_and_rewrites_the_model() {
        let original = Bytes::from_static(br#"{"model":"gpt","x":1}"#);
        let mut dom = None;
        let out = outbound_body("gpt", "upstream-gpt", &original, &mut dom).expect("rename");
        let parsed: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(parsed["model"], "upstream-gpt");
        assert_eq!(parsed["x"], 1);
        assert!(dom.is_some(), "DOM is kept for the next attempt");
        // A second attempt (another channel) reuses the same DOM.
        let out2 = outbound_body("gpt", "other", &original, &mut dom).expect("rename 2");
        let parsed2: Value = serde_json::from_slice(&out2).expect("json");
        assert_eq!(parsed2["model"], "other");
    }

    #[test]
    fn model_ref_scans_without_building_a_dom() {
        let body = br#"{"stream":true,"model":"gpt","messages":[1,2,3]}"#;
        let scanned: ModelRef = serde_json::from_slice(body).expect("scan");
        assert_eq!(scanned.model, Some("gpt"));
        let missing: ModelRef = serde_json::from_slice(br#"{"stream":true}"#).expect("scan");
        assert_eq!(missing.model, None);
        assert!(serde_json::from_slice::<ModelRef>(br#"not json"#).is_err());
    }
}
