use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::pool::Pool;

pub const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoreFile {
    pub version: u32,
    /// Local `YYYY-MM` of the last monthly reset; used to catch up after downtime.
    pub month: Option<String>,
    /// Only used when no master key is configured; generated on first start.
    pub master_key: Option<String>,
    pub next_id: u64,
    pub keys: Vec<KeyRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyRecord {
    pub id: u64,
    pub key: String,
    #[serde(default)]
    pub egress_id: Option<String>,
    pub alias: String,
    pub total_quota: u32,
    pub used_quota: u32,
    pub active: bool,
    pub invalid: bool,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// A missing file yields an empty store; a corrupt file is an error so state is never silently wiped.
pub fn load(path: &Path) -> Result<StoreFile> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut file: StoreFile = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse state file {}", path.display()))?;
            if file.version == 0 {
                file.version = STORE_VERSION;
            }
            anyhow::ensure!(
                file.version == STORE_VERSION,
                "unsupported state file version {} in {}",
                file.version,
                path.display()
            );
            Ok(file)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile {
            version: STORE_VERSION,
            next_id: 1,
            ..Default::default()
        }),
        Err(e) => Err(e).with_context(|| format!("read state file {}", path.display())),
    }
}

pub fn save(path: &Path, file: &StoreFile) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(file)?)
}

pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let tmp = tmp_path(path);
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Serializes writes of the pool snapshot to disk.
pub struct Persister {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Persister {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn save_now(&self, pool: &Pool) -> Result<()> {
        let _guard = self.lock.lock().await;
        pool.clear_dirty();
        let snapshot = pool.snapshot();
        let path = self.path.clone();
        let res = tokio::task::spawn_blocking(move || save(&path, &snapshot)).await?;
        if res.is_err() {
            pool.mark_dirty();
        }
        res
    }

    pub async fn flush_if_dirty(&self, pool: &Pool) -> Result<()> {
        if pool.is_dirty() {
            self.save_now(pool).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("state.json");
        let empty = load(&path).unwrap();
        assert_eq!(empty.next_id, 1);
        assert!(empty.keys.is_empty());

        let file = StoreFile {
            version: STORE_VERSION,
            month: Some("2026-10".into()),
            master_key: None,
            next_id: 2,
            keys: vec![KeyRecord {
                id: 1,
                key: "tvly-abc".into(),
                egress_id: None,
                alias: "a".into(),
                total_quota: 1000,
                used_quota: 5,
                active: true,
                invalid: false,
                created_at: Utc::now(),
                last_used_at: None,
            }],
        };
        save(&path, &file).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.keys, file.keys);
        assert_eq!(loaded.month.as_deref(), Some("2026-10"));

        std::fs::write(&path, b"{not json").unwrap();
        assert!(load(&path).is_err());
    }
}
