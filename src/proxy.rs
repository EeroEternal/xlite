//! OpenAI-compatible data plane: authenticate, pick a channel, copy bytes.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

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

pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "gateway": "xlite" }))
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

async fn try_channel(
    state: &AppState,
    channel: &Channel,
    model: &str,
    original: &Bytes,
    dom: &mut Option<Value>,
) -> Result<Response, String> {
    let body = outbound_body(model, &channel.upstream_model, original, dom)?;
    let upstream = state
        .upstream
        .post_stream(
            &channel.endpoint,
            &channel.authority,
            &channel.path,
            &channel.authorization,
            body,
        )
        .await
        .map_err(|e| e.to_string())?;
    if should_failover(upstream.status, false) {
        let _ = drain_error_body(upstream).await;
        return Err(format!(
            "upstream {} returned {}",
            channel.base_url, "retryable status"
        ));
    }
    Ok(forward_response(upstream))
}

fn forward_response(upstream: UpstreamResponse) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY));
    let content_type = upstream
        .content_type
        .unwrap_or_else(|| "application/json".into());
    builder = builder.header(axum::http::header::CONTENT_TYPE, content_type);
    builder
        .body(Body::from_stream(upstream.body))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "message": "failed to build response" } })),
            )
                .into_response()
        })
}

/// Read (and discard) an error body before failing over, capped so a broken
/// upstream streaming an unbounded error body cannot stall the failover.
async fn drain_error_body(upstream: UpstreamResponse) {
    const MAX_DRAIN: usize = 64 * 1024;
    let mut body = upstream.body;
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
