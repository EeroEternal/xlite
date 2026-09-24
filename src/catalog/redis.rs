use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::types::*;

const CATALOG_KEY: &str = "xlite:catalog";
const LOCK_KEY: &str = "xlite:catalog:lock";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Dump {
    providers: Vec<ProviderSecret>,
    models: Vec<Model>,
    channels: Vec<Channel>,
    api_keys: Vec<ApiKeyStored>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApiKeyStored {
    id: i64,
    name: String,
    key_hash: String,
    key_prefix: String,
    enabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    model_ids: Vec<i64>,
}

#[derive(Clone)]
pub struct RedisCatalog {
    conn: ConnectionManager,
    write: Arc<Mutex<()>>,
}

impl RedisCatalog {
    pub async fn connect(url: &str) -> Result<Self> {
        let client = redis::Client::open(url).context("parse REDIS_URL")?;
        let conn = ConnectionManager::new(client)
            .await
            .context("connect redis")?;
        Ok(Self {
            conn,
            write: Arc::new(Mutex::new(())),
        })
    }

    pub async fn ping(&self) -> Result<()> {
        let mut conn = self.conn.clone();
        let pong: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .context("redis ping")?;
        if pong.eq_ignore_ascii_case("PONG") {
            Ok(())
        } else {
            bail!("unexpected redis ping reply: {pong}")
        }
    }

    async fn load(&self) -> Result<Dump> {
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn.get(CATALOG_KEY).await.context("redis GET catalog")?;
        match raw {
            Some(s) if !s.is_empty() => serde_json::from_str(&s).context("decode catalog json"),
            _ => Ok(Dump::default()),
        }
    }

    async fn save(&self, dump: &Dump) -> Result<()> {
        let mut conn = self.conn.clone();
        let body = serde_json::to_string(dump).context("encode catalog json")?;
        let _: () = conn
            .set(CATALOG_KEY, body)
            .await
            .context("redis SET catalog")?;
        Ok(())
    }

    async fn mutate<T>(&self, f: impl FnOnce(&mut Dump) -> Result<T>) -> Result<T> {
        let _guard = self.write.lock().await;
        self.acquire_lock().await?;
        let result = async {
            let mut dump = self.load().await?;
            let out = f(&mut dump)?;
            self.save(&dump).await?;
            Ok(out)
        }
        .await;
        self.release_lock().await;
        result
    }

    async fn acquire_lock(&self) -> Result<()> {
        let mut conn = self.conn.clone();
        for _ in 0..50 {
            let ok: Option<String> = redis::cmd("SET")
                .arg(LOCK_KEY)
                .arg("1")
                .arg("NX")
                .arg("EX")
                .arg(5)
                .query_async(&mut conn)
                .await
                .context("redis lock")?;
            if ok.is_some() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        bail!("could not acquire redis catalog lock")
    }

    async fn release_lock(&self) {
        let mut conn = self.conn.clone();
        let _: Result<(), _> = conn.del(LOCK_KEY).await;
    }

    pub async fn overview_counts(&self) -> Result<OverviewCounts> {
        let dump = self.load().await?;
        Ok(OverviewCounts {
            providers: dump.providers.len() as i64,
            models: dump.models.len() as i64,
            api_keys: dump.api_keys.len() as i64,
            channels: dump.channels.len() as i64,
        })
    }

    pub async fn load_routing(&self) -> Result<RoutingSnapshot> {
        let dump = self.load().await?;
        let keys = dump
            .api_keys
            .iter()
            .filter(|k| k.enabled)
            .map(|k| RoutingKey {
                id: k.id,
                key_hash: k.key_hash.clone(),
                allowed_model_ids: if k.model_ids.is_empty() {
                    None
                } else {
                    Some(k.model_ids.clone())
                },
            })
            .collect();
        let mut by_model: std::collections::HashMap<i64, Vec<RoutingChannel>> =
            std::collections::HashMap::new();
        for ch in dump.channels.iter().filter(|c| c.enabled) {
            let Some(provider) = dump
                .providers
                .iter()
                .find(|p| p.id == ch.provider_id && p.enabled)
            else {
                continue;
            };
            by_model
                .entry(ch.model_id)
                .or_default()
                .push(RoutingChannel {
                    id: ch.id,
                    provider_id: ch.provider_id,
                    base_url: provider.base_url.clone(),
                    api_key: provider.api_key.clone(),
                    upstream_model: ch.upstream_model.clone(),
                    weight: ch.weight,
                    priority: ch.priority,
                });
        }
        for chs in by_model.values_mut() {
            chs.sort_by_key(|c| (c.priority, c.id));
        }
        let models = dump
            .models
            .into_iter()
            .filter(|m| m.enabled)
            .filter_map(|m| {
                by_model.remove(&m.id).map(|channels| RoutingModel {
                    model_id: m.id,
                    name: m.name,
                    channels,
                })
            })
            .collect();
        Ok(RoutingSnapshot { keys, models })
    }

    pub async fn list_providers(&self) -> Result<Vec<Provider>> {
        let mut dump = self.load().await?;
        dump.providers.sort_by_key(|p| std::cmp::Reverse(p.id));
        Ok(dump.providers.into_iter().map(strip_provider).collect())
    }

    pub async fn get_provider(&self, id: i64) -> Result<Option<Provider>> {
        let dump = self.load().await?;
        Ok(dump
            .providers
            .into_iter()
            .find(|p| p.id == id)
            .map(strip_provider))
    }

    pub async fn create_provider(
        &self,
        name: &str,
        base_url: &str,
        api_key: &str,
        enabled: bool,
    ) -> Result<Provider> {
        self.mutate(|dump| {
            let id = next_id(&dump.providers, |p| p.id);
            let row = ProviderSecret {
                id,
                name: name.to_string(),
                base_url: base_url.to_string(),
                api_key: api_key.to_string(),
                enabled,
                created_at: now(),
            };
            dump.providers.push(row.clone());
            Ok(strip_provider(row))
        })
        .await
    }

    pub async fn update_provider(
        &self,
        id: i64,
        name: &str,
        base_url: &str,
        api_key: &str,
        enabled: bool,
    ) -> Result<Option<Provider>> {
        self.mutate(|dump| {
            let Some(row) = dump.providers.iter_mut().find(|p| p.id == id) else {
                return Ok(None);
            };
            row.name = name.to_string();
            row.base_url = base_url.to_string();
            if !api_key.is_empty() {
                row.api_key = api_key.to_string();
            }
            row.enabled = enabled;
            Ok(Some(strip_provider(row.clone())))
        })
        .await
    }

    pub async fn delete_provider(&self, id: i64) -> Result<()> {
        self.mutate(|dump| {
            if dump.channels.iter().any(|c| c.provider_id == id) {
                bail!("provider is still used by a channel");
            }
            dump.providers.retain(|p| p.id != id);
            Ok(())
        })
        .await
    }

    pub async fn list_models(&self) -> Result<Vec<Model>> {
        let mut dump = self.load().await?;
        dump.models.sort_by_key(|m| std::cmp::Reverse(m.id));
        Ok(dump.models)
    }

    pub async fn get_model(&self, id: i64) -> Result<Option<Model>> {
        let dump = self.load().await?;
        Ok(dump.models.into_iter().find(|m| m.id == id))
    }

    pub async fn create_model(&self, name: &str, enabled: bool) -> Result<Model> {
        self.mutate(|dump| {
            if dump.models.iter().any(|m| m.name == name) {
                bail!("model name already exists");
            }
            let row = Model {
                id: next_id(&dump.models, |m| m.id),
                name: name.to_string(),
                enabled,
                created_at: now(),
            };
            dump.models.push(row.clone());
            Ok(row)
        })
        .await
    }

    pub async fn update_model(&self, id: i64, name: &str, enabled: bool) -> Result<Option<Model>> {
        self.mutate(|dump| {
            if dump.models.iter().any(|m| m.name == name && m.id != id) {
                bail!("model name already exists");
            }
            let Some(row) = dump.models.iter_mut().find(|m| m.id == id) else {
                return Ok(None);
            };
            row.name = name.to_string();
            row.enabled = enabled;
            Ok(Some(row.clone()))
        })
        .await
    }

    pub async fn delete_model(&self, id: i64) -> Result<()> {
        self.mutate(|dump| {
            dump.models.retain(|m| m.id != id);
            dump.channels.retain(|c| c.model_id != id);
            for key in &mut dump.api_keys {
                key.model_ids.retain(|mid| *mid != id);
            }
            Ok(())
        })
        .await
    }

    pub async fn list_channels(&self) -> Result<Vec<Channel>> {
        let mut dump = self.load().await?;
        dump.channels.sort_by_key(|c| (c.priority, c.id));
        Ok(dump.channels)
    }

    pub async fn create_channel(
        &self,
        model_id: i64,
        provider_id: i64,
        upstream_model: &str,
        weight: i32,
        priority: i32,
        enabled: bool,
    ) -> Result<Channel> {
        self.mutate(|dump| {
            if !dump.models.iter().any(|m| m.id == model_id) {
                bail!("model not found");
            }
            if !dump.providers.iter().any(|p| p.id == provider_id) {
                bail!("provider not found");
            }
            let row = Channel {
                id: next_id(&dump.channels, |c| c.id),
                model_id,
                provider_id,
                upstream_model: upstream_model.to_string(),
                weight: weight.max(1),
                priority,
                enabled,
            };
            dump.channels.push(row.clone());
            Ok(row)
        })
        .await
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
    ) -> Result<Option<Channel>> {
        self.mutate(|dump| {
            if !dump.models.iter().any(|m| m.id == model_id) {
                bail!("model not found");
            }
            if !dump.providers.iter().any(|p| p.id == provider_id) {
                bail!("provider not found");
            }
            let Some(row) = dump.channels.iter_mut().find(|c| c.id == id) else {
                return Ok(None);
            };
            row.model_id = model_id;
            row.provider_id = provider_id;
            row.upstream_model = upstream_model.to_string();
            row.weight = weight.max(1);
            row.priority = priority;
            row.enabled = enabled;
            Ok(Some(row.clone()))
        })
        .await
    }

    pub async fn delete_channel(&self, id: i64) -> Result<()> {
        self.mutate(|dump| {
            dump.channels.retain(|c| c.id != id);
            Ok(())
        })
        .await
    }

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>> {
        let mut dump = self.load().await?;
        dump.api_keys.sort_by_key(|k| std::cmp::Reverse(k.id));
        Ok(dump.api_keys.into_iter().map(to_api_key).collect())
    }

    pub async fn create_api_key(
        &self,
        name: &str,
        key_hash: &str,
        key_prefix: &str,
        enabled: bool,
        model_ids: &[i64],
    ) -> Result<ApiKey> {
        self.mutate(|dump| {
            if dump.api_keys.iter().any(|k| k.key_hash == key_hash) {
                bail!("api key already exists");
            }
            let row = ApiKeyStored {
                id: next_id(&dump.api_keys, |k| k.id),
                name: name.to_string(),
                key_hash: key_hash.to_string(),
                key_prefix: key_prefix.to_string(),
                enabled,
                created_at: now(),
                model_ids: model_ids.to_vec(),
            };
            dump.api_keys.push(row.clone());
            Ok(to_api_key(row))
        })
        .await
    }

    pub async fn update_api_key(
        &self,
        id: i64,
        name: &str,
        enabled: bool,
        model_ids: Option<&[i64]>,
    ) -> Result<Option<ApiKey>> {
        self.mutate(|dump| {
            let Some(row) = dump.api_keys.iter_mut().find(|k| k.id == id) else {
                return Ok(None);
            };
            row.name = name.to_string();
            row.enabled = enabled;
            if let Some(ids) = model_ids {
                row.model_ids = ids.to_vec();
            }
            Ok(Some(to_api_key(row.clone())))
        })
        .await
    }

    pub async fn rotate_api_key(
        &self,
        id: i64,
        key_hash: &str,
        key_prefix: &str,
    ) -> Result<Option<ApiKey>> {
        self.mutate(|dump| {
            let Some(row) = dump.api_keys.iter_mut().find(|k| k.id == id) else {
                return Ok(None);
            };
            row.key_hash = key_hash.to_string();
            row.key_prefix = key_prefix.to_string();
            Ok(Some(to_api_key(row.clone())))
        })
        .await
    }

    pub async fn delete_api_key(&self, id: i64) -> Result<()> {
        self.mutate(|dump| {
            dump.api_keys.retain(|k| k.id != id);
            Ok(())
        })
        .await
    }
}

fn strip_provider(row: ProviderSecret) -> Provider {
    Provider {
        id: row.id,
        name: row.name,
        base_url: row.base_url,
        enabled: row.enabled,
        created_at: row.created_at,
    }
}

fn to_api_key(row: ApiKeyStored) -> ApiKey {
    ApiKey {
        id: row.id,
        name: row.name,
        key_prefix: row.key_prefix,
        enabled: row.enabled,
        created_at: row.created_at,
        model_ids: row.model_ids,
    }
}
