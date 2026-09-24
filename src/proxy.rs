//! OpenAI-compatible data plane: authenticate, pick a channel, copy bytes.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::admin::AppState;
use crate::store::Channel;
use crate::upstream::{UpstreamResponse, should_failover};

pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "gateway": "xlite" }))
}

pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(state.store.pool())
        .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({ "status": "ready" }))).into_response(),
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

    let mut payload: Value = match serde_json::from_slice(&body) {
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
    let Some(model) = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": { "message": "missing model", "type": "invalid_request_error" }
            })),
        )
            .into_response();
    };
    let Some(channels) = state.store.channels_for(&key, &model) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": { "message": format!("model {model} is not available"), "type": "invalid_request_error" }
            })),
        )
            .into_response();
    };

    let mut last_error = "no channel available".to_string();
    for channel in channels {
        match try_channel(&state, &channel, &mut payload).await {
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
    payload: &mut Value,
) -> Result<Response, String> {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "model".into(),
            Value::String(channel.upstream_model.clone()),
        );
    }
    let body = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let upstream = state
        .upstream
        .post_stream(&channel.base_url, &channel.api_key, &body)
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

async fn drain_error_body(upstream: UpstreamResponse) {
    let mut body = upstream.body;
    while body.next().await.is_some() {}
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
}
