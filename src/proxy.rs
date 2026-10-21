//! OpenAI-compatible data plane: authenticate, pick a channel, copy bytes.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::Stream;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::admin::AppState;
use crate::protocol::{
    ParsedResponsesRequest, build_openai_chat_body_from_anthropic,
    build_openai_chat_body_from_responses, build_openai_responses_body, collect_upstream_body,
    format_anthropic_error_body, parse_anthropic_request, parse_responses_request,
    translate_openai_chat_to_anthropic_json, translate_openai_chat_to_responses_json,
    translate_openai_sse_to_anthropic_stream, translate_openai_sse_to_responses_stream,
};
use crate::store::Channel;
use crate::upstream::{
    UpstreamResponse, embeddings_url, h2c_parts, responses_url, should_failover,
};

/// SSE comment frame emitted during silent upstream prefill or reasoning pauses.
/// Per the WHATWG `EventSource` specification, lines starting with `:` are
/// comments and are ignored by OpenAI SDKs and SSE parsers while keeping
/// intermediate NATs, reverse proxies, and client read timers alive.
const SSE_KEEPALIVE_FRAME: &[u8] = b": keep-alive\n\n";

/// Borrowed view of the only fields the gateway needs from the request body.
/// Deserializing this scans (and validates) the whole JSON without building a
/// `Value` DOM — no allocation proportional to the messages array.
#[derive(Deserialize)]
struct ModelRef<'a> {
    model: Option<&'a str>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    stream_options: Option<serde::de::IgnoredAny>,
}

/// Append `"stream_options":{"include_usage":true}` before the closing `}` of a
/// validated non-empty JSON object without allocating a `serde_json::Value` DOM.
fn ensure_stream_include_usage(body: &Bytes) -> Bytes {
    if let Some(close) = body.iter().rposition(|&b| b == b'}') {
        const SUFFIX: &[u8] = b",\"stream_options\":{\"include_usage\":true}}";
        let mut out = Vec::with_capacity(close + SUFFIX.len());
        out.extend_from_slice(&body[..close]);
        out.extend_from_slice(SUFFIX);
        Bytes::from(out)
    } else {
        body.clone()
    }
}

/// Cached per-request strategy for rewriting `"model"` when a channel maps the
/// public model name to a different `upstream_model`.
#[derive(Debug)]
enum RewritePlan {
    /// Byte range `start..end` (including quotes) of the single top-level
    /// `"model"` string value inside the original request body.
    Splice(Range<usize>),
    /// Fallback DOM used only when the top-level `"model"` key is escaped or
    /// duplicated.
    Dom(Value),
}

/// Scan a validated JSON object at nesting depth 1 to locate the byte range
/// (including quotes) of the single top-level `"model"` string value.
///
/// Returns `None` if the root is not an object, if any top-level key uses
/// JSON escape sequences, if `"model"` appears more than once at the top level,
/// or if its value is not a string literal.
fn find_top_level_model_value_span(json: &[u8]) -> Option<Range<usize>> {
    let mut i = skip_ws(json, 0);
    if json.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    let mut depth = 1usize;
    let mut expect_top_key = true;
    let mut found: Option<Range<usize>> = None;

    while i < json.len() {
        i = skip_ws(json, i);
        let Some(&b) = json.get(i) else {
            break;
        };
        match b {
            b'"' => {
                let (str_end, has_escape) = scan_json_string(json, i)?;
                if depth == 1 && expect_top_key {
                    expect_top_key = false;
                    if has_escape {
                        // Fall back to full DOM if a top-level key contains escapes
                        // (e.g. "\u006dodel").
                        return None;
                    }
                    let is_model_key = &json[i..str_end] == b"\"model\"";
                    i = skip_ws(json, str_end);
                    if json.get(i) != Some(&b':') {
                        return None;
                    }
                    i = skip_ws(json, i + 1);
                    if is_model_key {
                        if found.is_some() {
                            // Duplicate top-level "model" key; let DOM fallback deduplicate.
                            return None;
                        }
                        if json.get(i) != Some(&b'"') {
                            return None;
                        }
                        let (val_end, _) = scan_json_string(json, i)?;
                        found = Some(i..val_end);
                        i = val_end;
                    }
                } else {
                    i = str_end;
                }
            }
            b'{' | b'[' => {
                expect_top_key = false;
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            b',' => {
                if depth == 1 {
                    expect_top_key = true;
                }
                i += 1;
            }
            _ => {
                expect_top_key = false;
                i += 1;
            }
        }
    }
    found
}

#[inline]
fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while let Some(&b) = bytes.get(i) {
        if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// Given `bytes[start] == b'"'`, returns `(end_exclusive, has_escape)`.
#[inline]
fn scan_json_string(bytes: &[u8], start: usize) -> Option<(usize, bool)> {
    let mut i = start + 1;
    let mut has_escape = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                has_escape = true;
                i += 2;
            }
            b'"' => return Some((i + 1, has_escape)),
            _ => i += 1,
        }
    }
    None
}

