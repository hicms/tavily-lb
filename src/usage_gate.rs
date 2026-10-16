use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use sha2::{Digest, Sha256};

use crate::pool::now_ms;
use crate::proxy::json_error;

const INTERVAL_MS: u64 = 60_000;

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("usage is limited to one request per key and per exit IP per minute; retry after {0}s")]
    Limited(u64),
    #[error("cannot persist usage reservation: {0}")]
    Storage(#[from] anyhow::Error),
}

impl GateError {
    pub fn response(&self) -> Response {
        match self {
            Self::Limited(seconds) => {
                let mut resp = json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "usage_rate_limited",
                    &self.to_string(),
                );
                resp.headers_mut().insert(
                    "retry-after",
                    HeaderValue::from_str(&seconds.to_string()).unwrap(),
                );
                resp
            }
            Self::Storage(_) => json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "usage_guard_unavailable",
                "Cannot safely reserve a usage request.",
            ),
        }
    }
}

struct State {
    next: BTreeMap<String, u64>,
    monotonic: BTreeMap<String, Instant>,
    busy: HashSet<String>,
}

pub struct UsageGate {
    path: PathBuf,
    state: Mutex<State>,
    _lock: File,
}

pub struct UsagePermit<'a> {
    gate: &'a UsageGate,
    buckets: [String; 2],
}

impl UsageGate {
    pub fn open(state_file: &Path) -> Result<Self> {
        let path = state_file.with_extension("usage.json");
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_file.with_extension("usage.lock"))?;
        lock.try_lock()
            .context("another service is using this state file")?;
        let next: BTreeMap<String, u64> = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("corrupt usage reservation file")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        // Wall clocks can change across restart. Every persisted bucket gets a fresh
        // minimum minute of protection, even if its wall timestamp appears expired.
        let until = Instant::now() + Duration::from_millis(INTERVAL_MS);
        let monotonic = next.keys().map(|key| (key.clone(), until)).collect();
        Ok(Self {
            path,
            state: Mutex::new(State {
                next,
                monotonic,
                busy: HashSet::new(),
            }),
            _lock: lock,
        })
    }

    pub fn reserve(&self, key: &str, exit_ip: &str) -> Result<UsagePermit<'_>, GateError> {
        let digest: String = Sha256::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let buckets = [format!("key:{digest}"), format!("ip:{exit_ip}")];
        let mut st = self.state.lock().unwrap();
        let now = now_ms();
        let monotonic_now = Instant::now();
        let retry = buckets
            .iter()
            .map(|b| {
                if st.busy.contains(b) {
                    INTERVAL_MS
                } else {
                    let wall = st.next.get(b).copied().unwrap_or(0).saturating_sub(now);
                    let monotonic = st
                        .monotonic
                        .get(b)
                        .map(|until| {
                            until
                                .saturating_duration_since(monotonic_now)
                                .as_nanos()
                                .div_ceil(1_000_000) as u64
                        })
                        .unwrap_or(0);
                    wall.max(monotonic)
                }
            })
            .max()
            .unwrap();
        if retry > 0 {
            return Err(GateError::Limited(retry.div_ceil(1000)));
        }
        let expired: Vec<_> = st
            .next
            .iter()
            .filter(|(b, until)| {
                **until <= now
                    && st
                        .monotonic
                        .get(*b)
                        .is_none_or(|until| *until <= monotonic_now)
                    && !st.busy.contains(*b)
            })
            .map(|(b, _)| b.clone())
            .collect();
        for bucket in expired {
            st.next.remove(&bucket);
            st.monotonic.remove(&bucket);
        }
        for bucket in &buckets {
            st.next.insert(bucket.clone(), now + INTERVAL_MS);
            st.monotonic.insert(
                bucket.clone(),
                monotonic_now + Duration::from_millis(INTERVAL_MS),
            );
        }
        // Persist before sending; a failed or cancelled request still consumes its slot.
        crate::store::write_atomic(
            &self.path,
            &serde_json::to_vec(&st.next).map_err(anyhow::Error::from)?,
        )?;
        st.busy.extend(buckets.iter().cloned());
        Ok(UsagePermit {
            gate: self,
            buckets,
        })
    }
}

impl Drop for UsagePermit<'_> {
    fn drop(&mut self) {
        let mut st = self.gate.state.lock().unwrap();
        for bucket in &self.buckets {
            st.busy.remove(bucket);
        }
    }
}

/// Classify the normalized URL actually sent by reqwest, including encoded aliases.
pub fn is_usage_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        let path = percent_encoding::percent_decode_str(url.path()).decode_utf8_lossy();
        let mut segments = Vec::new();
        for segment in path.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    segments.pop();
                }
                part => segments.push(part),
            }
        }
        segments == ["usage"]
    })
}

#[cfg(test)]
#[path = "../tests/support/usage_gate_clock.rs"]
mod clock_tests;
