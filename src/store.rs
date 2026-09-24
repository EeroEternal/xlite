//! In-memory snapshot of keys, models, and channels.
//! Routing is new-api style: pick a weighted random enabled channel for the
//! requested model, then failover to the rest before the first byte.

use anyhow::Result;
use rand::Rng;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::catalog::{Catalog, RoutingChannel, RoutingSnapshot};
use crate::hash::hash_api_key;

#[derive(Clone, Debug)]
pub struct Channel {
    pub id: i64,
    pub provider_id: i64,
    pub base_url: String,
    pub api_key: String,
    pub upstream_model: String,
    pub weight: i32,
    pub priority: i32,
}

#[derive(Clone, Debug)]
pub struct ModelRoute {
    pub model_id: i64,
    pub name: String,
    pub channels: Vec<Channel>,
}

#[derive(Clone, Debug)]
pub struct KeyRecord {
    pub id: i64,
    pub allowed_model_ids: Option<Vec<i64>>,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub keys: HashMap<String, KeyRecord>,
    pub models: HashMap<String, ModelRoute>,
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
        *self.inner.write().expect("snapshot lock") = snapshot_from_routing(routing);
        Ok(())
    }

    pub fn authenticate(&self, bearer: &str) -> Option<KeyRecord> {
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

    pub fn channels_for(&self, key: &KeyRecord, model: &str) -> Option<Vec<Channel>> {
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
            KeyRecord {
                id: k.id,
                allowed_model_ids: k.allowed_model_ids,
            },
        );
    }
    let mut models = HashMap::new();
    for m in routing.models {
        models.insert(
            m.name.clone(),
            ModelRoute {
                model_id: m.model_id,
                name: m.name,
                channels: m.channels.into_iter().map(to_channel).collect(),
            },
        );
    }
    Snapshot { keys, models }
}

fn to_channel(c: RoutingChannel) -> Channel {
    Channel {
        id: c.id,
        provider_id: c.provider_id,
        base_url: c.base_url,
        api_key: c.api_key,
        upstream_model: c.upstream_model,
        weight: c.weight,
        priority: c.priority,
    }
}

pub fn order_channels(channels: &[Channel]) -> Vec<Channel> {
    let min_p = channels.iter().map(|c| c.priority).min().unwrap_or(0);
    let first: Vec<&Channel> = channels.iter().filter(|c| c.priority == min_p).collect();
    let rest: Vec<Channel> = channels
        .iter()
        .filter(|c| c.priority != min_p)
        .cloned()
        .collect();
    let picked = weighted_pick(&first);
    let mut out = Vec::with_capacity(channels.len());
    if let Some(p) = picked {
        out.push(p.clone());
        for c in first {
            if c.id != p.id {
                out.push((*c).clone());
            }
        }
    }
    out.extend(rest);
    out
}

fn weighted_pick<'a>(group: &[&'a Channel]) -> Option<&'a Channel> {
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

    fn ch(id: i64, w: i32, p: i32) -> Channel {
        Channel {
            id,
            provider_id: id,
            base_url: format!("http://p{id}"),
            api_key: String::new(),
            upstream_model: "m".into(),
            weight: w,
            priority: p,
        }
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
}
