//! In-memory snapshot of keys, models, and channels.
//! Routing is new-api style: pick a weighted random enabled channel for the
//! requested model, then failover to the rest before the first byte.

use anyhow::Result;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::catalog::{Catalog, RoutingChannel, RoutingSnapshot};
use crate::hash::hash_api_key;
use crate::upstream::{
    build_auth, build_extra_headers, chat_completions_url, h2c_parts, upstream_url,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    pub id: i64,
    pub provider_id: i64,
    pub base_url: String,
    /// Full chat/completions URL, computed once at snapshot build instead of
    /// re-formatted on every request.
    pub endpoint: String,
    /// `host:port` for the h2c pool key and Host header.
    pub authority: String,
    /// Path (+ query) sent on the h2 request.
    pub path: String,
    pub api_key: String,
    /// Pre-rendered credential header (`authorization: Bearer …` by default),
    /// `None` when the provider is configured with no credential.
    pub auth: Option<(http::HeaderName, http::HeaderValue)>,
    /// Static extra request headers, already parsed and validated.
    pub extra_headers: Vec<(http::HeaderName, http::HeaderValue)>,
    pub upstream_model: String,
    pub weight: i32,
    pub priority: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRoute {
    pub model_id: i64,
    pub name: String,
    /// `Arc` per channel so the per-request ordering clones pointers, not the
    /// five `String`s inside each channel.
    pub channels: Vec<Arc<Channel>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRecord {
    pub id: i64,
    pub allowed_model_ids: Option<Vec<i64>>,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub keys: HashMap<String, Arc<KeyRecord>>,
    pub models: HashMap<String, Arc<ModelRoute>>,
}

#[derive(Clone)]
pub struct Store {
    /// Routing snapshot, swapped wholesale by the reload loop.
    ///
    /// `ArcSwap` rather than `RwLock`, for a **structural** reason: with an
    /// `RwLock`, a `reload()` writer holds the lock while it swaps the snapshot
    /// (and, before this change, while it dropped the old one), so concurrent
    /// readers wait on it; with `ArcSwap` readers keep serving the old `Arc` and
    /// never wait. `load()` is a lock-free acquire.
    ///
    /// Not `mem::replace` under the same `RwLock`: that would move the old
    /// snapshot's drop outside the critical section, but the writer would still
    /// hold the lock while swapping, so readers would still wait at that moment.
    ///
    /// The CPU rationale ("the lock's cache line bounces between workers") is a
    /// **falsified hypothesis, not a claim**: measured at +0.15% (inside noise).
    /// Method, negative result and its limits: see
    /// `docs/dev/loadtest-newapi-controlled-2026-09.md` §12.6.
    inner: Arc<ArcSwap<Snapshot>>,
    catalog: Catalog,
}

impl Store {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(Snapshot::default())),
            catalog,
        }
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub async fn reload(&self) -> Result<()> {
        let routing = self.catalog.load_routing().await?;
        let next = snapshot_from_routing(routing)?;
        // Skip the swap when nothing changed. The snapshot is rebuilt above on every
        // reload anyway, so this is not about allocation: `ArcSwap`'s pointer store is
        // O(1), while this `==` is a deep compare. What it buys is that a genuine
        // swap happens *only* on real change — so the previous snapshot is not handed
        // to the reclaimer (it is dropped by whichever thread releases the last
        // `Guard`, see §12.6) every few seconds for nothing.
        if self.inner.load().as_ref() == &next {
            return Ok(());
        }
        self.inner.store(Arc::new(next));
        Ok(())
    }

    pub fn authenticate(&self, bearer: &str) -> Option<Arc<KeyRecord>> {
        let hash = hash_api_key(bearer);
        self.inner.load().keys.get(&hash).cloned()
    }

    pub fn model_names_for(&self, key: &KeyRecord) -> Vec<String> {
        let snap = self.inner.load();
        let mut names: Vec<String> = snap
            .models
            .values()
            .filter(|route| match &key.allowed_model_ids {
                Some(allow) => allow.contains(&route.model_id),
                None => true,
            })
            .map(|route| route.name.clone())
            .collect();
        names.sort();
        names
    }

    pub fn channels_for(&self, key: &KeyRecord, model: &str) -> Option<Vec<Arc<Channel>>> {
        let snap = self.inner.load();
        let route = snap.models.get(model)?;
        if let Some(allow) = &key.allowed_model_ids
            && !allow.contains(&route.model_id)
        {
            return None;
        }
        if route.channels.is_empty() {
            return None;
        }
        Some(order_channels(&route.channels))
    }
}

