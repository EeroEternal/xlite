//! YAML catalog import/export. Scheduling is always weighted-random + priority.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Catalog;
use super::types::{HeaderPair, UpstreamShape};
use crate::hash::{generate_api_key, hash_api_key, key_prefix};
use crate::upstream;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CatalogYaml {
    #[serde(default)]
    pub providers: Vec<ProviderYaml>,
    #[serde(default)]
    pub models: Vec<ModelYaml>,
    #[serde(default)]
    pub api_keys: Vec<ApiKeyYaml>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderYaml {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(
        default = "default_auth_scheme",
        skip_serializing_if = "is_default_auth_scheme"
    )]
    pub auth_scheme: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub auth_header: String,
    /// Static extra request headers, in operator order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderPair>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chat_path: String,
}

fn default_auth_scheme() -> String {
    "bearer".to_string()
}

fn is_default_auth_scheme(value: &String) -> bool {
    value == "bearer"
}

impl ProviderYaml {
    fn upstream(&self) -> UpstreamShape {
        UpstreamShape {
            auth_scheme: self.auth_scheme.clone(),
            auth_header: self.auth_header.clone(),
            extra_headers: self.headers.clone(),
            chat_path: self.chat_path.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelYaml {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub channels: Vec<ChannelYaml>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelYaml {
    pub provider: String,
    pub upstream_model: String,
    #[serde(default = "default_weight")]
    pub weight: i32,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiKeyYaml {
    pub name: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_weight() -> i32 {
    1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportMode {
    Merge,
    Replace,
}

impl ImportMode {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "replace" => Self::Replace,
            _ => Self::Merge,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ImportCounts {
    pub created: u32,
    pub updated: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct CreatedSecret {
    pub name: String,
    pub key: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ImportReport {
    pub providers: ImportCounts,
    pub models: ImportCounts,
    pub channels: ImportCounts,
    pub api_keys: ImportCounts,
    pub secrets: Vec<CreatedSecret>,
}

pub fn parse_yaml(text: &str) -> Result<CatalogYaml> {
    serde_yaml::from_str(text).context("parse catalog yaml")
}

pub fn to_yaml(doc: &CatalogYaml) -> Result<String> {
    serde_yaml::to_string(doc).context("encode catalog yaml")
}

pub async fn export_yaml(catalog: &Catalog) -> Result<String> {
    to_yaml(&export_doc(catalog).await?)
}

pub async fn export_doc(catalog: &Catalog) -> Result<CatalogYaml> {
    let providers = catalog.list_provider_secrets().await?;
    let models = catalog.list_models().await?;
    let channels = catalog.list_channels().await?;
    let mut model_docs = Vec::new();
    for model in models {
        let mut chs: Vec<ChannelYaml> = channels
            .iter()
            .filter(|c| c.model_id == model.id)
            .filter_map(|c| {
                let provider = providers.iter().find(|p| p.id == c.provider_id)?;
                Some(ChannelYaml {
                    provider: provider.name.clone(),
                    upstream_model: c.upstream_model.clone(),
                    weight: c.weight,
                    priority: c.priority,
                    enabled: c.enabled,
                })
            })
            .collect();
        chs.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then(a.provider.cmp(&b.provider))
        });
        model_docs.push(ModelYaml {
            name: model.name,
            enabled: model.enabled,
            channels: chs,
        });
    }
    model_docs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut provider_docs: Vec<ProviderYaml> = providers
        .into_iter()
        .map(|p| ProviderYaml {
            name: p.name,
            base_url: p.base_url,
            api_key: p.api_key,
            enabled: p.enabled,
            auth_scheme: p.upstream.auth_scheme,
            auth_header: p.upstream.auth_header,
            headers: p.upstream.extra_headers,
            chat_path: p.upstream.chat_path,
        })
        .collect();
    provider_docs.sort_by(|a, b| a.name.cmp(&b.name));
    let models = catalog.list_models().await?;
    let api_keys = catalog
        .list_api_keys()
        .await?
        .into_iter()
        .map(|key| ApiKeyYaml {
            name: key.name,
            key: None,
            models: key
                .model_ids
                .iter()
                .filter_map(|id| models.iter().find(|m| m.id == *id).map(|m| m.name.clone()))
                .collect(),
            enabled: key.enabled,
        })
        .collect();
    Ok(CatalogYaml {
        providers: provider_docs,
        models: model_docs,
        api_keys,
    })
}

pub async fn import_yaml(catalog: &Catalog, text: &str, mode: ImportMode) -> Result<ImportReport> {
    let doc = parse_yaml(text)?;
    import_doc(catalog, doc, mode).await
}

pub async fn import_doc(
    catalog: &Catalog,
    doc: CatalogYaml,
    mode: ImportMode,
) -> Result<ImportReport> {
    if matches!(mode, ImportMode::Replace) {
        catalog.wipe().await?;
    }
    let mut report = ImportReport::default();
    for provider in &doc.providers {
        if provider.name.trim().is_empty() {
            bail!("provider name is empty");
        }
        let upstream = provider.upstream();
        upstream::validate_upstream(&upstream, &provider.api_key)
            .map_err(|e| anyhow::anyhow!("provider {}: {e}", provider.name))?;
        let existing = catalog
            .list_providers()
            .await?
            .into_iter()
            .find(|p| p.name == provider.name);
        if let Some(row) = existing {
            catalog
                .update_provider(
                    row.id,
                    &provider.name,
                    &provider.base_url,
                    &provider.api_key,
                    provider.enabled,
                    &upstream,
                )
                .await?;
            report.providers.updated += 1;
        } else {
            catalog
                .create_provider(
                    &provider.name,
                    &provider.base_url,
                    &provider.api_key,
                    provider.enabled,
                    &upstream,
                )
                .await?;
            report.providers.created += 1;
        }
    }

    let providers = catalog.list_providers().await?;
    for model in &doc.models {
        if model.name.trim().is_empty() {
            bail!("model name is empty");
        }
        let existing = catalog
            .list_models()
            .await?
            .into_iter()
            .find(|m| m.name == model.name);
        let model_id = if let Some(row) = existing {
            catalog
                .update_model(row.id, &model.name, model.enabled)
                .await?;
            report.models.updated += 1;
            row.id
        } else {
            let row = catalog.create_model(&model.name, model.enabled).await?;
            report.models.created += 1;
            row.id
        };
        let channels = catalog.list_channels().await?;
        for ch in &model.channels {
            let Some(provider) = providers.iter().find(|p| p.name == ch.provider) else {
                bail!("channel provider {} not found", ch.provider);
            };
            let existing = channels.iter().find(|c| {
                c.model_id == model_id
                    && c.provider_id == provider.id
                    && c.upstream_model == ch.upstream_model
            });
            if let Some(row) = existing {
                catalog
                    .update_channel(
                        row.id,
                        model_id,
                        provider.id,
                        &ch.upstream_model,
                        ch.weight,
                        ch.priority,
                        ch.enabled,
                    )
                    .await?;
                report.channels.updated += 1;
            } else {
                catalog
                    .create_channel(
                        model_id,
                        provider.id,
                        &ch.upstream_model,
                        ch.weight,
                        ch.priority,
                        ch.enabled,
                    )
                    .await?;
                report.channels.created += 1;
            }
        }
    }

    let models = catalog.list_models().await?;
    for key in &doc.api_keys {
        if key.name.trim().is_empty() {
            bail!("api key name is empty");
        }
        let mut model_ids = Vec::new();
        for name in &key.models {
            let Some(model) = models.iter().find(|m| m.name == *name) else {
                bail!("api key model {name} not found");
            };
            model_ids.push(model.id);
        }
        let existing = catalog
            .list_api_keys()
            .await?
            .into_iter()
            .find(|k| k.name == key.name);
        if let Some(row) = existing {
            catalog
                .update_api_key(row.id, &key.name, key.enabled, Some(&model_ids))
                .await?;
            if let Some(plaintext) = key.key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let hash = hash_api_key(plaintext);
                let prefix = key_prefix(plaintext);
                catalog.rotate_api_key(row.id, &hash, &prefix).await?;
                report.secrets.push(CreatedSecret {
                    name: key.name.clone(),
                    key: plaintext.to_string(),
                });
            }
            report.api_keys.updated += 1;
        } else {
            let plaintext = match key.key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(k) => k.to_string(),
                None => generate_api_key(),
            };
            let hash = hash_api_key(&plaintext);
            let prefix = key_prefix(&plaintext);
            catalog
                .create_api_key(&key.name, &hash, &prefix, key.enabled, &model_ids)
                .await?;
            report.secrets.push(CreatedSecret {
                name: key.name.clone(),
                key: plaintext,
            });
            report.api_keys.created += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    async fn memory_catalog() -> Catalog {
        let cfg = Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: false,
            admin_password: "admin".into(),
            session_secret: None,
            max_conns_per_host: 8,
            catalog_reload_secs: 0,
            upstream_ttft_header: false,
        };
        Catalog::connect(&cfg).await.expect("connect")
    }

    const BARE: &str = r#"
providers:
  - name: p
    base_url: http://h
models: []
api_keys: []
"#;

    /// An untouched catalog must keep exporting exactly the keys it used to.
    #[tokio::test]
    async fn default_upstream_shape_is_not_exported() {
        let catalog = memory_catalog().await;
        import_yaml(&catalog, BARE, ImportMode::Replace)
            .await
            .expect("import");
        let text = export_yaml(&catalog).await.expect("export");
        for key in ["auth_scheme", "auth_header", "headers", "chat_path"] {
            assert!(
                !text.contains(key),
                "default shape must not add `{key}` to the export:\n{text}"
            );
        }
    }

    #[tokio::test]
    async fn upstream_shape_round_trips_through_yaml() {
        let catalog = memory_catalog().await;
        let text = r#"
providers:
  - name: p
    base_url: http://h
    api_key: sk-1
    auth_scheme: raw
    auth_header: X-Api-Key
    headers:
      - name: X-Title
        value: xlite
    chat_path: /api/chat
models: []
api_keys: []
"#;
        import_yaml(&catalog, text, ImportMode::Replace)
            .await
            .expect("import");
        let out = export_yaml(&catalog).await.expect("export");
        assert!(out.contains("auth_scheme: raw"), "{out}");
        assert!(out.contains("X-Api-Key"), "{out}");
        assert!(out.contains("/api/chat"), "{out}");

        // The export must be re-importable unchanged.
        import_yaml(&catalog, &out, ImportMode::Replace)
            .await
            .expect("re-import the export");
        let providers = catalog.list_providers().await.expect("providers");
        assert_eq!(providers[0].upstream.auth_scheme, "raw");
        assert_eq!(providers[0].upstream.auth_header, "X-Api-Key");
        assert_eq!(providers[0].upstream.extra_headers.len(), 1);
        assert_eq!(providers[0].upstream.chat_path, "/api/chat");
    }

    /// Import refuses a reserved header instead of storing a provider that the
    /// snapshot build would later reject.
    #[tokio::test]
    async fn reserved_headers_are_rejected_on_import() {
        let catalog = memory_catalog().await;
        let text = r#"
providers:
  - name: p
    base_url: http://h
    headers:
      - name: Host
        value: evil.example
models: []
api_keys: []
"#;
        let err = import_yaml(&catalog, text, ImportMode::Replace)
            .await
            .expect_err("reserved header must be rejected");
        assert!(err.to_string().contains("reserved"), "{err}");
    }
}
