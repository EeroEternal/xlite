mod redis;
mod sqlite;
pub mod types;
pub mod yaml;

use anyhow::Result;

use crate::config::{Config, is_memory_url};
use redis::RedisCatalog;
use sqlite::SqliteCatalog;

pub use types::*;

#[derive(Clone)]
enum Inner {
    Sqlite(SqliteCatalog),
    Redis(RedisCatalog),
}

#[derive(Clone)]
pub struct Catalog {
    inner: Inner,
    kind: StorageKind,
}

impl Catalog {
    pub async fn connect(cfg: &Config) -> Result<Self> {
        if let Some(url) = &cfg.redis_url {
            tracing::info!(url, "catalog backend redis");
            return Ok(Self {
                inner: Inner::Redis(RedisCatalog::connect(url).await?),
                kind: StorageKind::Redis,
            });
        }
        let kind = if is_memory_url(&cfg.database_url) {
            StorageKind::Memory
        } else {
            StorageKind::Sqlite
        };
        tracing::info!(url = %cfg.database_url, backend = kind.as_str(), "catalog backend sqlite");
        Ok(Self {
            inner: Inner::Sqlite(SqliteCatalog::connect(&cfg.database_url).await?),
            kind,
        })
    }

    pub fn kind(&self) -> StorageKind {
        self.kind
    }

    pub async fn ping(&self) -> Result<()> {
        match &self.inner {
            Inner::Sqlite(c) => c.ping().await,
            Inner::Redis(c) => c.ping().await,
        }
    }

    pub async fn overview_counts(&self) -> Result<OverviewCounts> {
        match &self.inner {
            Inner::Sqlite(c) => c.overview_counts().await,
            Inner::Redis(c) => c.overview_counts().await,
        }
    }

    pub async fn load_routing(&self) -> Result<RoutingSnapshot> {
        match &self.inner {
            Inner::Sqlite(c) => c.load_routing().await,
            Inner::Redis(c) => c.load_routing().await,
        }
    }

    pub async fn list_providers(&self) -> Result<Vec<Provider>> {
        match &self.inner {
            Inner::Sqlite(c) => c.list_providers().await,
            Inner::Redis(c) => c.list_providers().await,
        }
    }

    pub async fn list_provider_secrets(&self) -> Result<Vec<ProviderSecret>> {
        match &self.inner {
            Inner::Sqlite(c) => c.list_provider_secrets().await,
            Inner::Redis(c) => c.list_provider_secrets().await,
        }
    }

    pub async fn wipe(&self) -> Result<()> {
        match &self.inner {
            Inner::Sqlite(c) => c.wipe().await,
            Inner::Redis(c) => c.wipe().await,
        }
    }

    pub async fn create_provider(
        &self,
        name: &str,
        base_url: &str,
        api_key: &str,
        enabled: bool,
        upstream: &UpstreamShape,
    ) -> Result<Provider> {
        match &self.inner {
            Inner::Sqlite(c) => {
                c.create_provider(name, base_url, api_key, enabled, upstream)
                    .await
            }
            Inner::Redis(c) => {
                c.create_provider(name, base_url, api_key, enabled, upstream)
                    .await
            }
        }
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
        match &self.inner {
            Inner::Sqlite(c) => {
                c.update_provider(id, name, base_url, api_key, enabled, upstream)
                    .await
            }
            Inner::Redis(c) => {
                c.update_provider(id, name, base_url, api_key, enabled, upstream)
                    .await
            }
        }
    }

    pub async fn delete_provider(&self, id: i64) -> Result<()> {
        match &self.inner {
            Inner::Sqlite(c) => c.delete_provider(id).await,
            Inner::Redis(c) => c.delete_provider(id).await,
        }
    }

    pub async fn list_models(&self) -> Result<Vec<Model>> {
        match &self.inner {
            Inner::Sqlite(c) => c.list_models().await,
            Inner::Redis(c) => c.list_models().await,
        }
    }

    pub async fn create_model(&self, name: &str, enabled: bool) -> Result<Model> {
        match &self.inner {
            Inner::Sqlite(c) => c.create_model(name, enabled).await,
            Inner::Redis(c) => c.create_model(name, enabled).await,
        }
    }

    pub async fn update_model(&self, id: i64, name: &str, enabled: bool) -> Result<Option<Model>> {
        match &self.inner {
            Inner::Sqlite(c) => c.update_model(id, name, enabled).await,
            Inner::Redis(c) => c.update_model(id, name, enabled).await,
        }
    }

    pub async fn delete_model(&self, id: i64) -> Result<()> {
        match &self.inner {
            Inner::Sqlite(c) => c.delete_model(id).await,
            Inner::Redis(c) => c.delete_model(id).await,
        }
    }

    pub async fn list_channels(&self) -> Result<Vec<Channel>> {
        match &self.inner {
            Inner::Sqlite(c) => c.list_channels().await,
            Inner::Redis(c) => c.list_channels().await,
        }
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
        match &self.inner {
            Inner::Sqlite(c) => {
                c.create_channel(
                    model_id,
                    provider_id,
                    upstream_model,
                    weight,
                    priority,
                    enabled,
                )
                .await
            }
            Inner::Redis(c) => {
                c.create_channel(
                    model_id,
                    provider_id,
                    upstream_model,
                    weight,
                    priority,
                    enabled,
                )
                .await
            }
        }
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
        match &self.inner {
            Inner::Sqlite(c) => {
                c.update_channel(
                    id,
                    model_id,
                    provider_id,
                    upstream_model,
                    weight,
                    priority,
                    enabled,
                )
                .await
            }
            Inner::Redis(c) => {
                c.update_channel(
                    id,
                    model_id,
                    provider_id,
                    upstream_model,
                    weight,
                    priority,
                    enabled,
                )
                .await
            }
        }
    }