fn snapshot_from_routing(routing: RoutingSnapshot) -> Result<Snapshot> {
    let mut keys = HashMap::new();
    for k in routing.keys {
        keys.insert(
            k.key_hash,
            Arc::new(KeyRecord {
                id: k.id,
                allowed_model_ids: k.allowed_model_ids,
            }),
        );
    }
    let mut models = HashMap::new();
    for m in routing.models {
        // A malformed provider shape fails the whole reload: `reload()` keeps the
        // previous snapshot and logs, so bad config never reaches the hot path.
        let channels = m
            .channels
            .into_iter()
            .map(to_channel)
            .collect::<Result<Vec<_>>>()?;
        models.insert(
            m.name.clone(),
            Arc::new(ModelRoute {
                model_id: m.model_id,
                name: m.name,
                channels: channels.into_iter().map(Arc::new).collect(),
            }),
        );
    }
    Ok(Snapshot { keys, models })
}

fn to_channel(c: RoutingChannel) -> Result<Channel> {
    let chat_path = c.upstream.chat_path.trim();
    let endpoint = if chat_path.is_empty() {
        chat_completions_url(&c.base_url)
    } else {
        upstream_url(&c.base_url, chat_path)
    };
    let (authority, path) =
        h2c_parts(&endpoint).unwrap_or_else(|| (String::new(), "/v1/chat/completions".to_string()));
    let auth = build_auth(&c.upstream.auth_scheme, &c.upstream.auth_header, &c.api_key)
        .map_err(|e| anyhow::anyhow!("provider {} auth: {e}", c.provider_id))?;
    let extra_headers = build_extra_headers(&c.upstream.extra_headers)
        .map_err(|e| anyhow::anyhow!("provider {} headers: {e}", c.provider_id))?;
    Ok(Channel {
        id: c.id,
        provider_id: c.provider_id,
        endpoint,
        authority,
        path,
        base_url: c.base_url,
        api_key: c.api_key,
        auth,
        extra_headers,
        upstream_model: c.upstream_model,
        weight: c.weight,
        priority: c.priority,
    })
}

pub fn order_channels(channels: &[Arc<Channel>]) -> Vec<Arc<Channel>> {
    let min_p = channels.iter().map(|c| c.priority).min().unwrap_or(0);
    let first: Vec<&Arc<Channel>> = channels.iter().filter(|c| c.priority == min_p).collect();
    let rest: Vec<Arc<Channel>> = channels
        .iter()
        .filter(|c| c.priority != min_p)
        .cloned()
        .collect();
    let picked = weighted_pick(&first);
    let mut out = Vec::with_capacity(channels.len());
    if let Some(p) = picked {
        out.push(Arc::clone(p));
        for c in first {
            if c.id != p.id {
                out.push(Arc::clone(c));
            }
        }
    }
    out.extend(rest);
    out
}

