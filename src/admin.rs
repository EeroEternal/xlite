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
use crate::catalog::yaml::{self, CatalogYaml, ImportMode};
use crate::catalog::{HeaderPair, UpstreamShape};
use crate::config::Config;
use crate::hash::{generate_api_key, hash_api_key, key_prefix, mask_api_key};
use crate::store::Store;
use crate::upstream::{self, Upstream};

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
        .route("/api/pool", get(pool_stats))
        .route("/api/providers", get(list_providers).post(create_provider))
        .route(
            "/api/providers/{id}",
            put(update_provider).delete(delete_provider),
        )
        .route("/api/models", get(list_models).post(create_model))
        .route("/api/models/{id}", put(update_model).delete(delete_model))
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
    if let Some(token) = auth::bearer_or_header_token(headers)
        && (auth::constant_time_str_eq(token, &state.admin_password) || state.sessions.valid(token))
    {
        return None;
    }
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

async fn logout(State(_state): State<AppState>) -> impl IntoResponse {
    // Sessions are stateless signatures, so logout only clears the cookie.
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
    Json(body): Json<Value>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    let mode = if body.get("replace").and_then(Value::as_bool) == Some(true) {
        ImportMode::Replace
    } else {
        ImportMode::Merge
    };
    // Accept either `{"yaml": "<catalog text>"}` or the catalog document itself as JSON.
    let has_doc = ["providers", "models", "api_keys"]
        .iter()
        .any(|key| body.get(key).is_some());
    let result = match body.get("yaml").and_then(Value::as_str) {
        Some(text) => yaml::import_yaml(state.store.catalog(), text, mode).await,
        None if has_doc => match serde_json::from_value::<CatalogYaml>(body) {
            Ok(doc) => yaml::import_doc(state.store.catalog(), doc, mode).await,
            Err(e) => return err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
        },
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                "expected {\"yaml\": \"...\"} or {\"providers\": [...], \"models\": [...], \"api_keys\": [...]}",
            )
            .into_response();
        }
    };
    match result {
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

/// Coarse percentile out of a wait-time histogram ([`PoolStats::wait_buckets`]).
///
/// Returns the **upper edge** of the bucket the percentile lands in, or `None` when
/// no wait was ever recorded. Upper edge on purpose: the number is then an upper
/// bound, so a queue's tail is never understated. Bucket `i` holds waits under
/// `2^(i+1)` ms, so its edge is `2^(i+1)` — except the overflow bucket, which has no
/// upper edge and therefore reports its own lower edge.
fn wait_percentile(buckets: &[u64; xlite_h2_pool::WAIT_BUCKETS], pct: u64) -> Option<u64> {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return None;
    }
    let target = (total * pct).div_ceil(100);
    let mut seen = 0u64;
    for (i, n) in buckets.iter().enumerate() {
        seen += n;
        if seen >= target {
            let edge = if i + 1 == xlite_h2_pool::WAIT_BUCKETS {
                1u64 << i
            } else {
                1u64 << (i + 1)
            };
            return Some(edge);
        }
    }
    None
}

