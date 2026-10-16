use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::app::AppState;
use crate::egress::{EgressError, Route};
use crate::pool::KeyEntry;

const USAGE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REPORTED_ERRORS: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error(transparent)]
    Gate(#[from] crate::usage_gate::GateError),
    #[error(transparent)]
    Egress(#[from] EgressError),
    #[error("upstream status {status}: {body}")]
    Status { status: StatusCode, body: String },
    #[error("unexpected /usage response: {0}")]
    Parse(#[from] serde_json::Error),
}

impl UsageError {
    pub fn is_rate_limited(&self) -> bool {
        matches!(
            self,
            UsageError::Gate(crate::usage_gate::GateError::Limited(_))
        ) || matches!(self, UsageError::Status { status, .. } if *status == StatusCode::TOO_MANY_REQUESTS)
    }
}

#[derive(Deserialize)]
struct UsageResponse {
    #[serde(default)]
    key: UsageKey,
    #[serde(default)]
    account: UsageAccount,
}

#[derive(Deserialize, Default)]
struct UsageKey {
    usage: Option<f64>,
    limit: Option<f64>,
}

#[derive(Deserialize, Default)]
struct UsageAccount {
    plan_limit: Option<f64>,
}

fn to_u32(v: f64) -> u32 {
    v.clamp(0.0, f64::from(u32::MAX)) as u32
}

/// Returns `(used, limit)` as reported by Tavily `GET /usage`.
async fn fetch_usage(
    route: Route<'_>,
    base_url: &str,
    key: &str,
) -> Result<(u32, Option<u32>), UsageError> {
    let request = route
        .client
        .get(format!("{base_url}/usage"))
        .bearer_auth(key)
        .timeout(USAGE_TIMEOUT);
    let resp = route.send(request).await?;
    let status = resp.status;
    let body = resp.body;
    if status != StatusCode::OK {
        let text = String::from_utf8_lossy(&body);
        return Err(UsageError::Status {
            status,
            body: text.chars().take(300).collect(),
        });
    }
    let parsed: UsageResponse = serde_json::from_slice(&body)?;
    let used = to_u32(parsed.key.usage.unwrap_or(0.0));
    let limit = parsed
        .key
        .limit
        .or(parsed.account.plan_limit)
        .map(to_u32)
        .filter(|l| *l > 0);
    Ok((used, limit))
}

pub async fn sync_key(app: &AppState, entry: &KeyEntry) -> Result<(), UsageError> {
    let route = app.egress.route(entry)?;
    let _permit = app.usage_gate.reserve(&entry.key, route.exit_ip)?;
    match fetch_usage(route, &app.cfg.upstream_base_url, &entry.key).await {
        Ok((used, limit)) => {
            app.pool.set_usage(entry, used, limit);
            // Tavily accepted the key, so an earlier 401 (e.g. a revoked-then-restored key) no longer applies.
            app.pool.clear_invalid(entry);
            Ok(())
        }
        Err(e) => {
            if let UsageError::Status { status, .. } = &e {
                match status.as_u16() {
                    401 => app.pool.mark_invalid(entry),
                    432 | 433 => app.pool.mark_exhausted(entry),
                    _ => {}
                }
            }
            Err(e)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncKeyError {
    pub id: u64,
    pub alias: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncStatus {
    pub running: bool,
    pub total: usize,
    pub done: usize,
    pub succeeded: usize,
    pub failed: usize,
    /// Keys not attempted because Tavily rate limited the run.
    pub skipped: usize,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub errors: Vec<SyncKeyError>,
}

#[derive(Default)]
pub struct SyncJob {
    status: Mutex<SyncStatus>,
}

impl SyncJob {
    pub fn status(&self) -> SyncStatus {
        self.status.lock().unwrap().clone()
    }

    /// Starts a background sync of every non-invalid key. Returns `Err` with the current
    /// status if a sync is already running.
    pub fn start(self: &Arc<Self>, app: AppState) -> Result<SyncStatus, SyncStatus> {
        let keys: Vec<Arc<KeyEntry>> = app
            .pool
            .list()
            .into_iter()
            .filter(|k| !k.is_invalid())
            .collect();
        let snapshot = {
            let mut st = self.status.lock().unwrap();
            if st.running {
                return Err(st.clone());
            }
            *st = SyncStatus {
                running: true,
                total: keys.len(),
                started_at: Some(Utc::now()),
                ..Default::default()
            };
            st.clone()
        };
        let job = self.clone();
        tokio::spawn(async move { job.run(app, keys).await });
        Ok(snapshot)
    }

    async fn run(&self, app: AppState, keys: Vec<Arc<KeyEntry>>) {
        let interval = app
            .cfg
            .usage_sync
            .request_interval
            .max(Duration::from_secs(60));
        for (i, key) in keys.iter().enumerate() {
            if i > 0 && !interval.is_zero() {
                tokio::time::sleep(interval).await;
            }
            let result = sync_key(&app, key).await;
            let mut st = self.status.lock().unwrap();
            st.done += 1;
            match result {
                Ok(()) => st.succeeded += 1,
                Err(e) => {
                    st.failed += 1;
                    warn!(key = %key.alias(), error = %e, "usage sync failed");
                    if st.errors.len() < MAX_REPORTED_ERRORS {
                        st.errors.push(SyncKeyError {
                            id: key.id,
                            alias: key.alias(),
                            error: e.to_string(),
                        });
                    }
                    // The /usage limit is shared by all keys, so the remaining requests would
                    // fail too and only prolong the block.
                    if e.is_rate_limited() {
                        st.skipped = keys.len() - st.done;
                        warn!(
                            skipped = st.skipped,
                            "Tavily rate limited usage sync; stopping this run"
                        );
                        break;
                    }
                }
            }
        }
        if let Err(e) = app.persister.save_now(&app.pool).await {
            warn!(error = %e, "failed to persist state after usage sync");
        }
        let mut st = self.status.lock().unwrap();
        st.running = false;
        st.finished_at = Some(Utc::now());
        info!(
            total = st.total,
            succeeded = st.succeeded,
            failed = st.failed,
            skipped = st.skipped,
            "usage sync finished"
        );
    }
}
