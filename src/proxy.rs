//! OpenAI-compatible data plane: authenticate, pick a channel, copy bytes.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;

use crate::admin::AppState;
use crate::store::Channel;
use crate::upstream::{UpstreamBody, UpstreamResponse, should_failover};

struct GuardedBody {
    inner: UpstreamBody,
    _permit: Option<OwnedSemaphorePermit>,
}

impl Stream for GuardedBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_next(cx)
    }
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

    let mut permit = if let Some(sem) = &state.in_flight {
        match sem.clone().acquire_owned().await {
            Ok(p) => Some(p),
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "error": { "message": "in-flight limiter closed", "type": "server_error" }
                    })),
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    let mut last_error = "no channel available".to_string();
    for channel in channels {
        match try_channel(&state, &channel, &mut payload, permit).await {
            Ok(response) => return response,
            Err((err, returned_permit)) => {
                last_error = err;
                permit = returned_permit;
            }
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
    permit: Option<OwnedSemaphorePermit>,
) -> Result<Response, (String, Option<OwnedSemaphorePermit>)> {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "model".into(),
            Value::String(channel.upstream_model.clone()),
        );
    }
    let body = match serde_json::to_vec(payload) {
        Ok(b) => b,
        Err(e) => return Err((e.to_string(), permit)),
    };
    let upstream = match state
        .upstream
        .post_stream(&channel.base_url, &channel.api_key, &body)
        .await
    {
        Ok(u) => u,
        Err(e) => return Err((e.to_string(), permit)),
    };
    if should_failover(upstream.status, false) {
        let _ = drain_error_body(upstream).await;
        return Err((
            format!("upstream {} returned retryable status", channel.base_url),
            permit,
        ));
    }
    Ok(forward_response(upstream, permit))
}

fn forward_response(upstream: UpstreamResponse, permit: Option<OwnedSemaphorePermit>) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY));
    let content_type = upstream
        .content_type
        .unwrap_or_else(|| "application/json".into());
    builder = builder.header(axum::http::header::CONTENT_TYPE, content_type);
    let body = GuardedBody {
        inner: upstream.body,
        _permit: permit,
    };
    builder.body(Body::from_stream(body)).unwrap_or_else(|_| {
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