/// Bytes to send upstream:
/// - When `upstream_model == model` (the common case), the original `Bytes`
///   handle is cloned in `O(1)` without copying or parsing.
/// - When `upstream_model != model`, we locate the top-level `"model"` value
///   span once and splice the replacement string directly into a flat buffer,
///   avoiding multi-megabyte `serde_json::Value` DOM allocation and
///   re-serialization on long-context payloads.
fn outbound_body(
    model: &str,
    upstream_model: &str,
    original: &Bytes,
    plan: &mut Option<RewritePlan>,
) -> Result<Bytes, String> {
    if upstream_model == model {
        return Ok(original.clone());
    }
    if plan.is_none() {
        if let Some(span) = find_top_level_model_value_span(original) {
            *plan = Some(RewritePlan::Splice(span));
        } else {
            let v: Value = serde_json::from_slice(original).map_err(|e| e.to_string())?;
            *plan = Some(RewritePlan::Dom(v));
        }
    }
    match plan.as_mut().expect("plan initialized above") {
        RewritePlan::Splice(span) => {
            let encoded = serde_json::to_vec(upstream_model).map_err(|e| e.to_string())?;
            let new_len = original.len() - span.len() + encoded.len();
            let mut out = Vec::with_capacity(new_len);
            out.extend_from_slice(&original[..span.start]);
            out.extend_from_slice(&encoded);
            out.extend_from_slice(&original[span.end..]);
            Ok(Bytes::from(out))
        }
        RewritePlan::Dom(value) => {
            if let Some(obj) = value.as_object_mut() {
                obj.insert("model".into(), Value::String(upstream_model.to_string()));
            }
            serde_json::to_vec(value)
                .map(Bytes::from)
                .map_err(|e| e.to_string())
        }
    }
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
    let Some(model) = model_ref.model.map(str::trim).filter(|m| !m.is_empty()) else {
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

    let started_at = std::time::Instant::now();
    let effective_body = if usage_webhook_target().is_some()
        && model_ref.stream == Some(true)
        && model_ref.stream_options.is_none()
    {
        ensure_stream_include_usage(&body)
    } else {
        body.clone()
    };
    let mut last_failure = ChannelFailure::Transport {
        message: "no channel available".to_string(),
        channel_id: 0,
        provider_id: 0,
        upstream_model: String::new(),
    };
    let mut plan: Option<RewritePlan> = None;
    for channel in channels {
        match try_channel(
            &state,
            &key,
            &channel,
            model,
            &effective_body,
            &mut plan,
            started_at,
        )
        .await
        {
            Ok(response) => return response,
            Err(failure) => last_failure = failure,
        }
    }
    last_failure.into_openai_response()
}

/// Anthropic `/v1/messages` compatibility endpoint powered by `unigateway-protocol`.
pub async fn anthropic_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Response {
    let Some(token) = bearer_key(&headers) else {
        return anthropic_unauthorized();
    };
    let Some(key) = state.store.authenticate(token) else {
        return anthropic_unauthorized();
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return anthropic_bad_request("request body must be JSON");
        }
    };
    let request = match parse_anthropic_request(&payload) {
        Ok(r) => r,
        Err(msg) => return anthropic_bad_request(&msg),
    };
    let Some(channels) = state.store.channels_for(&key, &request.model) else {
        let body = format_anthropic_error_body(
            404,
            b"",
            &format!("model {} is not available", request.model),
        );
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response());
    };

    let started_at = std::time::Instant::now();
    let mut last_failure = ChannelFailure::Transport {
        message: "no channel available".to_string(),
        channel_id: 0,
        provider_id: 0,
        upstream_model: String::new(),
    };
    for channel in channels {
        let out_body = match build_openai_chat_body_from_anthropic(&channel, &request) {
            Ok(b) => b,
            Err(e) => return anthropic_bad_request(&e),
        };
        let upstream = match state
            .upstream
            .post_stream(
                &channel.endpoint,
                &channel.authority,
                &channel.path,
                channel.auth.as_ref(),
                &channel.extra_headers,
                out_body,
            )
            .await
        {
            Ok(u) => u,
            Err(e) => {
                channel.mark_cooldown(COOLDOWN_TRANSIENT);
                last_failure = ChannelFailure::Transport {
                    message: err_chain(&e),
                    channel_id: channel.id,
                    provider_id: channel.provider_id,
                    upstream_model: channel.upstream_model.clone(),
                };
                continue;
            }
        };
        if should_failover(upstream.status, false) {
            let status = upstream.status;
            let cooldown = if matches!(status, 401..=404) {
                COOLDOWN_QUOTA_OR_AUTH
            } else {
                COOLDOWN_TRANSIENT
            };
            channel.mark_cooldown(cooldown);
            let err_bytes = read_error_body(upstream, state.config.upstream_stall_timeout).await;
            last_failure = ChannelFailure::UpstreamStatus {
                status,
                body: err_bytes,
                channel_id: channel.id,
                provider_id: channel.provider_id,
                upstream_model: channel.upstream_model.clone(),
                base_url: channel.base_url.clone(),
            };
            continue;
        }
        if !(200..300).contains(&upstream.status) {
            let status = upstream.status;
            let err_bytes = read_error_body(upstream, state.config.upstream_stall_timeout).await;
            return ChannelFailure::UpstreamStatus {
                status,
                body: err_bytes,
                channel_id: channel.id,
                provider_id: channel.provider_id,
                upstream_model: channel.upstream_model.clone(),
                base_url: channel.base_url.clone(),
            }
            .into_anthropic_response();
        }
        channel.clear_cooldown();
        let usage_ctx = UsageReportContext::from_state(
            &state,
            &key,
            &channel,
            &request.model,
            started_at,
            upstream.status,
        );
        if request.stream {
            let guarded = StallGuard::new(upstream.body, state.config.upstream_stall_timeout);
            let observed = UsageStream::new(guarded, true, usage_ctx);
            let anthropic_sse =
                translate_openai_sse_to_anthropic_stream(observed, request.model.clone());
            let with_ka = StallGuard::with_keepalive(
                anthropic_sse,
                Duration::ZERO,
                state.config.sse_keepalive_interval,
            );
            return attach_channel_headers(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
                    .header(axum::http::header::CACHE_CONTROL, "no-cache")
                    .header("x-accel-buffering", "no"),
                &channel,
            )
            .body(Body::from_stream(with_ka))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
        let guarded = StallGuard::new(upstream.body, state.config.upstream_stall_timeout);
        let raw_bytes = match collect_upstream_body(guarded, state.config.max_body_bytes).await {
            Ok(b) => b,
            Err(e) => {
                last_failure = ChannelFailure::Transport {
                    message: e,
                    channel_id: channel.id,
                    provider_id: channel.provider_id,
                    upstream_model: channel.upstream_model.clone(),
                };
                continue;
            }
        };
        match translate_openai_chat_to_anthropic_json(&raw_bytes, &request.model) {
            Ok(json_val) => {
                if let Some(ctx) = usage_ctx {
                    ctx.emit_from_json_bytes(&raw_bytes);
                }
                return attach_channel_headers(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(axum::http::header::CONTENT_TYPE, "application/json"),
                    &channel,
                )
                .body(Body::from(
                    serde_json::to_vec(&json_val).unwrap_or_default(),
                ))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
            Err(e) => {
                last_failure = ChannelFailure::Transport {
                    message: e,
                    channel_id: channel.id,
                    provider_id: channel.provider_id,
                    upstream_model: channel.upstream_model.clone(),
                };
            }
        }
    }
    last_failure.into_anthropic_response()
}

/// OpenAI `/v1/responses` endpoint with automatic fallback to `/v1/chat/completions`
/// when an upstream provider only implements Chat Completions (`404`/`405`).
pub async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Response {
    let Some(token) = bearer_key(&headers) else {
        return unauthorized();
    };
    let Some(key) = state.store.authenticate(token) else {
        return unauthorized();
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": { "message": "request body must be JSON", "type": "invalid_request_error" }
                })),
            )
                .into_response();
        }
    };
    let request = match parse_responses_request(&payload) {
        Ok(r) => r,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": { "message": msg, "type": "invalid_request_error" }
                })),
            )
                .into_response();
        }
    };
    let Some(channels) = state.store.channels_for(&key, &request.model) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": { "message": format!("model {} is not available", request.model), "type": "invalid_request_error" }
            })),
        )
            .into_response();
    };

    let started_at = std::time::Instant::now();
    let mut last_failure = ChannelFailure::Transport {
        message: "no channel available".to_string(),
        channel_id: 0,
        provider_id: 0,
        upstream_model: String::new(),
    };
    for channel in channels {
        let out_body = match build_openai_responses_body(&channel, &request) {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": { "message": e, "type": "invalid_request_error" } })),
                )
                    .into_response();
            }
        };
        let resp_ep = responses_url(&channel.base_url);
        let (resp_auth, resp_path) = h2c_parts(&resp_ep)
            .unwrap_or_else(|| (channel.authority.clone(), "/v1/responses".into()));
        let upstream = match state
            .upstream
            .post_stream(
                &resp_ep,
                &resp_auth,
                &resp_path,
                channel.auth.as_ref(),
                &channel.extra_headers,
                out_body,
            )
            .await
        {
            Ok(u) => u,
            Err(e) => {
                channel.mark_cooldown(COOLDOWN_TRANSIENT);
                last_failure = ChannelFailure::Transport {
                    message: err_chain(&e),
                    channel_id: channel.id,
                    provider_id: channel.provider_id,
                    upstream_model: channel.upstream_model.clone(),
                };
                continue;
            }
        };

        if matches!(upstream.status, 404 | 405) {
            let _ = read_error_body(upstream, state.config.upstream_stall_timeout).await;
            match try_responses_via_chat(&state, &key, &channel, &request, started_at).await {
                Ok(resp) => return resp,
                Err(failure) => {
                    last_failure = failure;
                    continue;
                }
            }
        }

        if should_failover(upstream.status, false) {
            let status = upstream.status;
            let cooldown = if matches!(status, 401..=404) {
                COOLDOWN_QUOTA_OR_AUTH
            } else {
                COOLDOWN_TRANSIENT
            };
            channel.mark_cooldown(cooldown);
            let err_bytes = read_error_body(upstream, state.config.upstream_stall_timeout).await;
            last_failure = ChannelFailure::UpstreamStatus {
                status,
                body: err_bytes,
                channel_id: channel.id,
                provider_id: channel.provider_id,
                upstream_model: channel.upstream_model.clone(),
                base_url: channel.base_url.clone(),
            };
            continue;
        }
        channel.clear_cooldown();
        let usage_ctx = UsageReportContext::from_state(
            &state,
            &key,
            &channel,
            &request.model,
            started_at,
            upstream.status,
        );
        return forward_response_with_channel(
            upstream,
            &channel,
            state.config.upstream_stall_timeout,
            state.config.sse_keepalive_interval,
            usage_ctx,
        );
    }
    last_failure.into_openai_response()
}

