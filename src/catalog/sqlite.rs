use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use super::types::*;
use crate::config::is_memory_url;

/// `providers` row including the upstream-shape columns. `extra_headers` is JSON
/// text in SQLite; it is parsed here and validated again at snapshot build.
#[derive(sqlx::FromRow)]
struct ProviderRow {
    id: i64,
    name: String,
    base_url: String,
    api_key: String,
    enabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    auth_scheme: String,
    auth_header: String,
    extra_headers: String,
    chat_path: String,
    header_timeout_secs: Option<i64>,
    stall_timeout_secs: Option<i64>,
}

const PROVIDER_COLUMNS: &str = "id, name, base_url, api_key, enabled, created_at, \
                                auth_scheme, auth_header, extra_headers, chat_path, \
                                header_timeout_secs, stall_timeout_secs";

impl ProviderRow {
    fn upstream(&self) -> Result<UpstreamShape> {
        Ok(UpstreamShape {
            auth_scheme: self.auth_scheme.clone(),
            auth_header: self.auth_header.clone(),
            extra_headers: decode_headers(&self.extra_headers)?,
            chat_path: self.chat_path.clone(),
            header_timeout_secs: self.header_timeout_secs.and_then(|v| u64::try_from(v).ok()),
            stall_timeout_secs: self.stall_timeout_secs.and_then(|v| u64::try_from(v).ok()),
        })
    }

    fn into_provider(self) -> Result<Provider> {
        let upstream = self.upstream()?;
        Ok(Provider {
            id: self.id,
            name: self.name,
            base_url: self.base_url,
            enabled: self.enabled,
            created_at: self.created_at,
            upstream,
        })
    }

    fn into_secret(self) -> Result<ProviderSecret> {
        let upstream = self.upstream()?;
        Ok(ProviderSecret {
            id: self.id,
            name: self.name,
            base_url: self.base_url,
            api_key: self.api_key,
            enabled: self.enabled,
            created_at: self.created_at,
            upstream,
        })
    }
}

fn encode_headers(headers: &[HeaderPair]) -> Result<String> {
    serde_json::to_string(headers).context("encode extra_headers")
}

fn decode_headers(raw: &str) -> Result<Vec<HeaderPair>> {
    serde_json::from_str(raw).with_context(|| format!("decode extra_headers: {raw}"))
}

#[derive(Clone)]
pub struct SqliteCatalog {
    pool: SqlitePool,
}

