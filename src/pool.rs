use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, TimeZone, Utc};
use rand::seq::SliceRandom;
use serde::Serialize;

use crate::store::{KeyRecord, STORE_VERSION, StoreFile};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStatus {
    Available,
    Cooling,
    Exhausted,
    Disabled,
    Invalid,
}

pub struct KeyEntry {
    pub id: u64,
    pub key: String,
    alias: Mutex<String>,
    egress_id: Mutex<Option<String>>,
    total_quota: AtomicU32,
    used_quota: AtomicU32,
    active: AtomicBool,
    invalid: AtomicBool,
    cooldown_until_ms: AtomicU64,
    /// 0 = never used.
    last_used_ms: AtomicU64,
    pub created_at: DateTime<Utc>,
}

impl std::fmt::Debug for KeyEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyEntry")
            .field("id", &self.id)
            .field("key", &mask_key(&self.key))
            .field("alias", &self.alias())
            .finish()
    }
}

impl KeyEntry {
    fn from_record(r: KeyRecord) -> Self {
        Self {
            id: r.id,
            key: r.key,
            alias: Mutex::new(r.alias),
            egress_id: Mutex::new(r.egress_id),
            total_quota: AtomicU32::new(r.total_quota),
            used_quota: AtomicU32::new(r.used_quota.min(r.total_quota)),
            active: AtomicBool::new(r.active && !r.invalid),
            invalid: AtomicBool::new(r.invalid),
            cooldown_until_ms: AtomicU64::new(0),
            last_used_ms: AtomicU64::new(
                r.last_used_at
                    .map(|t| t.timestamp_millis().max(0) as u64)
                    .unwrap_or(0),
            ),
            created_at: r.created_at,
        }
    }

    fn to_record(&self) -> KeyRecord {
        KeyRecord {
            id: self.id,
            key: self.key.clone(),
            alias: self.alias(),
            egress_id: self.egress_id(),
            total_quota: self.total_quota(),
            used_quota: self.used_quota(),
            active: self.is_active(),
            invalid: self.is_invalid(),
            created_at: self.created_at,
            last_used_at: self.last_used_at(),
        }
    }

    pub fn alias(&self) -> String {
        self.alias.lock().map(|a| a.clone()).unwrap_or_default()
    }
    pub fn egress_id(&self) -> Option<String> {
        self.egress_id.lock().unwrap().clone()
    }
    pub fn total_quota(&self) -> u32 {
        self.total_quota.load(Ordering::Relaxed)
    }
    pub fn used_quota(&self) -> u32 {
        self.used_quota.load(Ordering::Relaxed)
    }
    pub fn remaining(&self) -> u32 {
        self.total_quota().saturating_sub(self.used_quota())
    }
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
    pub fn is_invalid(&self) -> bool {
        self.invalid.load(Ordering::Relaxed)
    }
    pub fn cooldown_until_ms(&self) -> u64 {
        self.cooldown_until_ms.load(Ordering::Relaxed)
    }
    pub fn last_used_at(&self) -> Option<DateTime<Utc>> {
        match self.last_used_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Utc.timestamp_millis_opt(ms as i64).single(),
        }
    }

    pub fn status(&self, now_ms: u64) -> KeyStatus {
        if self.is_invalid() {
            KeyStatus::Invalid
        } else if !self.is_active() {
            KeyStatus::Disabled
        } else if self.remaining() == 0 {
            KeyStatus::Exhausted
        } else if self.cooldown_until_ms() > now_ms {
            KeyStatus::Cooling
        } else {
            KeyStatus::Available
        }
    }

    fn usable(&self) -> bool {
        self.is_active() && !self.is_invalid() && self.remaining() > 0
    }
}

pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 12 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..8].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

#[derive(Debug, Default, Clone)]
pub struct KeyPatch {
    pub alias: Option<String>,
    pub total_quota: Option<u32>,
    pub used_quota: Option<u32>,
    pub active: Option<bool>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PoolError {
    #[error("key already exists")]
    Duplicate,
    #[error("key not found")]
    NotFound,
}

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub key_count: usize,
    pub available: usize,
    pub cooling: usize,
    pub exhausted: usize,
    pub disabled: usize,
    pub invalid: usize,
    pub total_quota: u64,
    pub used_quota: u64,
    pub remaining_quota: u64,
}

struct Inner {
    keys: Vec<Arc<KeyEntry>>,
    next_id: u64,
    month: Option<String>,
    master_key: Option<String>,
    egress_ids: Vec<String>,
}

pub struct Pool {
    inner: RwLock<Inner>,
    dirty: AtomicBool,
}