async fn try_responses_via_chat(
    state: &AppState,
    key: &crate::store::KeyRecord,
    channel: &Channel,
    request: &ParsedResponsesRequest,
    started_at: std::time::Instant,
) -> Result<Response, ChannelFailure> {
    let chat_body = build_openai_chat_body_from_responses(channel, request).map_err(|e| {
        ChannelFailure::Transport {
            message: e,
            channel_id: channel.id,
            provider_id: channel.provider_id,
            upstream_model: channel.upstream_model.clone(),
        }
    })?;
    let upstream = state
        .upstream
        .post_stream(
            &channel.endpoint,
            &channel.authority,
            &channel.path,
            channel.auth.as_ref(),
            &channel.extra_headers,
            chat_body,
        )
        .await
        .map_err(|e| {
            channel.mark_cooldown(COOLDOWN_TRANSIENT);
            ChannelFailure::Transport {
                message: err_chain(&e),
                channel_id: channel.id,
                provider_id: channel.provider_id,
                upstream_model: channel.upstream_model.clone(),
            }
        })?;
    if should_failover(upstream.status, false) {
        let status = upstream.status;
        let cooldown = if matches!(status, 401..=404) {
            COOLDOWN_QUOTA_OR_AUTH
        } else {
            COOLDOWN_TRANSIENT
        };
        channel.mark_cooldown(cooldown);
        let err_bytes = read_error_body(upstream, state.config.upstream_stall_timeout).await;
        return Err(ChannelFailure::UpstreamStatus {
            status,
            body: err_bytes,
            channel_id: channel.id,
            provider_id: channel.provider_id,
            upstream_model: channel.upstream_model.clone(),
            base_url: channel.base_url.clone(),
        });
    }
    if !(200..300).contains(&upstream.status) {
        let status = upstream.status;
        let err_bytes = read_error_body(upstream, state.config.upstream_stall_timeout).await;
        return Ok(ChannelFailure::UpstreamStatus {
            status,
            body: err_bytes,
            channel_id: channel.id,
            provider_id: channel.provider_id,
            upstream_model: channel.upstream_model.clone(),
            base_url: channel.base_url.clone(),
        }
        .into_openai_response());
    }
    channel.clear_cooldown();
    let usage_ctx = UsageReportContext::from_state(
        state,
        key,
        channel,
        &request.model,
        started_at,
        upstream.status,
    );
    if request.stream {
        let guarded = StallGuard::new(upstream.body, state.config.upstream_stall_timeout);
        let observed = UsageStream::new(guarded, true, usage_ctx);
        let resp_sse = translate_openai_sse_to_responses_stream(observed, request.model.clone());
        let with_ka = StallGuard::with_keepalive(
            resp_sse,
            Duration::ZERO,
            state.config.sse_keepalive_interval,
        );
        return Ok(attach_channel_headers(
            Response::builder()
                .status(StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
                .header(axum::http::header::CACHE_CONTROL, "no-cache")
                .header("x-accel-buffering", "no"),
            channel,
        )
        .body(Body::from_stream(with_ka))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()));
    }
    let guarded = StallGuard::new(upstream.body, state.config.upstream_stall_timeout);
    let raw_bytes = collect_upstream_body(guarded, state.config.max_body_bytes)
        .await
        .map_err(|e| ChannelFailure::Transport {
            message: e,
            channel_id: channel.id,
            provider_id: channel.provider_id,
            upstream_model: channel.upstream_model.clone(),
        })?;
    let json_val =
        translate_openai_chat_to_responses_json(&raw_bytes, &request.model).map_err(|e| {
            ChannelFailure::Transport {
                message: e,
                channel_id: channel.id,
                provider_id: channel.provider_id,
                upstream_model: channel.upstream_model.clone(),
            }
        })?;
    if let Some(ctx) = usage_ctx {
        ctx.emit_from_json_bytes(&raw_bytes);
    }
    Ok(attach_channel_headers(
        Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "application/json"),
        channel,
    )
    .body(Body::from(
        serde_json::to_vec(&json_val).unwrap_or_default(),
    ))
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

/// OpenAI `/v1/embeddings` endpoint with zero-DOM `"model"` splicing and failover.
pub async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Response {
    let Some(token) = bearer_key(&headers) else {
        return unauthorized();
    };
    let Some(key) = state.store.authenticate(token) else {
        return unauthorized();
    };
    let model_ref: ModelRef = match serde_json::from_slice(&body) {
        Ok(v) => v,
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
    let Some(model) = model_ref.model.map(str::trim).filter(|m| !m.is_empty()) else {
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

    let started_at = std::time::Instant::now();
    let mut last_failure = ChannelFailure::Transport {
        message: "no channel available".to_string(),
        channel_id: 0,
        provider_id: 0,
        upstream_model: String::new(),
    };
    let mut plan: Option<RewritePlan> = None;
    for channel in channels {
        let out_body = match outbound_body(model, &channel.upstream_model, &body, &mut plan) {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": { "message": e, "type": "invalid_request_error" } })),
                )
                    .into_response();
            }
        };
        let emb_ep = embeddings_url(&channel.base_url);
        let (emb_auth, emb_path) = h2c_parts(&emb_ep)
            .unwrap_or_else(|| (channel.authority.clone(), "/v1/embeddings".into()));
        let upstream = match state
            .upstream
            .post_stream(
                &emb_ep,
                &emb_auth,
                &emb_path,
                channel.auth.as_ref(),
                &channel.extra_headers,
                out_body,
            )
            .await
        {
            Ok(u) => u,
            Err(e) => {
                channel.mark_cooldown(COOLDOWN_TRANSIENT);
                last_failure = ChannelFailure::Transport {
                    message: err_chain(&e),
                    channel_id: channel.id,
                    provider_id: channel.provider_id,
                    upstream_model: channel.upstream_model.clone(),
                };
                continue;
            }
        };
        if should_failover(upstream.status, false) {
            let status = upstream.status;
            let cooldown = if matches!(status, 401..=404) {
                COOLDOWN_QUOTA_OR_AUTH
            } else {
                COOLDOWN_TRANSIENT
            };
            channel.mark_cooldown(cooldown);
            let err_bytes = read_error_body(upstream, state.config.upstream_stall_timeout).await;
            last_failure = ChannelFailure::UpstreamStatus {
                status,
                body: err_bytes,
                channel_id: channel.id,
                provider_id: channel.provider_id,
                upstream_model: channel.upstream_model.clone(),
                base_url: channel.base_url.clone(),
            };
            continue;
        }
        channel.clear_cooldown();
        let usage_ctx = UsageReportContext::from_state(
            &state,
            &key,
            &channel,
            model,
            started_at,
            upstream.status,
        );
        return forward_response_with_channel(
            upstream,
            &channel,
            state.config.upstream_stall_timeout,
            state.config.sse_keepalive_interval,
            usage_ctx,
        );
    }
    last_failure.into_openai_response()
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

/// Cooldown duration applied to a channel that returned a quota/balance or
/// credential/route error (`401`, `402`, `403`, `404`), avoiding repeated
/// upstream round-trips to an exhausted or revoked key on every request.
const COOLDOWN_QUOTA_OR_AUTH: Duration = Duration::from_secs(30);

/// Cooldown duration applied to a channel that returned a transient rate-limit
/// (`429`), server error (`5xx`), or transport/header timeout error.
const COOLDOWN_TRANSIENT: Duration = Duration::from_secs(5);

enum ChannelFailure {
    Transport {
        message: String,
        channel_id: i64,
        provider_id: i64,
        upstream_model: String,
    },
    UpstreamStatus {
        status: u16,
        body: Bytes,
        channel_id: i64,
        provider_id: i64,
        upstream_model: String,
        base_url: String,
    },
}

/// Map `502`/`504` to `503 Service Unavailable` when returning a terminal
/// JSON error payload so reverse proxies / CDNs (such as Cloudflare Orange
/// Cloud) pass the JSON body through instead of replacing `502`/`504` with a
/// generic gateway error page.
fn cloudflare_safe_error_status(status: u16) -> StatusCode {
    match status {
        400..=499 => StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
        500 | 503 => StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        502 | 504 => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    }
}

impl ChannelFailure {
    fn into_openai_response(self) -> Response {
        match self {
            Self::Transport {
                message,
                channel_id,
                provider_id,
                upstream_model,
            } => attach_failure_headers(
                Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .header(axum::http::header::CONTENT_TYPE, "application/json"),
                channel_id,
                provider_id,
                &upstream_model,
            )
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "error": { "message": message, "type": "upstream_error" }
                }))
                .unwrap_or_default(),
            ))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()),
            Self::UpstreamStatus {
                status,
                body,
                channel_id,
                provider_id,
                upstream_model,
                base_url,
            } => {
                let http_status = cloudflare_safe_error_status(status);
                let out_body = if serde_json::from_slice::<Value>(&body).is_ok() {
                    body
                } else {
                    let text = std::str::from_utf8(&body)
                        .ok()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("upstream {base_url} returned {status}"));
                    Bytes::from(
                        serde_json::to_vec(&json!({
                            "error": { "message": text, "type": "upstream_error" }
                        }))
                        .unwrap_or_default(),
                    )
                };
                attach_failure_headers(
                    Response::builder()
                        .status(http_status)
                        .header(axum::http::header::CONTENT_TYPE, "application/json"),
                    channel_id,
                    provider_id,
                    &upstream_model,
                )
                .body(Body::from(out_body))
                .unwrap_or_else(|_| http_status.into_response())
            }
        }
    }

    fn into_anthropic_response(self) -> Response {
        match self {
            Self::Transport {
                message,
                channel_id,
                provider_id,
                upstream_model,
            } => {
                let body = format_anthropic_error_body(502, b"", &message);
                attach_failure_headers(
                    Response::builder()
                        .status(StatusCode::BAD_GATEWAY)
                        .header(axum::http::header::CONTENT_TYPE, "application/json"),
                    channel_id,
                    provider_id,
                    &upstream_model,
                )
                .body(Body::from(body))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
            }
            Self::UpstreamStatus {
                status,
                body,
                channel_id,
                provider_id,
                upstream_model,
                base_url,
            } => {
                let http_status = cloudflare_safe_error_status(status);
                let fallback = format!("upstream {base_url} returned {status}");
                let out_body = format_anthropic_error_body(status, &body, &fallback);
                attach_failure_headers(
                    Response::builder()
                        .status(http_status)
                        .header(axum::http::header::CONTENT_TYPE, "application/json"),
                    channel_id,
                    provider_id,
                    &upstream_model,
                )
                .body(Body::from(out_body))
                .unwrap_or_else(|_| http_status.into_response())
            }
        }
    }
}

