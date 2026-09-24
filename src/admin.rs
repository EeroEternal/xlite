//! Control-plane REST API. `{ success, data, message }` envelope.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{self, Sessions};
use crate::catalog::yaml::{self, ImportMode};
use crate::config::Config;
use crate::hash::{generate_api_key, hash_api_key, key_prefix, mask_api_key};
use crate::store::Store;
use crate::upstream::Upstream;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub sessions: Sessions,
    pub admin_password: String,
    pub upstream: Upstream,
    pub config: Config,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/overview", get(overview))
        .route("/api/providers", get(list_providers).post(create_provider))
        .route(
            "/api/providers/{id}",
            get(get_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route("/api/models", get(list_models).post(create_model))
        .route(
            "/api/models/{id}",
            get(get_model).put(update_model).delete(delete_model),
        )
        .route("/api/channels", get(list_channels).post(create_channel))
        .route(
            "/api/channels/{id}",
            put(update_channel).delete(delete_channel),
        )
        .route("/api/api-keys", get(list_api_keys).post(create_api_key))
        .route(
            "/api/api-keys/{id}",
            put(update_api_key).delete(delete_api_key),
        )
        .route("/api/api-keys/{id}/rotate", post(rotate_api_key))
        .route("/api/catalog/export", get(export_catalog))
        .route("/api/catalog/import", post(import_catalog))
}

fn ok<T: Serialize>(data: T) -> Json<Value> {
    Json(json!({ "success": true, "data": data }))
}

fn err(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({ "success": false, "message": message })),
    )
}

fn require_session(state: &AppState, headers: &HeaderMap) -> Option<(StatusCode, Json<Value>)> {
    let Some(token) = auth::cookie_value(headers) else {
        return Some(err(StatusCode::UNAUTHORIZED, "Unauthorized"));
    };
    if state.sessions.valid(&token) {
        None
    } else {
        Some(err(StatusCode::UNAUTHORIZED, "Unauthorized"))
    }
}

#[derive(Deserialize)]
struct LoginBody {
    password: String,
    #[allow(dead_code)]
    username: Option<String>,
}

