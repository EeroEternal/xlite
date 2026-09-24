//! Control-plane REST API. `{ success, data, message }` envelope.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;

use crate::auth::{self, Sessions};
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

async fn overview(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    let pool = state.store.pool();
    let providers = count(pool, "SELECT COUNT(*) FROM providers").await;
    let models = count(pool, "SELECT COUNT(*) FROM models").await;
    let keys = count(pool, "SELECT COUNT(*) FROM api_keys").await;
    let channels = count(pool, "SELECT COUNT(*) FROM channels").await;
    ok(json!({
        "providers": providers,
        "models": models,
        "api_keys": keys,
        "channels": channels,
        "h2c": state.config.h2c,
        "listen": state.config.listen,
    }))
    .into_response()
}

async fn count(pool: &sqlx::PgPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

#[derive(FromRow, Serialize)]
struct ProviderRow {
    id: i64,
    name: String,
    base_url: String,
    enabled: bool,
    created_at: DateTime<Utc>,
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
    match sqlx::query_as::<_, ProviderRow>(
        "SELECT id, name, base_url, enabled, created_at FROM providers ORDER BY id DESC",
    )
    .fetch_all(state.store.pool())
    .await
    {
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
    match sqlx::query_as::<_, ProviderRow>(
        "SELECT id, name, base_url, enabled, created_at FROM providers WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(state.store.pool())
    .await
    {
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
    match sqlx::query_as::<_, ProviderRow>(
        "INSERT INTO providers (name, base_url, api_key, enabled)
         VALUES ($1, $2, $3, $4)
         RETURNING id, name, base_url, enabled, created_at",
    )
    .bind(&body.name)
    .bind(&body.base_url)
    .bind(&body.api_key)
    .bind(body.enabled)
    .fetch_one(state.store.pool())
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
    match sqlx::query_as::<_, ProviderRow>(
        "UPDATE providers SET name = $1, base_url = $2, api_key = CASE WHEN $3 = '' THEN api_key ELSE $3 END, enabled = $4
         WHERE id = $5
         RETURNING id, name, base_url, enabled, created_at",
    )
    .bind(&body.name)
    .bind(&body.base_url)
    .bind(&body.api_key)
    .bind(body.enabled)
    .bind(id)
    .fetch_optional(state.store.pool())
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
    match sqlx::query("DELETE FROM providers WHERE id = $1")
        .bind(id)
        .execute(state.store.pool())
        .await
    {
        Ok(_) => {
            let _ = state.store.reload().await;
            ok(json!({ "id": id })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

#[derive(FromRow, Serialize)]
struct ModelRow {
    id: i64,
    name: String,
    enabled: bool,
    created_at: DateTime<Utc>,
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
    match sqlx::query_as::<_, ModelRow>(
        "SELECT id, name, enabled, created_at FROM models ORDER BY id DESC",
    )
    .fetch_all(state.store.pool())
    .await
    {
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
    match sqlx::query_as::<_, ModelRow>(
        "SELECT id, name, enabled, created_at FROM models WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(state.store.pool())
    .await
    {
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
    match sqlx::query_as::<_, ModelRow>(
        "INSERT INTO models (name, enabled) VALUES ($1, $2)
         RETURNING id, name, enabled, created_at",
    )
    .bind(&body.name)
    .bind(body.enabled)
    .fetch_one(state.store.pool())
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
    match sqlx::query_as::<_, ModelRow>(
        "UPDATE models SET name = $1, enabled = $2 WHERE id = $3
         RETURNING id, name, enabled, created_at",
    )
    .bind(&body.name)
    .bind(body.enabled)
    .bind(id)
    .fetch_optional(state.store.pool())
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
    match sqlx::query("DELETE FROM models WHERE id = $1")
        .bind(id)
        .execute(state.store.pool())
        .await
    {
        Ok(_) => {
            let _ = state.store.reload().await;
            ok(json!({ "id": id })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

#[derive(FromRow, Serialize)]
struct ChannelRow {
    id: i64,
    model_id: i64,
    provider_id: i64,
    upstream_model: String,
    weight: i32,
    priority: i32,
    enabled: bool,
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
    match sqlx::query_as::<_, ChannelRow>(
        "SELECT id, model_id, provider_id, upstream_model, weight, priority, enabled
         FROM channels ORDER BY priority ASC, id ASC",
    )
    .fetch_all(state.store.pool())
    .await
    {
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
    match sqlx::query_as::<_, ChannelRow>(
        "INSERT INTO channels (model_id, provider_id, upstream_model, weight, priority, enabled)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, model_id, provider_id, upstream_model, weight, priority, enabled",
    )
    .bind(body.model_id)
    .bind(body.provider_id)
    .bind(&body.upstream_model)
    .bind(body.weight.max(1))
    .bind(body.priority)
    .bind(body.enabled)
    .fetch_one(state.store.pool())
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
    match sqlx::query_as::<_, ChannelRow>(
        "UPDATE channels SET model_id = $1, provider_id = $2, upstream_model = $3,
            weight = $4, priority = $5, enabled = $6
         WHERE id = $7
         RETURNING id, model_id, provider_id, upstream_model, weight, priority, enabled",
    )
    .bind(body.model_id)
    .bind(body.provider_id)
    .bind(&body.upstream_model)
    .bind(body.weight.max(1))
    .bind(body.priority)
    .bind(body.enabled)
    .bind(id)
    .fetch_optional(state.store.pool())
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
    match sqlx::query("DELETE FROM channels WHERE id = $1")
        .bind(id)
        .execute(state.store.pool())
        .await
    {
        Ok(_) => {
            let _ = state.store.reload().await;
            ok(json!({ "id": id })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

#[derive(FromRow, Serialize)]
struct ApiKeyRow {
    id: i64,
    name: String,
    key_prefix: String,
    enabled: bool,
    created_at: DateTime<Utc>,
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

#[derive(FromRow)]
struct KeyBindingRow {
    api_key_id: i64,
    model_id: i64,
}

async fn list_api_keys(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    let rows = match sqlx::query_as::<_, ApiKeyRow>(
        "SELECT id, name, key_prefix, enabled, created_at FROM api_keys ORDER BY id DESC",
    )
    .fetch_all(state.store.pool())
    .await
    {
        Ok(rows) => rows,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    };
    let bindings =
        sqlx::query_as::<_, KeyBindingRow>("SELECT api_key_id, model_id FROM api_key_models")
            .fetch_all(state.store.pool())
            .await
            .unwrap_or_default();
    let mut by_key: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    for b in bindings {
        by_key.entry(b.api_key_id).or_default().push(b.model_id);
    }
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
                "model_ids": by_key.get(&row.id).cloned().unwrap_or_default(),
            })
        })
        .collect();
    ok(data).into_response()
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
    let row = match sqlx::query_as::<_, ApiKeyRow>(
        "INSERT INTO api_keys (name, key_hash, key_prefix, enabled)
         VALUES ($1, $2, $3, $4)
         RETURNING id, name, key_prefix, enabled, created_at",
    )
    .bind(&body.name)
    .bind(&hash)
    .bind(&prefix)
    .bind(body.enabled)
    .fetch_one(state.store.pool())
    .await
    {
        Ok(row) => row,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    };
    if let Err(e) = replace_key_models(state.store.pool(), row.id, &body.model_ids).await {
        return err(StatusCode::BAD_REQUEST, &e.to_string()).into_response();
    }
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

async fn update_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<ApiKeyUpdateBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    match sqlx::query_as::<_, ApiKeyRow>(
        "UPDATE api_keys SET name = $1, enabled = $2 WHERE id = $3
         RETURNING id, name, key_prefix, enabled, created_at",
    )
    .bind(&body.name)
    .bind(body.enabled)
    .bind(id)
    .fetch_optional(state.store.pool())
    .await
    {
        Ok(Some(row)) => {
            if let Some(model_ids) = body.model_ids
                && let Err(e) = replace_key_models(state.store.pool(), id, &model_ids).await
            {
                return err(StatusCode::BAD_REQUEST, &e.to_string()).into_response();
            }
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
    match sqlx::query_as::<_, ApiKeyRow>(
        "UPDATE api_keys SET key_hash = $1, key_prefix = $2 WHERE id = $3
         RETURNING id, name, key_prefix, enabled, created_at",
    )
    .bind(&hash)
    .bind(&prefix)
    .bind(id)
    .fetch_optional(state.store.pool())
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
    match sqlx::query("DELETE FROM api_keys WHERE id = $1")
        .bind(id)
        .execute(state.store.pool())
        .await
    {
        Ok(_) => {
            let _ = state.store.reload().await;
            ok(json!({ "id": id })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

async fn replace_key_models(
    pool: &sqlx::PgPool,
    key_id: i64,
    model_ids: &[i64],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM api_key_models WHERE api_key_id = $1")
        .bind(key_id)
        .execute(pool)
        .await?;
    for model_id in model_ids {
        sqlx::query("INSERT INTO api_key_models (api_key_id, model_id) VALUES ($1, $2)")
            .bind(key_id)
            .bind(model_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}