fn attach_channel_headers(
    builder: axum::http::response::Builder,
    channel: &Channel,
) -> axum::http::response::Builder {
    attach_failure_headers(
        builder,
        channel.id,
        channel.provider_id,
        &channel.upstream_model,
    )
}

fn attach_failure_headers(
    mut builder: axum::http::response::Builder,
    channel_id: i64,
    provider_id: i64,
    upstream_model: &str,
) -> axum::http::response::Builder {
    if channel_id > 0 {
        builder = builder
            .header("x-xlite-channel-id", channel_id.to_string())
            .header("x-xlite-provider-id", provider_id.to_string());
    }
    if !upstream_model.is_empty()
        && let Ok(val) = axum::http::HeaderValue::from_str(upstream_model)
    {
        builder = builder.header("x-xlite-upstream-model", val);
    }
    builder
}

async fn try_channel(
    state: &AppState,
    key: &crate::store::KeyRecord,
    channel: &Channel,
    model: &str,
    original: &Bytes,
    plan: &mut Option<RewritePlan>,
    started_at: std::time::Instant,
) -> Result<Response, ChannelFailure> {
    let body = outbound_body(model, &channel.upstream_model, original, plan).map_err(|e| {
        ChannelFailure::Transport {
            message: e,
            channel_id: channel.id,
            provider_id: channel.provider_id,
            upstream_model: channel.upstream_model.clone(),
        }
    })?;
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
        .map_err(|e| {
            channel.mark_cooldown(COOLDOWN_TRANSIENT);
            ChannelFailure::Transport {
                message: err_chain(&e),
                channel_id: channel.id,
                provider_id: channel.provider_id,
                upstream_model: channel.upstream_model.clone(),
            }
        })?;
    if should_failover(upstream.status, false) {
        // Read the status before draining: `read_error_body` consumes `upstream`.
        let status = upstream.status;
        let cooldown = if matches!(status, 401..=404) {
            COOLDOWN_QUOTA_OR_AUTH
        } else {
            COOLDOWN_TRANSIENT
        };
        channel.mark_cooldown(cooldown);
        let err_bytes = read_error_body(upstream, state.config.upstream_stall_timeout).await;
        return Err(ChannelFailure::UpstreamStatus {
            status,
            body: err_bytes,
            channel_id: channel.id,
            provider_id: channel.provider_id,
            upstream_model: channel.upstream_model.clone(),
            base_url: channel.base_url.clone(),
        });
    }
    channel.clear_cooldown();
    let usage_ctx =
        UsageReportContext::from_state(state, key, channel, model, started_at, upstream.status);
    Ok(forward_response_with_channel(
        upstream,
        channel,
        state.config.upstream_stall_timeout,
        state.config.sse_keepalive_interval,
        usage_ctx,
    ))
}