fn least_loaded_egress(inner: &Inner) -> Option<String> {
    inner
        .egress_ids
        .iter()
        .min_by_key(|id| {
            inner
                .keys
                .iter()
                .filter(|k| k.egress_id().as_ref() == Some(id))
                .count()
        })
        .cloned()
}

impl Pool {
    pub fn from_store(file: StoreFile) -> Self {
        let max_id = file.keys.iter().map(|k| k.id).max().unwrap_or(0);
        let keys = file
            .keys
            .into_iter()
            .map(|r| Arc::new(KeyEntry::from_record(r)))
            .collect();
        Self {
            inner: RwLock::new(Inner {
                keys,
                next_id: file.next_id.max(max_id + 1),
                month: file.month,
                master_key: file.master_key,
                egress_ids: Vec::new(),
            }),
            dirty: AtomicBool::new(false),
        }
    }

    pub fn snapshot(&self) -> StoreFile {
        let inner = self.inner.read().unwrap();
        StoreFile {
            version: STORE_VERSION,
            month: inner.month.clone(),
            master_key: inner.master_key.clone(),
            next_id: inner.next_id,
            keys: inner.keys.iter().map(|k| k.to_record()).collect(),
        }
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }
    pub fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Release);
    }
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    pub fn stored_master_key(&self) -> Option<String> {
        self.inner.read().unwrap().master_key.clone()
    }

    /// Preserve established exits; distribute only unassigned keys to the least loaded exit.
    pub fn configure_egress(&self, ids: Vec<String>) -> anyhow::Result<()> {
        let mut inner = self.inner.write().unwrap();
        for key in &inner.keys {
            if let Some(id) = key.egress_id() {
                anyhow::ensure!(
                    ids.contains(&id),
                    "key #{} references missing egress {id}",
                    key.id
                );
            }
        }
        inner.egress_ids = ids;
        for key in &inner.keys {
            if key.egress_id().is_none() {
                let id = least_loaded_egress(&inner);
                *key.egress_id.lock().unwrap() = id;
            }
        }
        self.mark_dirty();
        Ok(())
    }

    pub fn set_stored_master_key(&self, key: String) {
        self.inner.write().unwrap().master_key = Some(key);
        self.mark_dirty();
    }

    /// Ordering: most remaining quota first, ties shuffled, keys in 429 cooldown moved to the end
    /// (kept rather than dropped so a fully rate-limited pool still gets tried).
    pub fn candidates(&self, now_ms: u64) -> Vec<Arc<KeyEntry>> {
        let mut list: Vec<(u32, Arc<KeyEntry>)> = {
            let inner = self.inner.read().unwrap();
            inner
                .keys
                .iter()
                .filter(|k| k.usable())
                .map(|k| (k.remaining(), k.clone()))
                .collect()
        };
        list.shuffle(&mut rand::rng());
        // Stable sort keeps the shuffled order within groups of equal remaining quota.
        list.sort_by_key(|(remaining, _)| std::cmp::Reverse(*remaining));
        let (ready, cooling): (Vec<_>, Vec<_>) = list
            .into_iter()
            .map(|(_, k)| k)
            .partition(|k| k.cooldown_until_ms() <= now_ms);
        ready.into_iter().chain(cooling).collect()
    }

    pub fn list(&self) -> Vec<Arc<KeyEntry>> {
        self.inner.read().unwrap().keys.clone()
    }

    pub fn get(&self, id: u64) -> Option<Arc<KeyEntry>> {
        self.inner
            .read()
            .unwrap()
            .keys
            .iter()
            .find(|k| k.id == id)
            .cloned()
    }

    pub fn add(
        &self,
        key: &str,
        alias: &str,
        total_quota: u32,
    ) -> Result<Arc<KeyEntry>, PoolError> {
        let mut inner = self.inner.write().unwrap();
        if inner.keys.iter().any(|k| k.key == key) {
            return Err(PoolError::Duplicate);
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let entry = Arc::new(KeyEntry::from_record(KeyRecord {
            id,
            key: key.to_string(),
            egress_id: least_loaded_egress(&inner),
            alias: alias.to_string(),
            total_quota,
            used_quota: 0,
            active: true,
            invalid: false,
            created_at: Utc::now(),
            last_used_at: None,
        }));
        inner.keys.push(entry.clone());
        drop(inner);
        self.mark_dirty();
        Ok(entry)
    }

    pub fn update(&self, id: u64, patch: KeyPatch) -> Result<Arc<KeyEntry>, PoolError> {
        let entry = self.get(id).ok_or(PoolError::NotFound)?;
        if let Some(alias) = patch.alias {
            *entry.alias.lock().unwrap() = alias;
        }
        if let Some(total) = patch.total_quota.filter(|t| *t > 0) {
            entry.total_quota.store(total, Ordering::Relaxed);
            entry.used_quota.fetch_min(total, Ordering::Relaxed);
        }
        if let Some(used) = patch.used_quota {
            entry
                .used_quota
                .store(used.min(entry.total_quota()), Ordering::Relaxed);
        }
        if let Some(active) = patch.active {
            // An invalid (401) key cannot be re-enabled; delete and re-add it instead.
            entry
                .active
                .store(active && !entry.is_invalid(), Ordering::Relaxed);
            if active {
                entry.cooldown_until_ms.store(0, Ordering::Relaxed);
            }
        }
        self.mark_dirty();
        Ok(entry)
    }

    pub fn remove(&self, id: u64) -> Result<(), PoolError> {
        let mut inner = self.inner.write().unwrap();
        let before = inner.keys.len();
        inner.keys.retain(|k| k.id != id);
        if inner.keys.len() == before {
            return Err(PoolError::NotFound);
        }
        drop(inner);
        self.mark_dirty();
        Ok(())
    }

    pub fn remove_invalid(&self) -> usize {
        let mut inner = self.inner.write().unwrap();
        let before = inner.keys.len();
        inner.keys.retain(|k| !k.is_invalid());
        let removed = before - inner.keys.len();
        drop(inner);
        if removed > 0 {
            self.mark_dirty();
        }
        removed
    }

    pub fn mark_invalid(&self, entry: &KeyEntry) {
        entry.invalid.store(true, Ordering::Relaxed);
        entry.active.store(false, Ordering::Relaxed);
        self.mark_dirty();
    }

    pub fn mark_exhausted(&self, entry: &KeyEntry) {
        entry
            .used_quota
            .store(entry.total_quota(), Ordering::Relaxed);
        self.mark_dirty();
    }

    pub fn mark_rate_limited(&self, entry: &KeyEntry, cooldown: Duration) {
        entry
            .cooldown_until_ms
            .store(now_ms() + cooldown.as_millis() as u64, Ordering::Relaxed);
    }

    pub fn clear_cooldown(&self, entry: &KeyEntry) {
        entry.cooldown_until_ms.store(0, Ordering::Relaxed);
    }

    /// Increments used quota by one, never past the total.
    pub fn record_success(&self, entry: &KeyEntry) {
        let total = entry.total_quota();
        let _ = entry
            .used_quota
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |u| {
                (u < total).then_some(u + 1)
            });
        entry.last_used_ms.store(now_ms(), Ordering::Relaxed);
        self.mark_dirty();
    }

    pub fn set_usage(&self, entry: &KeyEntry, used: u32, total: Option<u32>) {
        if let Some(t) = total.filter(|t| *t > 0) {
            entry.total_quota.store(t, Ordering::Relaxed);
        }
        entry
            .used_quota
            .store(used.min(entry.total_quota()), Ordering::Relaxed);
        self.mark_dirty();
    }

    pub fn clear_invalid(&self, entry: &KeyEntry) {
        if entry.invalid.swap(false, Ordering::Relaxed) {
            self.mark_dirty();
        }
    }

    pub fn reset_usage(&self, entry: &KeyEntry) {
        entry.used_quota.store(0, Ordering::Relaxed);
        entry.cooldown_until_ms.store(0, Ordering::Relaxed);
        self.mark_dirty();
    }

    /// Resets all usage when the month changed since the last recorded reset.
    /// Returns true if a reset happened. A store with no recorded month just adopts `current`.
    pub fn roll_month(&self, current: &str) -> bool {
        let mut inner = self.inner.write().unwrap();
        let changed = match inner.month.as_deref() {
            Some(m) if m == current => return false,
            Some(_) => true,
            None => false,
        };
        inner.month = Some(current.to_string());
        if changed {
            for k in &inner.keys {
                k.used_quota.store(0, Ordering::Relaxed);
            }
        }
        drop(inner);
        self.mark_dirty();
        changed
    }

    pub fn overview(&self, now_ms: u64) -> Overview {
        let inner = self.inner.read().unwrap();
        let mut o = Overview {
            key_count: inner.keys.len(),
            available: 0,
            cooling: 0,
            exhausted: 0,
            disabled: 0,
            invalid: 0,
            total_quota: 0,
            used_quota: 0,
            remaining_quota: 0,
        };
        for k in &inner.keys {
            match k.status(now_ms) {
                KeyStatus::Available => o.available += 1,
                KeyStatus::Cooling => o.cooling += 1,
                KeyStatus::Exhausted => o.exhausted += 1,
                KeyStatus::Disabled => o.disabled += 1,
                KeyStatus::Invalid => o.invalid += 1,
            }
            o.total_quota += u64::from(k.total_quota());
            o.used_quota += u64::from(k.used_quota());
            if k.usable() {
                o.remaining_quota += u64::from(k.remaining());
            }
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_with(quotas: &[(u32, u32)]) -> Pool {
        let pool = Pool::from_store(StoreFile {
            next_id: 1,
            ..Default::default()
        });
        for (i, (total, used)) in quotas.iter().enumerate() {
            let k = pool
                .add(
                    &format!("tvly-key-{i:04}-padding"),
                    &format!("k{i}"),
                    *total,
                )
                .unwrap();
            pool.set_usage(&k, *used, None);
        }
        pool
    }

    #[test]
    fn candidates_sorted_by_remaining_desc() {
        let pool = pool_with(&[(1000, 900), (1000, 0), (1000, 500)]);
        let order: Vec<u32> = pool
            .candidates(now_ms())
            .iter()
            .map(|k| k.remaining())
            .collect();
        assert_eq!(order, vec![1000, 500, 100]);
    }

    #[test]
    fn candidates_exclude_unusable() {
        let pool = pool_with(&[(1000, 1000), (1000, 0), (1000, 0), (1000, 0)]);
        let keys = pool.list();
        pool.mark_invalid(&keys[1]);
        pool.update(
            keys[2].id,
            KeyPatch {
                active: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        let ids: Vec<u64> = pool.candidates(now_ms()).iter().map(|k| k.id).collect();
        assert_eq!(ids, vec![keys[3].id]);
    }

    #[test]
    fn ties_are_shuffled() {
        let pool = pool_with(&[(1000, 0), (1000, 0), (1000, 0)]);
        let mut firsts = std::collections::HashSet::new();
        for _ in 0..200 {
            firsts.insert(pool.candidates(now_ms())[0].id);
        }
        assert_eq!(firsts.len(), 3);
    }

    #[test]
    fn cooling_keys_go_last() {
        let pool = pool_with(&[(1000, 0), (1000, 500)]);
        let best = pool.candidates(now_ms())[0].clone();
        assert_eq!(best.remaining(), 1000);
        pool.mark_rate_limited(&best, Duration::from_secs(60));
        let order = pool.candidates(now_ms());
        assert_eq!(order.len(), 2);
        assert_eq!(order[1].id, best.id);
        assert_eq!(best.status(now_ms()), KeyStatus::Cooling);
    }

    #[test]
    fn record_success_caps_at_total() {
        let pool = pool_with(&[(2, 1)]);
        let k = pool.list()[0].clone();
        pool.record_success(&k);
        pool.record_success(&k);
        assert_eq!(k.used_quota(), 2);
        assert!(k.last_used_at().is_some());
        assert!(pool.candidates(now_ms()).is_empty());
    }

    #[test]
    fn concurrent_increments_are_exact() {
        let pool = Arc::new(pool_with(&[(100_000, 0)]));
        let k = pool.list()[0].clone();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (pool, k) = (pool.clone(), k.clone());
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        pool.record_success(&k);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(k.used_quota(), 8000);
    }

    #[test]
    fn invalid_cannot_be_reactivated() {
        let pool = pool_with(&[(1000, 0)]);
        let k = pool.list()[0].clone();
        pool.mark_invalid(&k);
        pool.update(
            k.id,
            KeyPatch {
                active: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!k.is_active());
        assert_eq!(k.status(now_ms()), KeyStatus::Invalid);
    }

    #[test]
    fn duplicate_add_rejected() {
        let pool = pool_with(&[]);
        pool.add("tvly-dup-000000000", "a", 10).unwrap();
        assert_eq!(
            pool.add("tvly-dup-000000000", "b", 10).unwrap_err(),
            PoolError::Duplicate
        );
    }

    #[test]
    fn roll_month_resets_once() {
        let pool = pool_with(&[(1000, 300)]);
        assert!(!pool.roll_month("2026-09"));
        assert_eq!(pool.list()[0].used_quota(), 300);
        assert!(pool.roll_month("2026-10"));
        assert_eq!(pool.list()[0].used_quota(), 0);
        assert!(!pool.roll_month("2026-10"));
    }

    #[test]
    fn snapshot_roundtrip_keeps_ids() {
        let pool = pool_with(&[(1000, 10), (500, 20)]);
        pool.remove(1).unwrap();
        let snap = pool.snapshot();
        let restored = Pool::from_store(snap);
        let k = restored.add("tvly-new-0000000000", "n", 10).unwrap();
        assert_eq!(k.id, 3);
        assert_eq!(restored.list()[0].used_quota(), 20);
    }

    #[test]
    fn masking() {
        assert_eq!(mask_key("tvly-abcdefghijklmnop"), "tvly-abc...mnop");
        assert_eq!(mask_key("short"), "*****");
    }
}