fn weighted_pick<'a>(group: &[&'a Arc<Channel>]) -> Option<&'a Arc<Channel>> {
    let total: i32 = group.iter().map(|c| c.weight.max(1)).sum();
    if total <= 0 || group.is_empty() {
        return group.first().copied();
    }
    let mut r = rand::rng().random_range(0..total);
    for c in group {
        r -= c.weight.max(1);
        if r < 0 {
            return Some(*c);
        }
    }
    group.last().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::UpstreamShape;
    use crate::hash::{generate_api_key, key_prefix};

    fn ch(id: i64, w: i32, p: i32) -> Arc<Channel> {
        let base_url = format!("http://p{id}");
        let endpoint = chat_completions_url(&base_url);
        let (authority, path) = h2c_parts(&endpoint).unwrap_or_default();
        Arc::new(Channel {
            id,
            provider_id: id,
            base_url,
            endpoint,
            authority,
            path,
            api_key: String::new(),
            auth: None,
            extra_headers: Vec::new(),
            upstream_model: "m".into(),
            weight: w,
            priority: p,
        })
    }

    #[test]
    fn lower_priority_is_tried_after_higher() {
        let ordered = order_channels(&[ch(1, 1, 0), ch(2, 1, 10)]);
        assert_eq!(ordered[0].provider_id, 1);
        assert_eq!(ordered[1].provider_id, 2);
    }

    #[test]
    fn weight_one_group_stays_size() {
        let ordered = order_channels(&[ch(1, 1, 0), ch(2, 9, 0)]);
        assert_eq!(ordered.len(), 2);
        assert!(ordered.iter().any(|c| c.provider_id == 1));
        assert!(ordered.iter().any(|c| c.provider_id == 2));
    }

    #[test]
    fn first_group_always_tried_before_failover() {
        let ordered = order_channels(&[ch(3, 1, 20), ch(1, 1, 0), ch(2, 1, 0)]);
        assert!(ordered[0].priority == 0);
        assert_eq!(ordered.last().map(|c| c.provider_id), Some(3));
    }

    #[test]
    fn ordering_clones_pointers_not_channels() {
        let group = [ch(1, 1, 0), ch(2, 1, 0)];
        let ordered = order_channels(&group);
        assert!(
            ordered
                .iter()
                .all(|c| Arc::ptr_eq(c, &group[0]) || Arc::ptr_eq(c, &group[1])),
            "every ordered channel must be a pointer to an input, never a clone"
        );
    }

    /// 回归：`reload()` 换快照后，**已经拿到的 key 句柄**必须看到新路由、且旧路由消失。
    /// 这是 `RwLock` → `ArcSwap` 这次改动唯一的行为契约（读者不再被写者阻塞，但换代必须可见）。
    fn catalog_yaml(model: &str) -> String {
        format!(
            r#"providers:
  - name: p
    base_url: http://127.0.0.1:1
    api_key: ""
    enabled: true
models:
  - name: {model}
    enabled: true
    channels:
      - provider: p
        upstream_model: m
        weight: 1
        priority: 0
        enabled: true
api_keys:
  - name: k
    key: sk-test
    models: []
    enabled: true
"#
        )
    }

    #[tokio::test]
    async fn reload_swaps_the_snapshot_so_readers_see_the_new_routing() {
        use crate::catalog::{Catalog, yaml};
        use crate::config::Config;

        let cfg = Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: false,
            admin_password: "admin".into(),
            session_secret: None,
            max_conns_per_host: 8,
            h1_max_conns_per_host: 8,
            upstream_stream_cap: usize::MAX,
            catalog_reload_secs: 0,
            upstream_ttft_header: false,
        };
        let catalog = Catalog::connect(&cfg).await.expect("connect");
        yaml::import_yaml(&catalog, &catalog_yaml("alpha"), yaml::ImportMode::Replace)
            .await
            .expect("import alpha");
        let store = Store::new(catalog.clone());
        store.reload().await.expect("reload alpha");

        let key = store.authenticate("sk-test").expect("key survives reload");
        assert_eq!(
            store.channels_for(&key, "alpha").map(|c| c.len()),
            Some(1),
            "alpha must route after the first reload"
        );
        assert!(store.channels_for(&key, "beta").is_none());

        yaml::import_yaml(&catalog, &catalog_yaml("beta"), yaml::ImportMode::Replace)
            .await
            .expect("import beta");
        store.reload().await.expect("reload beta");

        assert!(
            store.channels_for(&key, "alpha").is_none(),
            "the old route must be gone after the swap"
        );
        assert_eq!(
            store.channels_for(&key, "beta").map(|c| c.len()),
            Some(1),
            "the same key handle must observe the new snapshot"
        );
    }

    async fn store_with_provider(shape: &UpstreamShape) -> (Store, Arc<Channel>, String) {
        use crate::catalog::Catalog;
        use crate::config::Config;

        let cfg = Config {
            database_url: "memory".into(),
            redis_url: None,
            listen: "127.0.0.1:0".into(),
            h2c: false,
            admin_password: "admin".into(),
            session_secret: None,
            max_conns_per_host: 8,
            h1_max_conns_per_host: 8,
            upstream_stream_cap: usize::MAX,
            catalog_reload_secs: 0,
            upstream_ttft_header: false,
        };
        let catalog = Catalog::connect(&cfg).await.expect("connect");
        let provider = catalog
            .create_provider("p", "http://127.0.0.1:9", "sk-provider", true, shape)
            .await
            .expect("provider");
        let model = catalog.create_model("gpt", true).await.expect("model");
        catalog
            .create_channel(model.id, provider.id, "gpt", 1, 0, true)
            .await
            .expect("channel");
        let key = generate_api_key();
        catalog
            .create_api_key("k", &hash_api_key(&key), &key_prefix(&key), true, &[])
            .await
            .expect("api key");
        let store = Store::new(catalog);
        store.reload().await.expect("reload");
        let record = store.authenticate(&key).expect("key");
        let channel = store.channels_for(&record, "gpt").expect("route")[0].clone();
        (store, channel, key)
    }

    /// The default shape must reproduce xlite's pre-feature request exactly.
    #[tokio::test]
    async fn default_provider_shape_yields_the_legacy_bearer_channel() {
        let (_, channel, _) = store_with_provider(&UpstreamShape::default()).await;
        let (name, value) = channel.auth.as_ref().expect("credential header");
        assert_eq!(name.as_str(), "authorization");
        assert_eq!(value.to_str().unwrap(), "Bearer sk-provider");
        assert!(
            channel.extra_headers.is_empty(),
            "no extra headers by default"
        );
        assert_eq!(channel.endpoint, "http://127.0.0.1:9/v1/chat/completions");
        assert_eq!(channel.path, "/v1/chat/completions");
    }

    #[tokio::test]
    async fn provider_shape_is_precomputed_into_the_channel() {
        let shape = UpstreamShape {
            auth_scheme: "raw".into(),
            auth_header: "X-Api-Key".into(),
            extra_headers: vec![crate::catalog::HeaderPair {
                name: "X-Title".into(),
                value: "xlite".into(),
            }],
            chat_path: "/api/chat".into(),
        };
        let (_, channel, _) = store_with_provider(&shape).await;
        let (name, value) = channel.auth.as_ref().expect("credential header");
        assert_eq!(name.as_str(), "x-api-key");
        assert_eq!(value.to_str().unwrap(), "sk-provider");
        assert_eq!(channel.extra_headers.len(), 1);
        assert_eq!(channel.extra_headers[0].0.as_str(), "x-title");
        assert_eq!(channel.endpoint, "http://127.0.0.1:9/api/chat");
        assert_eq!(channel.path, "/api/chat");
    }

    /// A shape that somehow reached the catalog must fail the reload loudly and
    /// leave the previous snapshot serving — bad config never hits the hot path.
    #[tokio::test]
    async fn malformed_provider_shape_fails_the_reload_and_keeps_the_old_snapshot() {
        let (store, _, key) = store_with_provider(&UpstreamShape::default()).await;
        let bad = UpstreamShape {
            extra_headers: vec![crate::catalog::HeaderPair {
                name: "Host".into(),
                value: "evil.example".into(),
            }],
            ..UpstreamShape::default()
        };
        let bad_provider = store
            .catalog()
            .create_provider("bad", "http://127.0.0.1:10", "sk", true, &bad)
            .await
            .expect("raw catalog write bypasses control-plane validation");
        let model = store.catalog().list_models().await.expect("models");
        store
            .catalog()
            .create_channel(model[0].id, bad_provider.id, "gpt", 1, 0, true)
            .await
            .expect("channel");

        let err = store.reload().await.expect_err("reserved header must fail");
        assert!(err.to_string().contains("reserved"), "{err}");

        let record = store.authenticate(&key).expect("old key still works");
        assert!(
            store.channels_for(&record, "gpt").is_some(),
            "the previous snapshot must keep serving after a failed reload"
        );
    }
}