fn is_event_stream(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
}

fn forward_response_with_channel(
    upstream: UpstreamResponse,
    channel: &Channel,
    stall_timeout: Duration,
    sse_keepalive_interval: Duration,
    usage_ctx: Option<UsageReportContext>,
) -> Response {
    let builder = attach_channel_headers(
        Response::builder()
            .status(StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY)),
        channel,
    );
    finish_forward_response(
        builder,
        upstream,
        stall_timeout,
        sse_keepalive_interval,
        usage_ctx,
    )
}

fn finish_forward_response(
    mut builder: axum::http::response::Builder,
    upstream: UpstreamResponse,
    stall_timeout: Duration,
    sse_keepalive_interval: Duration,
    usage_ctx: Option<UsageReportContext>,
) -> Response {
    let content_type = upstream
        .content_type
        .unwrap_or_else(|| "application/json".into());
    let sse = is_event_stream(&content_type);
    builder = builder.header(axum::http::header::CONTENT_TYPE, content_type);
    if sse {
        // Prevent downstream reverse proxies (Nginx, Envoy, Cloudflare, CDN) from
        // buffering SSE chunks or caching long-running event streams.
        builder = builder
            .header(axum::http::header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no");
    }

    let keepalive = if sse {
        sse_keepalive_interval
    } else {
        Duration::ZERO
    };
    let observed = UsageStream::new(upstream.body, sse, usage_ctx);
    let body = StallGuard::with_keepalive(observed, stall_timeout, keepalive);
    builder.body(Body::from_stream(body)).unwrap_or_else(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "message": "failed to build response" } })),
        )
            .into_response()
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct UsageStats {
    input_tokens: u64,
    output_tokens: u64,
    cache_hit_tokens: u64,
    total_tokens: u64,
}

impl UsageStats {
    fn merge_usage_obj(&mut self, usage: &Value) {
        if let Some(inp) = usage
            .get("prompt_tokens")
            .or_else(|| usage.get("input_tokens"))
            .and_then(Value::as_u64)
            && inp > 0
        {
            self.input_tokens = inp;
        }
        if let Some(out) = usage
            .get("completion_tokens")
            .or_else(|| usage.get("output_tokens"))
            .and_then(Value::as_u64)
            && out > 0
        {
            self.output_tokens = out;
        }
        let cached = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .or_else(|| {
                usage
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })
            .or_else(|| usage.get("cache_read_input_tokens"))
            .or_else(|| usage.get("prompt_cache_hit_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if cached > 0 {
            self.cache_hit_tokens = cached;
        }
        if let Some(tot) = usage.get("total_tokens").and_then(Value::as_u64)
            && tot > 0
        {
            self.total_tokens = tot;
        } else if self.input_tokens > 0 || self.output_tokens > 0 {
            self.total_tokens = self.input_tokens + self.output_tokens;
        }
    }

    fn inspect_json_value(&mut self, val: &Value) {
        if let Some(u) = val.get("usage") {
            self.merge_usage_obj(u);
        }
        if let Some(u) = val.get("response").and_then(|r| r.get("usage")) {
            self.merge_usage_obj(u);
        }
        if let Some(u) = val.get("message").and_then(|m| m.get("usage")) {
            self.merge_usage_obj(u);
        }
    }

    fn inspect_json_bytes(&mut self, raw: &[u8]) {
        if raw.windows(7).any(|w| w == b"\"usage\"")
            && let Ok(val) = serde_json::from_slice::<Value>(raw)
        {
            self.inspect_json_value(&val);
        }
    }

    fn inspect_sse_line(&mut self, line: &[u8]) {
        let Ok(text) = std::str::from_utf8(line) else {
            return;
        };
        let trimmed = text.trim();
        let data = trimmed
            .strip_prefix("data:")
            .map(str::trim)
            .unwrap_or(trimmed);
        if data.starts_with('{')
            && data.contains("\"usage\"")
            && let Ok(val) = serde_json::from_str::<Value>(data)
        {
            self.inspect_json_value(&val);
        }
    }
}

#[derive(Clone)]
struct WebhookTarget {
    url: String,
    auth: Option<(http::HeaderName, http::HeaderValue)>,
}

fn usage_webhook_target() -> Option<&'static WebhookTarget> {
    static TARGET: std::sync::OnceLock<Option<WebhookTarget>> = std::sync::OnceLock::new();
    TARGET
        .get_or_init(|| {
            let url = std::env::var("XLITE_USAGE_WEBHOOK_URL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())?;
            let token = std::env::var("XLITE_USAGE_WEBHOOK_TOKEN")
                .ok()
                .map(|v| v.trim().to_string())
                .unwrap_or_default();
            let auth = crate::upstream::build_auth("bearer", "authorization", &token)
                .ok()
                .flatten();
            Some(WebhookTarget { url, auth })
        })
        .as_ref()
}

#[derive(Clone)]
struct UsageReportContext {
    upstream: crate::upstream::Upstream,
    target: WebhookTarget,
    model: String,
    upstream_model: String,
    api_key_name: String,
    provider_name: String,
    started_at: std::time::Instant,
    status: u16,
}

impl UsageReportContext {
    fn from_state(
        state: &AppState,
        key: &crate::store::KeyRecord,
        channel: &Channel,
        model: &str,
        started_at: std::time::Instant,
        status: u16,
    ) -> Option<Self> {
        let target = usage_webhook_target()?.clone();
        if !(200..300).contains(&status) {
            return None;
        }
        let provider_name = state
            .store
            .provider_name(channel.provider_id)
            .unwrap_or_default();
        Some(Self {
            upstream: state.upstream.clone(),
            target,
            model: model.to_string(),
            upstream_model: channel.upstream_model.clone(),
            api_key_name: key.name.clone(),
            provider_name,
            started_at,
            status,
        })
    }

    fn emit(self, stats: UsageStats) {
        let latency_ms = self.started_at.elapsed().as_millis() as u64;
        let payload = json!({
            "model": self.model,
            "upstream_model": self.upstream_model,
            "api_key_name": self.api_key_name,
            "provider_name": self.provider_name,
            "input_tokens": stats.input_tokens,
            "output_tokens": stats.output_tokens,
            "cache_hit_tokens": stats.cache_hit_tokens,
            "tokens": stats.total_tokens,
            "latency": latency_ms,
            "status": self.status,
        });
        let Ok(body) = serde_json::to_vec(&payload).map(Bytes::from) else {
            return;
        };
        tokio::spawn(async move {
            let _ = self
                .upstream
                .post_json_h1(&self.target.url, self.target.auth.as_ref(), body)
                .await;
        });
    }

    fn emit_from_json_bytes(self, raw_bytes: &[u8]) {
        let mut stats = UsageStats::default();
        stats.inspect_json_bytes(raw_bytes);
        self.emit(stats);
    }
}

struct UsageStream<S> {
    inner: S,
    ctx: Option<UsageReportContext>,
    is_sse: bool,
    buf: Vec<u8>,
    stats: UsageStats,
}

impl<S> UsageStream<S> {
    fn new(inner: S, is_sse: bool, ctx: Option<UsageReportContext>) -> Self {
        Self {
            inner,
            ctx,
            is_sse,
            buf: Vec::new(),
            stats: UsageStats::default(),
        }
    }