    pub async fn delete_channel(&self, id: i64) -> Result<()> {
        match &self.inner {
            Inner::Sqlite(c) => c.delete_channel(id).await,
            Inner::Redis(c) => c.delete_channel(id).await,
        }
    }

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>> {
        match &self.inner {
            Inner::Sqlite(c) => c.list_api_keys().await,
            Inner::Redis(c) => c.list_api_keys().await,
        }
    }

    pub async fn create_api_key(
        &self,
        name: &str,
        key_hash: &str,
        key_prefix: &str,
        enabled: bool,
        model_ids: &[i64],
    ) -> Result<ApiKey> {
        match &self.inner {
            Inner::Sqlite(c) => {
                c.create_api_key(name, key_hash, key_prefix, enabled, model_ids)
                    .await
            }
            Inner::Redis(c) => {
                c.create_api_key(name, key_hash, key_prefix, enabled, model_ids)
                    .await
            }
        }
    }

    pub async fn update_api_key(
        &self,
        id: i64,
        name: &str,
        enabled: bool,
        model_ids: Option<&[i64]>,
    ) -> Result<Option<ApiKey>> {
        match &self.inner {
            Inner::Sqlite(c) => c.update_api_key(id, name, enabled, model_ids).await,
            Inner::Redis(c) => c.update_api_key(id, name, enabled, model_ids).await,
        }
    }

    pub async fn rotate_api_key(
        &self,
        id: i64,
        key_hash: &str,
        key_prefix: &str,
    ) -> Result<Option<ApiKey>> {
        match &self.inner {
            Inner::Sqlite(c) => c.rotate_api_key(id, key_hash, key_prefix).await,
            Inner::Redis(c) => c.rotate_api_key(id, key_hash, key_prefix).await,
        }
    }

    pub async fn delete_api_key(&self, id: i64) -> Result<()> {
        match &self.inner {
            Inner::Sqlite(c) => c.delete_api_key(id).await,
            Inner::Redis(c) => c.delete_api_key(id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_cfg() -> Config {
        Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: false,
            admin_password: "admin".into(),
            session_secret: None,
            max_conns_per_host: 8,
            catalog_reload_secs: 5,
            upstream_ttft_header: false,
        }
    }

    #[tokio::test]
    async fn sqlite_memory_provider_and_channel() {
        let catalog = Catalog::connect(&test_cfg()).await.expect("connect");
        assert_eq!(catalog.kind(), StorageKind::Memory);
        catalog.ping().await.expect("ping");
        let provider = catalog
            .create_provider(
                "p1",
                "http://127.0.0.1:9",
                "sk",
                true,
                &UpstreamShape::default(),
            )
            .await
            .expect("provider");
        let model = catalog.create_model("gpt", true).await.expect("model");
        let channel = catalog
            .create_channel(model.id, provider.id, "gpt", 2, 0, true)
            .await
            .expect("channel");
        assert_eq!(channel.weight, 2);
        let snap = catalog.load_routing().await.expect("routing");
        assert_eq!(snap.models.len(), 1);
        assert_eq!(snap.models[0].channels[0].base_url, "http://127.0.0.1:9");
        let counts = catalog.overview_counts().await.expect("counts");
        assert_eq!(counts.providers, 1);
        assert_eq!(counts.models, 1);
        assert_eq!(counts.channels, 1);
    }

    #[tokio::test]
    async fn yaml_import_one_key_many_providers() {
        let catalog = Catalog::connect(&test_cfg()).await.expect("connect");
        let yaml = r#"
providers:
  - name: mock-a
    base_url: http://127.0.0.1:4101
  - name: mock-b
    base_url: http://127.0.0.1:4102
models:
  - name: mock
    channels:
      - provider: mock-a
        upstream_model: mock
        weight: 1
      - provider: mock-b
        upstream_model: mock
        weight: 1
api_keys:
  - name: loadtest
    models: []
"#;
        let report = yaml::import_yaml(&catalog, yaml, yaml::ImportMode::Replace)
            .await
            .expect("import");
        assert_eq!(report.providers.created, 2);
        assert_eq!(report.models.created, 1);
        assert_eq!(report.channels.created, 2);
        assert_eq!(report.api_keys.created, 1);
        assert_eq!(report.secrets.len(), 1);
        let snap = catalog.load_routing().await.expect("routing");
        assert_eq!(snap.models[0].channels.len(), 2);
        let exported = yaml::export_yaml(&catalog).await.expect("export");
        assert!(exported.contains("mock-a"));
        assert!(exported.contains("mock-b"));
        let report2 = yaml::import_yaml(&catalog, &exported, yaml::ImportMode::Merge)
            .await
            .expect("reimport");
        assert_eq!(report2.providers.updated, 2);
        assert_eq!(report2.channels.updated, 2);
    }
}
