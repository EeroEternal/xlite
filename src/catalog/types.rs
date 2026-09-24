use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Provider {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProviderSecret {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
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
    pub key_hash: String,
    pub allowed_model_ids: Option<Vec<i64>>,
}

#[derive(Clone, Debug, Default)]
pub struct RoutingSnapshot {
    pub keys: Vec<RoutingKey>,
    pub models: Vec<RoutingModel>,
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