    fn feed_chunk(&mut self, chunk: &[u8]) {
        if self.ctx.is_none() {
            return;
        }
        if self.is_sse {
            const MAX_SSE_LINE_BUF: usize = 256 * 1024;
            if self.buf.len() + chunk.len() > MAX_SSE_LINE_BUF {
                self.buf.clear();
            }
            self.buf.extend_from_slice(chunk);
            while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line = &self.buf[..pos];
                self.stats.inspect_sse_line(line);
                self.buf.drain(..=pos);
            }
        } else {
            const MAX_JSON_BUF: usize = 512 * 1024;
            let remaining = MAX_JSON_BUF.saturating_sub(self.buf.len());
            if remaining > 0 {
                let take = chunk.len().min(remaining);
                self.buf.extend_from_slice(&chunk[..take]);
            }
        }
    }

    fn finish(&mut self) {
        let Some(ctx) = self.ctx.take() else {
            return;
        };
        if self.is_sse {
            if !self.buf.is_empty() {
                self.stats.inspect_sse_line(&self.buf);
                self.buf.clear();
            }
        } else if !self.buf.is_empty() {
            self.stats.inspect_json_bytes(&self.buf);
            self.buf.clear();
        }
        ctx.emit(self.stats);
    }
}

impl<S> Drop for UsageStream<S> {
    fn drop(&mut self) {
        self.finish();
    }
}

impl<S> Stream for UsageStream<S>
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Stream::poll_next(Pin::new(&mut this.inner), cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                this.feed_chunk(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => {
                this.finish();
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                this.finish();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// End a response body with a `TimedOut` error when the upstream has produced
/// no bytes for `timeout`, so a hung upstream cannot park a gateway stream (and
/// its pool lease) until the client gives up.
///
/// When `keepalive_interval` is non-zero (used on `text/event-stream` responses),
/// `StallGuard` also emits `: keep-alive\n\n` SSE comment heartbeats during
/// silent upstream windows without resetting the upstream `timeout` deadline.
struct StallGuard<S> {
    inner: S,
    timeout: Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    keepalive_interval: Duration,
    keepalive_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    fired: bool,
}

impl<S> StallGuard<S> {
    fn new(inner: S, timeout: Duration) -> Self {
        Self::with_keepalive(inner, timeout, Duration::ZERO)
    }

    fn with_keepalive(inner: S, timeout: Duration, keepalive_interval: Duration) -> Self {
        let sleep = (!timeout.is_zero()).then(|| Box::pin(tokio::time::sleep(timeout)));
        Self {
            inner,
            timeout,
            sleep,
            keepalive_interval,
            // Allocated lazily only if the upstream stream actually yields `Poll::Pending`.
            keepalive_sleep: None,
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
                let now = if this.sleep.is_some() || this.keepalive_sleep.is_some() {
                    Some(tokio::time::Instant::now())
                } else {
                    None
                };
                if let (Some(sleep), Some(now)) = (this.sleep.as_mut(), now) {
                    sleep.as_mut().reset(now + this.timeout);
                }
                if let (Some(ka), Some(now)) = (this.keepalive_sleep.as_mut(), now) {
                    ka.as_mut().reset(now + this.keepalive_interval);
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
                    this.keepalive_sleep = None;
                    return Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("upstream produced no data for {}s", this.timeout.as_secs()),
                    ))));
                }
                if !this.keepalive_interval.is_zero() {
                    let interval = this.keepalive_interval;
                    let ka = this
                        .keepalive_sleep
                        .get_or_insert_with(|| Box::pin(tokio::time::sleep(interval)));
                    if ka.as_mut().poll(cx).is_ready() {
                        ka.as_mut().reset(tokio::time::Instant::now() + interval);
                        return Poll::Ready(Some(Ok(Bytes::from_static(SSE_KEEPALIVE_FRAME))));
                    }
                }
                Poll::Pending
            }
        }
    }
}

/// Read an upstream error body before failing over, capped in both size and
/// per-chunk silence so a broken upstream streaming an unbounded or hung error
/// body cannot stall the failover while still preserving structured JSON errors
/// if all candidate channels fail.
async fn read_error_body(upstream: UpstreamResponse, stall_timeout: Duration) -> Bytes {
    read_error_stream(upstream.body, stall_timeout).await
}

async fn read_error_stream<S>(stream: S, stall_timeout: Duration) -> Bytes
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    const MAX_DRAIN: usize = 64 * 1024;
    let mut body = StallGuard::new(stream, stall_timeout);
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(bytes) => {
                let take = bytes.len().min(MAX_DRAIN.saturating_sub(out.len()));
                out.extend_from_slice(&bytes[..take]);
                if out.len() >= MAX_DRAIN {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Bytes::from(out)
}

#[cfg(test)]
async fn drain_stream<S>(stream: S, stall_timeout: Duration)
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    let _ = read_error_stream(stream, stall_timeout).await;
}

fn bearer_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(bearer) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Some(bearer);
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
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

fn anthropic_unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "invalid api key" }
        })),
    )
        .into_response()
}