impl SqliteCatalog {
    pub async fn connect(url: &str) -> Result<Self> {
        let memory = is_memory_url(url);
        let options = if memory {
            SqliteConnectOptions::new()
                .filename(":memory:")
                .shared_cache(true)
                .create_if_missing(true)
                .foreign_keys(true)
                .busy_timeout(Duration::from_secs(5))
        } else {
            let path = sqlite_path(url);
            if let Some(parent) = Path::new(&path).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create sqlite directory {}", parent.display()))?;
            }
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true)
                .journal_mode(SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_secs(5))
        };
        // :memory: is per connection. One connection keeps migrations and
        // catalog writes on the same in-process database.
        let max_connections = if memory { 1 } else { 8 };
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .context("connect sqlite")?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("run sqlite migrations")?;
        Ok(Self { pool })
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map(|_| ())
            .context("sqlite ping")
    }

    pub async fn overview_counts(&self) -> Result<OverviewCounts> {
        Ok(OverviewCounts {
            providers: count(&self.pool, "SELECT COUNT(*) FROM providers").await,
            models: count(&self.pool, "SELECT COUNT(*) FROM models").await,
            api_keys: count(&self.pool, "SELECT COUNT(*) FROM api_keys").await,
            channels: count(&self.pool, "SELECT COUNT(*) FROM channels").await,
        })
    }

    pub async fn load_routing(&self) -> Result<RoutingSnapshot> {
        let keys = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, name, key_hash, key_prefix, enabled, created_at FROM api_keys WHERE enabled = 1",
        )
        .fetch_all(&self.pool)
        .await?;
        let bindings = sqlx::query("SELECT api_key_id, model_id FROM api_key_models")
            .fetch_all(&self.pool)
            .await?;
        let mut allowed: std::collections::HashMap<i64, Vec<i64>> =
            std::collections::HashMap::new();
        for row in &bindings {
            let key_id: i64 = row.get("api_key_id");
            let model_id: i64 = row.get("model_id");
            allowed.entry(key_id).or_default().push(model_id);
        }
        let routing_keys = keys
            .into_iter()
            .map(|k| RoutingKey {
                id: k.id,
                name: k.name,
                key_hash: k.key_hash,
                allowed_model_ids: allowed.get(&k.id).cloned(),
            })
            .collect();

        let models = sqlx::query_as::<_, Model>(
            "SELECT id, name, enabled, created_at FROM models WHERE enabled = 1",
        )
        .fetch_all(&self.pool)
        .await?;
        let channels = sqlx::query(
            r#"
            SELECT c.id, c.model_id, c.provider_id, c.upstream_model, c.weight, c.priority,
                   c.max_concurrency,
                   p.name AS provider_name, p.base_url, p.api_key, p.auth_scheme, p.auth_header,
                   p.extra_headers, p.chat_path, p.header_timeout_secs, p.stall_timeout_secs
            FROM channels c
            JOIN providers p ON p.id = c.provider_id
            WHERE c.enabled = 1 AND p.enabled = 1
            ORDER BY c.priority ASC, c.id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut by_model: std::collections::HashMap<i64, Vec<RoutingChannel>> =
            std::collections::HashMap::new();
        let mut provider_names: std::collections::HashMap<i64, String> =
            std::collections::HashMap::new();
        for row in channels {
            let model_id: i64 = row.get("model_id");
            let provider_id: i64 = row.get("provider_id");
            let provider_name: String = row.get("provider_name");
            provider_names.entry(provider_id).or_insert(provider_name);
            let extra_headers: String = row.get("extra_headers");
            by_model.entry(model_id).or_default().push(RoutingChannel {
                id: row.get("id"),
                provider_id,
                base_url: row.get("base_url"),
                api_key: row.get("api_key"),
                upstream_model: row.get("upstream_model"),
                weight: row.get("weight"),
                priority: row.get("priority"),
                max_concurrency: row.get("max_concurrency"),
                upstream: UpstreamShape {
                    auth_scheme: row.get("auth_scheme"),
                    auth_header: row.get("auth_header"),
                    extra_headers: decode_headers(&extra_headers)?,
                    chat_path: row.get("chat_path"),
                    header_timeout_secs: row
                        .get::<Option<i64>, _>("header_timeout_secs")
                        .and_then(|v| u64::try_from(v).ok()),
                    stall_timeout_secs: row
                        .get::<Option<i64>, _>("stall_timeout_secs")
                        .and_then(|v| u64::try_from(v).ok()),
                },
            });
        }
        let routing_models = models
            .into_iter()
            .filter_map(|m| {
                by_model.remove(&m.id).map(|chs| RoutingModel {
                    model_id: m.id,
                    name: m.name,
                    channels: chs,
                })
            })
            .collect();
        Ok(RoutingSnapshot {
            keys: routing_keys,
            models: routing_models,
            provider_names,
        })
    }

    pub async fn list_providers(&self) -> Result<Vec<Provider>> {
        let sql = format!("SELECT {PROVIDER_COLUMNS} FROM providers ORDER BY id DESC");
        let rows = sqlx::query_as::<_, ProviderRow>(&sql)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(ProviderRow::into_provider).collect()
    }

    pub async fn list_provider_secrets(&self) -> Result<Vec<ProviderSecret>> {
        let sql = format!("SELECT {PROVIDER_COLUMNS} FROM providers ORDER BY id ASC");
        let rows = sqlx::query_as::<_, ProviderRow>(&sql)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(ProviderRow::into_secret).collect()
    }

    pub async fn wipe(&self) -> Result<()> {
        sqlx::query("DELETE FROM api_key_models")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM api_keys")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM channels")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM models")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM providers")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn create_provider(
        &self,
        name: &str,
        base_url: &str,
        api_key: &str,
        enabled: bool,
        upstream: &UpstreamShape,
    ) -> Result<Provider> {
        let created = now();
        let sql = format!(
            "INSERT INTO providers (name, base_url, api_key, enabled, created_at,
                auth_scheme, auth_header, extra_headers, chat_path,
                header_timeout_secs, stall_timeout_secs)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING {PROVIDER_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ProviderRow>(&sql)
            .bind(name)
            .bind(base_url)
            .bind(api_key)
            .bind(enabled)
            .bind(created)
            .bind(&upstream.auth_scheme)
            .bind(&upstream.auth_header)
            .bind(encode_headers(&upstream.extra_headers)?)
            .bind(&upstream.chat_path)
            .bind(upstream.header_timeout_secs.map(|v| v as i64))
            .bind(upstream.stall_timeout_secs.map(|v| v as i64))
            .fetch_one(&self.pool)
            .await?;
        row.into_provider()
    }

    pub async fn update_provider(
        &self,
        id: i64,
        name: &str,
        base_url: &str,
        api_key: &str,
        enabled: bool,
        upstream: &UpstreamShape,
    ) -> Result<Option<Provider>> {
        let sql = format!(
            "UPDATE providers SET name = ?, base_url = ?,
                api_key = CASE WHEN ? = '' THEN api_key ELSE ? END,
                enabled = ?,
                auth_scheme = ?, auth_header = ?, extra_headers = ?, chat_path = ?,
                header_timeout_secs = ?, stall_timeout_secs = ?
             WHERE id = ?
             RETURNING {PROVIDER_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ProviderRow>(&sql)
            .bind(name)
            .bind(base_url)
            .bind(api_key)
            .bind(api_key)
            .bind(enabled)
            .bind(&upstream.auth_scheme)
            .bind(&upstream.auth_header)
            .bind(encode_headers(&upstream.extra_headers)?)
            .bind(&upstream.chat_path)
            .bind(upstream.header_timeout_secs.map(|v| v as i64))
            .bind(upstream.stall_timeout_secs.map(|v| v as i64))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(ProviderRow::into_provider).transpose()
    }

    pub async fn delete_provider(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM providers WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_models(&self) -> Result<Vec<Model>> {
        Ok(sqlx::query_as::<_, Model>(
            "SELECT id, name, enabled, created_at FROM models ORDER BY id DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_model(&self, name: &str, enabled: bool) -> Result<Model> {
        let created = now();
        Ok(sqlx::query_as::<_, Model>(
            "INSERT INTO models (name, enabled, created_at) VALUES (?, ?, ?)
             RETURNING id, name, enabled, created_at",
        )
        .bind(name)
        .bind(enabled)
        .bind(created)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn update_model(&self, id: i64, name: &str, enabled: bool) -> Result<Option<Model>> {
        Ok(sqlx::query_as::<_, Model>(
            "UPDATE models SET name = ?, enabled = ? WHERE id = ?
             RETURNING id, name, enabled, created_at",
        )
        .bind(name)
        .bind(enabled)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn delete_model(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM models WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_channels(&self) -> Result<Vec<Channel>> {
        Ok(sqlx::query_as::<_, Channel>(
            "SELECT id, model_id, provider_id, upstream_model, weight, priority, enabled,
                    max_concurrency
             FROM channels ORDER BY priority ASC, id ASC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_channel(
        &self,
        model_id: i64,
        provider_id: i64,
        upstream_model: &str,
        weight: i32,
        priority: i32,
        enabled: bool,
        max_concurrency: Option<i64>,
    ) -> Result<Channel> {
        Ok(sqlx::query_as::<_, Channel>(
            "INSERT INTO channels
                (model_id, provider_id, upstream_model, weight, priority, enabled, max_concurrency)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             RETURNING id, model_id, provider_id, upstream_model, weight, priority, enabled,
                       max_concurrency",
        )
        .bind(model_id)
        .bind(provider_id)
        .bind(upstream_model)
        .bind(weight.max(1))
        .bind(priority)
        .bind(enabled)
        .bind(max_concurrency)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn update_channel(
        &self,
        id: i64,
        model_id: i64,
        provider_id: i64,
        upstream_model: &str,
        weight: i32,
        priority: i32,
        enabled: bool,
        max_concurrency: Option<i64>,
    ) -> Result<Option<Channel>> {
        Ok(sqlx::query_as::<_, Channel>(
            "UPDATE channels SET model_id = ?, provider_id = ?, upstream_model = ?,
                weight = ?, priority = ?, enabled = ?, max_concurrency = ?
             WHERE id = ?
             RETURNING id, model_id, provider_id, upstream_model, weight, priority, enabled,
                       max_concurrency",
        )
        .bind(model_id)
        .bind(provider_id)
        .bind(upstream_model)
        .bind(weight.max(1))
        .bind(priority)
        .bind(enabled)
        .bind(max_concurrency)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn delete_channel(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM channels WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>> {
        let rows = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, name, key_hash, key_prefix, enabled, created_at FROM api_keys ORDER BY id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let bindings = sqlx::query("SELECT api_key_id, model_id FROM api_key_models")
            .fetch_all(&self.pool)
            .await?;
        let mut by_key: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
        for row in bindings {
            by_key
                .entry(row.get("api_key_id"))
                .or_default()
                .push(row.get("model_id"));
        }
        Ok(rows
            .into_iter()
            .map(|row| ApiKey {
                id: row.id,
                name: row.name,
                key_prefix: row.key_prefix,
                enabled: row.enabled,
                created_at: row.created_at,
                model_ids: by_key.get(&row.id).cloned().unwrap_or_default(),
            })
            .collect())
    }

    pub async fn create_api_key(
        &self,
        name: &str,
        key_hash: &str,
        key_prefix: &str,
        enabled: bool,
        model_ids: &[i64],
    ) -> Result<ApiKey> {
        let created = now();
        let row = sqlx::query_as::<_, ApiKeyRow>(
            "INSERT INTO api_keys (name, key_hash, key_prefix, enabled, created_at)
             VALUES (?, ?, ?, ?, ?)
             RETURNING id, name, key_hash, key_prefix, enabled, created_at",
        )
        .bind(name)
        .bind(key_hash)
        .bind(key_prefix)
        .bind(enabled)
        .bind(created)
        .fetch_one(&self.pool)
        .await?;
        replace_key_models(&self.pool, row.id, model_ids).await?;
        Ok(ApiKey {
            id: row.id,
            name: row.name,
            key_prefix: row.key_prefix,
            enabled: row.enabled,
            created_at: row.created_at,
            model_ids: model_ids.to_vec(),
        })
    }

    pub async fn update_api_key(
        &self,
        id: i64,
        name: &str,
        enabled: bool,
        model_ids: Option<&[i64]>,
    ) -> Result<Option<ApiKey>> {
        let row = sqlx::query_as::<_, ApiKeyRow>(
            "UPDATE api_keys SET name = ?, enabled = ? WHERE id = ?
             RETURNING id, name, key_hash, key_prefix, enabled, created_at",
        )
        .bind(name)
        .bind(enabled)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if let Some(ids) = model_ids {
            replace_key_models(&self.pool, id, ids).await?;
        }
        let keys = self.list_api_keys().await?;
        Ok(keys.into_iter().find(|k| k.id == id).or(Some(ApiKey {
            id: row.id,
            name: row.name,
            key_prefix: row.key_prefix,
            enabled: row.enabled,
            created_at: row.created_at,
            model_ids: model_ids.map(|s| s.to_vec()).unwrap_or_default(),
        })))
    }

    pub async fn rotate_api_key(
        &self,
        id: i64,
        key_hash: &str,
        key_prefix: &str,
    ) -> Result<Option<ApiKey>> {
        let row = sqlx::query_as::<_, ApiKeyRow>(
            "UPDATE api_keys SET key_hash = ?, key_prefix = ? WHERE id = ?
             RETURNING id, name, key_hash, key_prefix, enabled, created_at",
        )
        .bind(key_hash)
        .bind(key_prefix)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let keys = self.list_api_keys().await?;
        Ok(keys.into_iter().find(|k| k.id == id).or(Some(ApiKey {
            id: row.id,
            name: row.name,
            key_prefix: row.key_prefix,
            enabled: row.enabled,
            created_at: row.created_at,
            model_ids: Vec::new(),
        })))
    }

    pub async fn delete_api_key(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM api_keys WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

async fn replace_key_models(pool: &SqlitePool, key_id: i64, model_ids: &[i64]) -> Result<()> {
    sqlx::query("DELETE FROM api_key_models WHERE api_key_id = ?")
        .bind(key_id)
        .execute(pool)
        .await?;
    for model_id in model_ids {
        sqlx::query("INSERT INTO api_key_models (api_key_id, model_id) VALUES (?, ?)")
            .bind(key_id)
            .bind(model_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

fn sqlite_path(url: &str) -> String {
    url.trim()
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:")
        .trim_start_matches("file:")
        .to_string()
}
