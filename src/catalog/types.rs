use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One static upstream request header. Stored as a pair (not a map) so the
/// operator's order survives a YAML/JSON round trip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderPair {
    pub name: String,
    pub value: String,
}

/// Provider-level upstream request shape.
///
/// Every field defaults to xlite's original behaviour — `Bearer <api_key>` sent
/// as `authorization` to the derived `/v1/chat/completions` — so a catalog that
/// never sets any of them produces byte-identical requests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamShape {
    /// `bearer` | `raw` | `none`.
    #[serde(default = "default_auth_scheme")]
    pub auth_scheme: String,
    /// Credential header name; empty means `authorization`.
    #[serde(default)]
    pub auth_header: String,
    /// Static extra request headers.
    #[serde(default)]
    pub extra_headers: Vec<HeaderPair>,
    /// Override for the chat/completions path; empty keeps the derived default.
    #[serde(default)]
    pub chat_path: String,
    /// Per-provider request→headers timeout in seconds; `None` inherits the
    /// global setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_timeout_secs: Option<u64>,
    /// Per-provider mid-stream stall timeout in seconds; `None` inherits the
    /// global setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_timeout_secs: Option<u64>,
}

fn default_auth_scheme() -> String {
    "bearer".into()
}

impl Default for UpstreamShape {
    fn default() -> Self {
        Self {
            auth_scheme: default_auth_scheme(),
            auth_header: String::new(),
            extra_headers: Vec::new(),
            chat_path: String::new(),
            header_timeout_secs: None,
            stall_timeout_secs: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provider {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    #[serde(flatten)]
    pub upstream: UpstreamShape,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderSecret {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    #[serde(flatten)]
    pub upstream: UpstreamShape,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Model {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Channel {
    pub id: i64,
    pub model_id: i64,
    pub provider_id: i64,
    pub upstream_model: String,
    pub weight: i32,
    pub priority: i32,
    pub enabled: bool,
    /// In-flight cap for this channel; `None` means "only the upstream pool
    /// bounds it". Enforced at request admission, not by queueing.
    #[serde(default)]
    pub max_concurrency: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub key_prefix: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub model_ids: Vec<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct ApiKeyRow {
    pub id: i64,
    pub name: String,
    pub key_hash: String,
    pub key_prefix: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct RoutingChannel {
    pub id: i64,
    pub provider_id: i64,
    pub base_url: String,
    pub api_key: String,
    pub upstream_model: String,
    pub weight: i32,
    pub priority: i32,
    /// In-flight cap carried into the snapshot; `None` = unbounded.
    pub max_concurrency: Option<i64>,
    /// Provider upstream shape, carried through unparsed: the snapshot build is
    /// the single place that validates and precomputes it.
    pub upstream: UpstreamShape,
}

#[derive(Clone, Debug)]
pub struct RoutingModel {
    pub model_id: i64,
    pub name: String,
    pub channels: Vec<RoutingChannel>,
}

#[derive(Clone, Debug)]
pub struct RoutingKey {
    pub id: i64,
    pub name: String,
    pub key_hash: String,
    pub allowed_model_ids: Option<Vec<i64>>,
}

#[derive(Clone, Debug, Default)]
pub struct RoutingSnapshot {
    pub keys: Vec<RoutingKey>,
    pub models: Vec<RoutingModel>,
    pub provider_names: std::collections::HashMap<i64, String>,
}

#[derive(Clone, Debug)]
pub struct OverviewCounts {
    pub providers: i64,
    pub models: i64,
    pub api_keys: i64,
    pub channels: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageKind {
    Sqlite,
    Memory,
    Redis,
}

impl StorageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Memory => "memory",
            Self::Redis => "redis",
        }
    }
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

pub fn next_id<T>(items: &[T], id: impl Fn(&T) -> i64) -> i64 {
    items.iter().map(id).max().unwrap_or(0) + 1
}