fn anthropic_bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": message }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

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
        let mut plan = None;
        let out = outbound_body("gpt", "gpt", &original, &mut plan).expect("passthrough");
        assert!(
            std::ptr::eq(out.as_ptr(), original.as_ptr()),
            "must be the same underlying buffer"
        );
        assert!(
            plan.is_none(),
            "no rewrite plan may be built for the fast path"
        );
    }

    #[test]
    fn rename_splices_model_without_building_dom_and_reuses_span() {
        let original = Bytes::from_static(
            br#"{"messages":[{"role":"user","content":"mention \"model\":\"nested\" here","meta":{"model":"ignore-nested"}}],"model":"gpt","x":1}"#,
        );
        let mut plan = None;
        let out = outbound_body("gpt", "upstream-gpt", &original, &mut plan).expect("rename");
        assert!(
            matches!(plan, Some(RewritePlan::Splice(_))),
            "standard top-level model field must use zero-DOM byte splice, got {plan:?}"
        );
        let parsed: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(parsed["model"], "upstream-gpt");
        assert_eq!(parsed["messages"][0]["meta"]["model"], "ignore-nested");
        assert_eq!(parsed["x"], 1);

        // A second attempt (another channel on failover) reuses the cached splice span.
        let out2 =
            outbound_body("gpt", "provider/model-v2", &original, &mut plan).expect("rename 2");
        let parsed2: Value = serde_json::from_slice(&out2).expect("json");
        assert_eq!(parsed2["model"], "provider/model-v2");
        assert_eq!(parsed2["messages"][0]["meta"]["model"], "ignore-nested");
    }

    #[test]
    fn rename_falls_back_to_dom_on_escaped_or_duplicate_model_key() {
        let escaped_key = Bytes::from_static(br#"{"\u006dodel":"gpt","x":1}"#);
        let mut plan = None;
        let out =
            outbound_body("gpt", "upstream-gpt", &escaped_key, &mut plan).expect("escaped rename");
        assert!(
            matches!(plan, Some(RewritePlan::Dom(_))),
            "escaped key must fall back to DOM rewrite, got {plan:?}"
        );
        let parsed: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(parsed["model"], "upstream-gpt");
    }

    #[tokio::test(start_paused = true)]
    async fn stall_guard_emits_sse_keepalive_without_resetting_stall_deadline() {
        let t0 = tokio::time::Instant::now();
        let inner = stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from_static(
            b"data: {\"id\":\"1\"}\n\n",
        ))])
        .chain(stream::pending::<Result<Bytes, std::io::Error>>());

        // Stall timeout = 35s, SSE keep-alive heartbeat = 15s.
        let mut guarded =
            StallGuard::with_keepalive(inner, Duration::from_secs(35), Duration::from_secs(15));

        assert_eq!(
            &guarded.next().await.unwrap().unwrap()[..],
            b"data: {\"id\":\"1\"}\n\n"
        );
        assert_eq!(t0.elapsed(), Duration::ZERO);

        // First keep-alive comment at t = 15s.
        assert_eq!(
            &guarded.next().await.unwrap().unwrap()[..],
            SSE_KEEPALIVE_FRAME
        );
        assert_eq!(t0.elapsed(), Duration::from_secs(15));

        // Second keep-alive comment at t = 30s.
        assert_eq!(
            &guarded.next().await.unwrap().unwrap()[..],
            SSE_KEEPALIVE_FRAME
        );
        assert_eq!(t0.elapsed(), Duration::from_secs(30));

        // Upstream stall timeout still fires at t = 35s (not reset by keep-alive comments).
        let err = guarded
            .next()
            .await
            .expect("stall timeout must fire at 35s")
            .expect_err("must be TimedOut");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(t0.elapsed(), Duration::from_secs(35));
    }

    #[test]
    fn is_event_stream_recognizes_sse_content_type_parameters() {
        assert!(is_event_stream("text/event-stream"));
        assert!(is_event_stream("text/event-stream; charset=utf-8"));
        assert!(is_event_stream("TEXT/EVENT-STREAM;charset=UTF-8"));
        assert!(!is_event_stream("application/json"));
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

    #[tokio::test]
    async fn failover_on_402_and_401_cools_down_bad_channels_so_subsequent_requests_skip_them() {
        use crate::catalog::{Catalog, yaml};
        use crate::config::Config;
        use crate::store::Store;
        use crate::upstream::Upstream;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        async fn spawn_mock(
            status: u16,
            body: &'static str,
        ) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let hits_bg = Arc::clone(&hits);
            tokio::spawn(async move {
                while let Ok((mut s, _)) = listener.accept().await {
                    let hits_conn = Arc::clone(&hits_bg);
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        while let Ok(n) = s.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                            hits_conn.fetch_add(1, Ordering::SeqCst);
                            let resp = format!(
                                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            );
                            if s.write_all(resp.as_bytes()).await.is_err() {
                                break;
                            }
                            let _ = s.flush().await;
                        }
                    });
                }
            });
            (addr, hits)
        }

        // Tier 0: returns 402 Payment Required (quota exhausted)
        // Tier 1: returns 401 Unauthorized (revoked key)
        // Tier 2: returns 200 OK
        let (addr_402, hits_402) = spawn_mock(402, r#"{"error":"insufficient_balance"}"#).await;
        let (addr_401, hits_401) = spawn_mock(401, r#"{"error":"invalid_api_key"}"#).await;
        let (addr_200, hits_200) = spawn_mock(200, r#"{"id":"chatcmpl-ok"}"#).await;

        let cfg = Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: false,
            admin_password: "admin".into(),
            session_secret: None,
            max_conns_per_host: 4,
            upstream_stream_cap: 250,
            catalog_reload_secs: 0,
            upstream_stall_timeout: Duration::from_secs(5),
            upstream_header_timeout: Duration::from_secs(5),
            sse_keepalive_interval: Duration::ZERO,
            max_body_bytes: crate::config::DEFAULT_MAX_BODY_BYTES,
        };
        let catalog = Catalog::connect(&cfg).await.unwrap();
        let yaml_doc = format!(
            r#"providers:
  - name: p402
    base_url: http://{addr_402}
    api_key: sk-1
    enabled: true
  - name: p401
    base_url: http://{addr_401}
    api_key: sk-2
    enabled: true
  - name: p200
    base_url: http://{addr_200}
    api_key: sk-3
    enabled: true
models:
  - name: mimo-v2-pro
    enabled: true
    channels:
      - provider: p402
        upstream_model: mimo-v2-pro
        weight: 1
        priority: 0
        enabled: true
      - provider: p401
        upstream_model: mimo-v2-pro
        weight: 1
        priority: 1
        enabled: true
      - provider: p200
        upstream_model: mimo-v2-pro
        weight: 1
        priority: 2
        enabled: true
api_keys:
  - name: client
    key: sk-client
    models: []
    enabled: true
"#
        );
        yaml::import_yaml(&catalog, &yaml_doc, yaml::ImportMode::Replace)
            .await
            .unwrap();
        let store = Store::new(catalog);
        store.reload().await.unwrap();
        let state = AppState {
            store,
            sessions: crate::auth::Sessions::new("secret", None),
            admin_password: "admin".into(),
            upstream: Upstream::new(&cfg),
            config: cfg,
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer sk-client"),
        );

        for _ in 0..3 {
            let resp = chat_completions(
                State(state.clone()),
                headers.clone(),
                Bytes::from_static(br#"{"model":"mimo-v2-pro","messages":[]}"#),
            )
            .await
            .into_response();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        assert_eq!(
            hits_402.load(Ordering::SeqCst),
            1,
            "402 channel must only be hit on the first request and skipped during cooldown"
        );
        assert_eq!(
            hits_401.load(Ordering::SeqCst),
            1,
            "401 channel must fail over on the first request and be skipped during cooldown"
        );
        assert_eq!(
            hits_200.load(Ordering::SeqCst),
            3,
            "healthy backup channel must serve all 3 requests"
        );
    }

    #[tokio::test]
    async fn all_channels_failing_with_402_preserves_402_status_and_json_body() {
        use crate::catalog::{Catalog, yaml};
        use crate::config::Config;
        use crate::store::Store;
        use crate::upstream::Upstream;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    if s.read(&mut buf).await.unwrap_or(0) > 0 {
                        let body = r#"{"error":{"code":"api_error","message":"insufficient balance","type":"api_error"}}"#;
                        let resp = format!(
                            "HTTP/1.1 402 Payment Required\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = s.write_all(resp.as_bytes()).await;
                    }
                });
            }
        });

        let cfg = Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: false,
            admin_password: "admin".into(),
            session_secret: None,
            max_conns_per_host: 4,
            upstream_stream_cap: 250,
            catalog_reload_secs: 0,
            upstream_stall_timeout: Duration::from_secs(5),
            upstream_header_timeout: Duration::from_secs(5),
            sse_keepalive_interval: Duration::ZERO,
            max_body_bytes: crate::config::DEFAULT_MAX_BODY_BYTES,
        };
        let catalog = Catalog::connect(&cfg).await.unwrap();
        let yaml_doc = format!(
            r#"providers:
  - name: p402
    base_url: http://{addr}
    api_key: sk-1
    enabled: true
models:
  - name: gpt-5.4
    enabled: true
    channels:
      - provider: p402
        upstream_model: gpt-5.4-up
        weight: 1
        priority: 0
        enabled: true
api_keys:
  - name: client
    key: sk-client
    models: []
    enabled: true
"#
        );
        yaml::import_yaml(&catalog, &yaml_doc, yaml::ImportMode::Replace)
            .await
            .unwrap();
        let store = Store::new(catalog);
        store.reload().await.unwrap();
        let state = AppState {
            store,
            sessions: crate::auth::Sessions::new("secret", None),
            admin_password: "admin".into(),
            upstream: Upstream::new(&cfg),
            config: cfg,
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer sk-client"),
        );

        let resp = chat_completions(
            State(state),
            headers,
            Bytes::from_static(br#"{"model":"gpt-5.4","messages":[]}"#),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(
            resp.headers()
                .get("x-xlite-upstream-model")
                .and_then(|v| v.to_str().ok()),
            Some("gpt-5.4-up")
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["error"]["message"], "insufficient balance");
    }

    #[tokio::test]
    async fn multi_protocol_messages_responses_and_embeddings_work_end_to_end() {
        use crate::catalog::{Catalog, yaml};
        use crate::config::Config;
        use crate::store::Store;
        use crate::upstream::Upstream;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let req_str = String::from_utf8_lossy(&buf[..n]);
                        let first_line = req_str.lines().next().unwrap_or("");
                        if first_line.contains("POST /v1/responses ") {
                            // Return 404 on /v1/responses to trigger automatic fallback to /v1/chat/completions
                            let body = r#"{"error":"not found"}"#;
                            let resp = format!(
                                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = s.write_all(resp.as_bytes()).await;
                        } else if first_line.contains("POST /v1/embeddings ") {
                            let body = r#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],"model":"emb-up","usage":{"prompt_tokens":3,"total_tokens":3}}"#;
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = s.write_all(resp.as_bytes()).await;
                        } else if req_str.contains("\"stream\":true") {
                            let sse = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think...\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"HELLO\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{sse}",
                                sse.len()
                            );
                            let _ = s.write_all(resp.as_bytes()).await;
                        } else {
                            let body = r#"{"id":"chatcmpl-1","choices":[{"index":0,"message":{"role":"assistant","content":"PONG"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":1,"total_tokens":5}}"#;
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = s.write_all(resp.as_bytes()).await;
                        }
                        let _ = s.flush().await;
                    }
                });
            }
        });

        let cfg = Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: false,
            admin_password: "admin".into(),
            session_secret: None,
            max_conns_per_host: 4,
            upstream_stream_cap: 250,
            catalog_reload_secs: 0,
            upstream_stall_timeout: Duration::from_secs(5),
            upstream_header_timeout: Duration::from_secs(5),
            sse_keepalive_interval: Duration::ZERO,
            max_body_bytes: crate::config::DEFAULT_MAX_BODY_BYTES,
        };
        let catalog = Catalog::connect(&cfg).await.unwrap();
        let yaml_doc = format!(
            r#"providers:
  - name: mock-up
    base_url: http://{addr}
    api_key: sk-up
    enabled: true
models:
  - name: claude-sonnet-4-6
    enabled: true
    channels:
      - provider: mock-up
        upstream_model: claude-sonnet-4-6-up
        weight: 1
        priority: 0
        enabled: true
api_keys:
  - name: client
    key: sk-client
    models: []
    enabled: true
"#
        );
        yaml::import_yaml(&catalog, &yaml_doc, yaml::ImportMode::Replace)
            .await
            .unwrap();
        let store = Store::new(catalog);
        store.reload().await.unwrap();
        let state = AppState {
            store,
            sessions: crate::auth::Sessions::new("secret", None),
            admin_password: "admin".into(),
            upstream: Upstream::new(&cfg),
            config: cfg,
        };

        // 1. Anthropic /v1/messages with x-api-key header (non-streaming)
        let mut anthropic_headers = HeaderMap::new();
        anthropic_headers.insert(
            "x-api-key",
            axum::http::HeaderValue::from_static("sk-client"),
        );
        let resp = anthropic_messages(
            State(state.clone()),
            anthropic_headers.clone(),
            Bytes::from_static(
                br#"{"model":"claude-sonnet-4-6","max_tokens":64,"messages":[{"role":"user","content":"ping"}]}"#,
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let b = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "PONG");
        assert_eq!(v["usage"]["input_tokens"], 4);

        // 2. Anthropic /v1/messages (streaming SSE)
        let resp_stream = anthropic_messages(
            State(state.clone()),
            anthropic_headers.clone(),
            Bytes::from_static(
                br#"{"model":"claude-sonnet-4-6","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"ping"}]}"#,
            ),
        )
        .await;
        assert_eq!(resp_stream.status(), StatusCode::OK);
        let sse_bytes = axum::body::to_bytes(resp_stream.into_body(), 65536)
            .await
            .unwrap();
        let sse_text = String::from_utf8_lossy(&sse_bytes);
        assert!(sse_text.contains("event: message_start"), "{sse_text}");
        assert!(sse_text.contains("\"thinking_delta\""), "{sse_text}");
        assert!(sse_text.contains("\"text_delta\""), "{sse_text}");
        assert!(sse_text.contains("event: message_stop"), "{sse_text}");

        // 3. OpenAI /v1/responses with automatic fallback to /v1/chat/completions on 404
        let resp_responses = responses(
            State(state.clone()),
            anthropic_headers.clone(),
            Bytes::from_static(br#"{"model":"claude-sonnet-4-6","input":"ping"}"#),
        )
        .await;
        assert_eq!(resp_responses.status(), StatusCode::OK);
        let rb = axum::body::to_bytes(resp_responses.into_body(), 65536)
            .await
            .unwrap();
        let rv: Value = serde_json::from_slice(&rb).unwrap();
        assert_eq!(rv["object"], "response");
        assert_eq!(rv["output_text"], "PONG");

        // 4. OpenAI /v1/embeddings
        let resp_emb = embeddings(
            State(state),
            anthropic_headers,
            Bytes::from_static(br#"{"model":"claude-sonnet-4-6","input":"hello"}"#),
        )
        .await;
        assert_eq!(resp_emb.status(), StatusCode::OK);
        let eb = axum::body::to_bytes(resp_emb.into_body(), 65536)
            .await
            .unwrap();
        let ev: Value = serde_json::from_slice(&eb).unwrap();
        assert_eq!(ev["object"], "list");
    }

    #[test]
    fn usage_stats_parses_openai_anthropic_and_responses_payloads() {
        let mut s = UsageStats::default();
        s.inspect_json_bytes(
            br#"{"id":"chatcmpl-1","usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20,"prompt_tokens_details":{"cached_tokens":4}}}"#,
        );
        assert_eq!(
            s,
            UsageStats {
                input_tokens: 12,
                output_tokens: 8,
                cache_hit_tokens: 4,
                total_tokens: 20,
            }
        );

        let mut sse_stats = UsageStats::default();
        sse_stats.inspect_sse_line(
            br#"data: {"type":"response.completed","response":{"usage":{"input_tokens":15,"output_tokens":5,"total_tokens":20,"input_tokens_details":{"cached_tokens":3}}}}"#,
        );
        assert_eq!(
            sse_stats,
            UsageStats {
                input_tokens: 15,
                output_tokens: 5,
                cache_hit_tokens: 3,
                total_tokens: 20,
            }
        );

        let raw = Bytes::from_static(br#"{"model":"glm-5","stream":true,"messages":[]} "#);
        let patched = ensure_stream_include_usage(&raw);
        let parsed: Value = serde_json::from_slice(&patched).expect("valid patched json");
        assert_eq!(parsed["stream_options"]["include_usage"], true);
    }
}