async fn login(State(state): State<AppState>, Json(body): Json<LoginBody>) -> impl IntoResponse {
    if body.password != state.admin_password {
        return err(StatusCode::UNAUTHORIZED, "Invalid username or password").into_response();
    }
    let token = state.sessions.issue();
    let mut response = ok(json!({
        "user": { "username": "admin", "role_id": 1 }
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        auth::set_cookie_header(&token).parse().expect("cookie"),
    );
    response
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(token) = auth::cookie_value(&headers) {
        state.sessions.revoke(&token);
    }
    let mut response = ok(json!({ "ok": true })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        auth::clear_cookie_header().parse().expect("cookie"),
    );
    response
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    ok(json!({ "username": "admin", "role_id": 1 })).into_response()
}

#[derive(Deserialize)]
struct ImportBody {
    yaml: String,
    #[serde(default)]
    replace: bool,
}

async fn export_catalog(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match yaml::export_yaml(state.store.catalog()).await {
        Ok(text) => ok(json!({ "yaml": text })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn import_catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ImportBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    let mode = if body.replace {
        ImportMode::Replace
    } else {
        ImportMode::Merge
    };
    match yaml::import_yaml(state.store.catalog(), &body.yaml, mode).await {
        Ok(report) => {
            let _ = state.store.reload().await;
            ok(report).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn overview(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().overview_counts().await {
        Ok(counts) => ok(json!({
            "providers": counts.providers,
            "models": counts.models,
            "api_keys": counts.api_keys,
            "channels": counts.channels,
            "h2c": state.config.h2c,
            "listen": state.config.listen,
            "storage": state.store.catalog().kind().as_str(),
            "version": env!("CARGO_PKG_VERSION"),
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ProviderBody {
    name: String,
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

async fn list_providers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().list_providers().await {
        Ok(rows) => ok(rows).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn get_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().get_provider(id).await {
        Ok(Some(row)) => ok(row).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "provider not found").into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn create_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProviderBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state
        .store
        .catalog()
        .create_provider(&body.name, &body.base_url, &body.api_key, body.enabled)
        .await
    {
        Ok(row) => {
            let _ = state.store.reload().await;
            ok(row).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn update_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<ProviderBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state
        .store
        .catalog()
        .update_provider(id, &body.name, &body.base_url, &body.api_key, body.enabled)
        .await
    {
        Ok(Some(row)) => {
            let _ = state.store.reload().await;
            ok(row).into_response()
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "provider not found").into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn delete_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().delete_provider(id).await {
        Ok(()) => {
            let _ = state.store.reload().await;
            ok(json!({ "id": id })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ModelBody {
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().list_models().await {
        Ok(rows) => ok(rows).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn get_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().get_model(id).await {
        Ok(Some(row)) => ok(row).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "model not found").into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn create_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ModelBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state
        .store
        .catalog()
        .create_model(&body.name, body.enabled)
        .await
    {
        Ok(row) => {
            let _ = state.store.reload().await;
            ok(row).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn update_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<ModelBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state
        .store
        .catalog()
        .update_model(id, &body.name, body.enabled)
        .await
    {
        Ok(Some(row)) => {
            let _ = state.store.reload().await;
            ok(row).into_response()
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "model not found").into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn delete_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().delete_model(id).await {
        Ok(()) => {
            let _ = state.store.reload().await;
            ok(json!({ "id": id })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ChannelBody {
    model_id: i64,
    provider_id: i64,
    upstream_model: String,
    #[serde(default = "default_weight")]
    weight: i32,
    #[serde(default)]
    priority: i32,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_weight() -> i32 {
    1
}

async fn list_channels(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().list_channels().await {
        Ok(rows) => ok(rows).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn create_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ChannelBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state
        .store
        .catalog()
        .create_channel(
            body.model_id,
            body.provider_id,
            &body.upstream_model,
            body.weight,
            body.priority,
            body.enabled,
        )
        .await
    {
        Ok(row) => {
            let _ = state.store.reload().await;
            ok(row).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn update_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<ChannelBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state
        .store
        .catalog()
        .update_channel(
            id,
            body.model_id,
            body.provider_id,
            &body.upstream_model,
            body.weight,
            body.priority,
            body.enabled,
        )
        .await
    {
        Ok(Some(row)) => {
            let _ = state.store.reload().await;
            ok(row).into_response()
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "channel not found").into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn delete_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().delete_channel(id).await {
        Ok(()) => {
            let _ = state.store.reload().await;
            ok(json!({ "id": id })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ApiKeyBody {
    name: String,
    #[serde(default)]
    model_ids: Vec<i64>,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Deserialize)]
struct ApiKeyUpdateBody {
    name: String,
    #[serde(default)]
    model_ids: Option<Vec<i64>>,
    #[serde(default = "default_true")]
    enabled: bool,
}

async fn list_api_keys(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().list_api_keys().await {
        Ok(rows) => {
            let data: Vec<Value> = rows
                .into_iter()
                .map(|row| {
                    json!({
                        "id": row.id,
                        "name": row.name,
                        "key_prefix": row.key_prefix,
                        "masked_key": format!("{}...****", row.key_prefix),
                        "enabled": row.enabled,
                        "created_at": row.created_at,
                        "model_ids": row.model_ids,
                    })
                })
                .collect();
            ok(data).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn create_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ApiKeyBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    let plaintext = generate_api_key();
    let hash = hash_api_key(&plaintext);
    let prefix = key_prefix(&plaintext);
    match state
        .store
        .catalog()
        .create_api_key(&body.name, &hash, &prefix, body.enabled, &body.model_ids)
        .await
    {
        Ok(row) => {
            let _ = state.store.reload().await;
            ok(json!({
                "id": row.id,
                "name": row.name,
                "key": plaintext,
                "key_prefix": row.key_prefix,
                "masked_key": mask_api_key(&plaintext),
                "enabled": row.enabled,
                "created_at": row.created_at,
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn update_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<ApiKeyUpdateBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state
        .store
        .catalog()
        .update_api_key(id, &body.name, body.enabled, body.model_ids.as_deref())
        .await
    {
        Ok(Some(row)) => {
            let _ = state.store.reload().await;
            ok(row).into_response()
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "api key not found").into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn rotate_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    let plaintext = generate_api_key();
    let hash = hash_api_key(&plaintext);
    let prefix = key_prefix(&plaintext);
    match state
        .store
        .catalog()
        .rotate_api_key(id, &hash, &prefix)
        .await
    {
        Ok(Some(row)) => {
            let _ = state.store.reload().await;
            ok(json!({
                "id": row.id,
                "name": row.name,
                "key": plaintext,
                "key_prefix": row.key_prefix,
                "masked_key": mask_api_key(&plaintext),
                "enabled": row.enabled,
                "created_at": row.created_at,
            }))
            .into_response()
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "api key not found").into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn delete_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match state.store.catalog().delete_api_key(id).await {
        Ok(()) => {
            let _ = state.store.reload().await;
            ok(json!({ "id": id })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}