/// Read-only snapshot of the upstream connection pool: per host, how many
/// connections are open, how many streams are reserved, and the cumulative
/// dial/cap bookkeeping.
///
/// Two uses, both control-plane: the load-test probe (`advanced/pool_sizing_probe.sh`)
/// reads it to decide whether `XLITE_UPSTREAM_STREAM_CAP` is actually binding
/// (issue #49), and it answers "what did the pool look like during that burst of
/// 502s?" without inferring it from `ss`. Session-guarded like every other
/// `/api/*` route: it exposes internal topology, so it is not public.
async fn pool_stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    let max_conns = state.config.max_conns_per_host;
    let hosts: Vec<Value> = state
        .upstream
        .pool_stats()
        .into_iter()
        .map(|(host, s)| {
            // The per-host admission ceiling is stated explicitly so nobody has to
            // multiply `min(cap, peer) × max_conns` by hand — and so changing the
            // cap is *visible* instead of silently resizing the ceiling. Both peer
            // values are `null` until a handshake has happened (the peer's
            // advertised SETTINGS does not exist before then), and they follow a
            // peer that re-advertises mid-connection.
            let per_conn = s.effective_streams_per_conn();
            json!({
                "host": host,
                "connections": s.connections,
                "in_flight": s.in_flight,
                "peer_cap": s.peer_cap,
                "effective_streams_per_conn": per_conn,
                "effective_ceiling": per_conn.map(|n| n.saturating_mul(max_conns)),
                // Queue visibility: `in_flight` says the pool is full; these say how
                // many requests are parked behind it and how long they wait. That is
                // the half that used to be invisible — the queue itself has always
                // been there (the pool waits at its ceiling).
                "waiting": s.waiting,
                "waiting_max": s.waiting_max,
                "queued_total": s.queued_total,
                "wait_ms_mean": if s.queued_total > 0 {
                    s.wait_millis / s.queued_total
                } else {
                    0
                },
                "wait_ms_max": s.wait_millis_max,
                "wait_ms_p50": wait_percentile(&s.wait_buckets, 50),
                "wait_ms_p99": wait_percentile(&s.wait_buckets, 99),
                "dials": s.dials,
                "reclaimed": s.reclaimed,
                "settings_fallbacks": s.settings_fallbacks,
                "cap_updates": s.cap_updates,
            })
        })
        .collect();
    let stream_budget = if state.config.upstream_stream_cap == usize::MAX {
        Value::Null
    } else {
        json!(state.config.upstream_stream_cap)
    };
    ok(json!({
        "stream_budget_per_conn": stream_budget,
        "max_conns_per_host": state.config.max_conns_per_host,
        "hosts": hosts,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ProviderBody {
    name: String,
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default = "default_true")]
    enabled: bool,
    /// `bearer` (default) | `raw` | `none`.
    #[serde(default = "default_auth_scheme")]
    auth_scheme: String,
    /// Credential header name; empty means `authorization`.
    #[serde(default)]
    auth_header: String,
    /// Static extra request headers.
    #[serde(default)]
    extra_headers: Vec<HeaderPair>,
    /// Chat/completions path override; empty keeps the derived default.
    #[serde(default)]
    chat_path: String,
}

fn default_auth_scheme() -> String {
    "bearer".to_string()
}

impl ProviderBody {
    /// Build the upstream shape, rejecting anything the snapshot build would
    /// refuse (reserved header names, bad values, unknown scheme) with a 400
    /// instead of letting it reach the catalog.
    fn upstream(&self) -> std::result::Result<UpstreamShape, String> {
        let shape = UpstreamShape {
            auth_scheme: self.auth_scheme.clone(),
            auth_header: self.auth_header.clone(),
            extra_headers: self.extra_headers.clone(),
            chat_path: self.chat_path.clone(),
        };
        upstream::validate_upstream(&shape, &self.api_key)?;
        Ok(shape)
    }
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

async fn create_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProviderBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    let upstream = match body.upstream() {
        Ok(shape) => shape,
        Err(message) => return err(StatusCode::BAD_REQUEST, &message).into_response(),
    };
    match state
        .store
        .catalog()
        .create_provider(
            &body.name,
            &body.base_url,
            &body.api_key,
            body.enabled,
            &upstream,
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

async fn update_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<ProviderBody>,
) -> Response {
    if let Some(resp) = require_session(&state, &headers) {
        return resp.into_response();
    }
    let upstream = match body.upstream() {
        Ok(shape) => shape,
        Err(message) => return err(StatusCode::BAD_REQUEST, &message).into_response(),
    };
    match state
        .store
        .catalog()
        .update_provider(
            id,
            &body.name,
            &body.base_url,
            &body.api_key,
            body.enabled,
            &upstream,
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_percentile_reads_the_bucket_edges() {
        let mut buckets = [0u64; xlite_h2_pool::WAIT_BUCKETS];
        assert_eq!(
            wait_percentile(&buckets, 50),
            None,
            "no waits ⇒ no percentile"
        );

        // Bucket 3 holds waits under 2^4 = 16ms; bucket 10 holds waits under 2^11 = 2048ms.
        buckets[3] = 99;
        buckets[10] = 1;
        assert_eq!(
            wait_percentile(&buckets, 50),
            Some(16),
            "the median is in bucket 3 ⇒ its upper edge"
        );
        assert_eq!(
            wait_percentile(&buckets, 99),
            Some(16),
            "99 of 100 ⇒ still bucket 3 (the percentile is inclusive)"
        );
        assert_eq!(
            wait_percentile(&buckets, 100),
            Some(2048),
            "the last sample is in bucket 10 ⇒ its upper edge"
        );

        // The overflow bucket has no upper edge, so it reports its lower edge rather
        // than a number that would overstate the wait.
        let mut overflow = [0u64; xlite_h2_pool::WAIT_BUCKETS];
        overflow[xlite_h2_pool::WAIT_BUCKETS - 1] = 1;
        assert_eq!(
            wait_percentile(&overflow, 100),
            Some(1u64 << (xlite_h2_pool::WAIT_BUCKETS - 1))
        );
    }
}
