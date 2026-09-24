//! In-memory snapshot of keys, models, and channels. Reloaded from Postgres.
//! Routing is new-api style: pick a weighted random enabled channel for the
//! requested model, then failover to the rest before the first byte.

use anyhow::Result;
use rand::Rng;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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
    pool: PgPool,
}

#[derive(sqlx::FromRow)]
struct KeyRow {
    id: i64,
    key_hash: String,
}

#[derive(sqlx::FromRow)]
struct BindingRow {
    api_key_id: i64,
    model_id: i64,
}

#[derive(sqlx::FromRow)]
struct ModelRow {
    id: i64,
    name: String,
}

#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: i64,
    model_id: i64,
    provider_id: i64,
    upstream_model: String,
    weight: i32,
    priority: i32,
    base_url: String,
    api_key: String,
}

impl Store {
    pub fn new(pool: PgPool) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Snapshot::default())),
            pool,
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn reload(&self) -> Result<()> {
        let keys =
            sqlx::query_as::<_, KeyRow>("SELECT id, key_hash FROM api_keys WHERE enabled = TRUE")
                .fetch_all(&self.pool)
                .await?;
        let bindings =
            sqlx::query_as::<_, BindingRow>("SELECT api_key_id, model_id FROM api_key_models")
                .fetch_all(&self.pool)
                .await?;
        let mut allowed: HashMap<i64, Vec<i64>> = HashMap::new();
        for b in &bindings {
            allowed.entry(b.api_key_id).or_default().push(b.model_id);
        }
        let mut key_map = HashMap::new();
        for k in keys {
            key_map.insert(
                k.key_hash,
                KeyRecord {
                    id: k.id,
                    allowed_model_ids: allowed.get(&k.id).cloned(),
                },
            );
        }

        let models =
            sqlx::query_as::<_, ModelRow>("SELECT id, name FROM models WHERE enabled = TRUE")
                .fetch_all(&self.pool)
                .await?;
        let channels = sqlx::query_as::<_, ChannelRow>(
            r#"
            SELECT c.id, c.model_id, c.provider_id, c.upstream_model, c.weight, c.priority,
                   p.base_url, p.api_key
            FROM channels c
            JOIN providers p ON p.id = c.provider_id
            WHERE c.enabled = TRUE AND p.enabled = TRUE
            ORDER BY c.priority ASC, c.id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut by_model: HashMap<i64, Vec<Channel>> = HashMap::new();
        for c in channels {
            by_model.entry(c.model_id).or_default().push(Channel {
                id: c.id,
                provider_id: c.provider_id,
                base_url: c.base_url,
                api_key: c.api_key,
                upstream_model: c.upstream_model,
                weight: c.weight,
                priority: c.priority,
            });
        }
        let mut model_map = HashMap::new();
        for m in models {
            if let Some(chs) = by_model.remove(&m.id) {
                model_map.insert(
                    m.name.clone(),
                    ModelRoute {
                        model_id: m.id,
                        name: m.name,
                        channels: chs,
                    },
                );
            }
        }

        *self.inner.write().expect("snapshot lock") = Snapshot {
            keys: key_map,
            models: model_map,
        };
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

    /// Weighted random among the first priority group, then the remaining
    /// channels in priority order for failover. Same idea as new-api channel groups.
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
