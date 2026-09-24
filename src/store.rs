//! In-memory snapshot of keys, models, and channels.
//! Routing is new-api style: pick a weighted random enabled channel for the
//! requested model, then failover to the rest before the first byte.

use anyhow::Result;
use rand::Rng;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::catalog::{Catalog, RoutingChannel, RoutingSnapshot};
use crate::hash::hash_api_key;
use crate::upstream::{authorization_header, chat_completions_url, h2c_parts};

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
    /// Pre-rendered `Authorization` value (`Bearer …`), empty when unused.
    pub authorization: String,
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
    inner: Arc<RwLock<Snapshot>>,
    catalog: Catalog,
}

impl Store {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Snapshot::default())),
            catalog,
        }
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub async fn reload(&self) -> Result<()> {
        let routing = self.catalog.load_routing().await?;
        let next = snapshot_from_routing(routing);
        // Skip the write-lock swap when nothing changed: the reload loop fires
        // every few seconds and a writer on this read-hot lock stalls request
        // tasks momentarily even when the routing is identical.
        {
            let current = self.inner.read().expect("snapshot lock");
            if *current == next {
                return Ok(());
            }
        }
        *self.inner.write().expect("snapshot lock") = next;
        Ok(())
    }

    pub fn authenticate(&self, bearer: &str) -> Option<Arc<KeyRecord>> {
        let hash = hash_api_key(bearer);
        self.inner.read().ok()?.keys.get(&hash).cloned()
    }

    pub fn model_names_for(&self, key: &KeyRecord) -> Vec<String> {
        let Ok(snap) = self.inner.read() else {
            return Vec::new();
        };
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
        let snap = self.inner.read().ok()?;
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

fn snapshot_from_routing(routing: RoutingSnapshot) -> Snapshot {
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
        models.insert(
            m.name.clone(),
            Arc::new(ModelRoute {
                model_id: m.model_id,
                name: m.name,
                channels: m
                    .channels
                    .into_iter()
                    .map(to_channel)
                    .map(Arc::new)
                    .collect(),
            }),
        );
    }
    Snapshot { keys, models }
}

fn to_channel(c: RoutingChannel) -> Channel {
    let endpoint = chat_completions_url(&c.base_url);
    let (authority, path) =
        h2c_parts(&endpoint).unwrap_or_else(|| (String::new(), "/v1/chat/completions".to_string()));
    let authorization = authorization_header(&c.api_key);
    Channel {
        id: c.id,
        provider_id: c.provider_id,
        endpoint,
        authority,
        path,
        base_url: c.base_url,
        api_key: c.api_key,
        authorization,
        upstream_model: c.upstream_model,
        weight: c.weight,
        priority: c.priority,
    }
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
            authorization: String::new(),
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
}
