use std::ops::Deref;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::routing::get;
use base64::Engine;
use tracing::info;

use crate::config::Config;
use crate::pool::Pool;
use crate::store::{self, Persister};
use crate::usage::SyncJob;
use crate::{admin, mcp, proxy, web};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterKeySource {
    Config,
    Stored,
    Generated,
}

pub struct AppInner {
    pub cfg: Config,
    pub pool: Arc<Pool>,
    pub persister: Persister,
    pub egress: crate::egress::EgressPool,
    pub usage_gate: crate::usage_gate::UsageGate,
    pub master_key: String,
    pub master_key_source: MasterKeySource,
    pub sync: Arc<SyncJob>,
}

#[derive(Clone)]
pub struct AppState(Arc<AppInner>);

impl Deref for AppState {
    type Target = AppInner;
    fn deref(&self) -> &AppInner {
        &self.0
    }
}

pub fn current_month() -> String {
    chrono::Local::now().format("%Y-%m").to_string()
}

fn generate_master_key() -> String {
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl AppState {
    /// Loads persisted state, resolves the master key, imports `keys_file`, and catches up a
    /// missed monthly reset. Call `persister.save_now` afterwards to write the result.
    pub fn build(cfg: Config) -> Result<Self> {
        let usage_gate = crate::usage_gate::UsageGate::open(&cfg.data_file)?;
        let file = store::load(&cfg.data_file)?;
        let pool = Arc::new(Pool::from_store(file));
        pool.configure_egress(cfg.egress.nodes.iter().map(|n| n.id.clone()).collect())?;

        let (master_key, master_key_source) = match (&cfg.master_key, pool.stored_master_key()) {
            (Some(k), _) => (k.trim().to_string(), MasterKeySource::Config),
            (None, Some(k)) => (k, MasterKeySource::Stored),
            (None, None) => {
                let k = generate_master_key();
                pool.set_stored_master_key(k.clone());
                (k, MasterKeySource::Generated)
            }
        };

        let state = AppState(Arc::new(AppInner {
            persister: Persister::new(cfg.data_file.clone()),
            egress: crate::egress::EgressPool::build(&cfg.egress)?,
            usage_gate,
            pool,
            master_key,
            master_key_source,
            sync: Arc::new(SyncJob::default()),
            cfg,
        }));

        if let Some(path) = &state.cfg.keys_file {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read keys file {}", path.display()))?;
            let res = admin::import_text(&state, &text, state.cfg.default_total_quota);
            info!(
                file = %path.display(),
                added = res.added,
                duplicates = res.duplicates,
                errors = res.errors.len(),
                "imported keys file"
            );
            for e in &res.errors {
                tracing::warn!(line = e.line, reason = %e.reason, "skipped line in keys file");
            }
        }

        if state.cfg.monthly_reset && state.pool.roll_month(&current_month()) {
            info!("new month since last run, quota usage reset");
        }
        Ok(state)
    }
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

pub fn router(app: AppState) -> Router {
    let mut r = Router::new()
        .route("/healthz", get(healthz))
        .nest("/api", admin::router(app.clone()));
    if app.cfg.admin_ui {
        r = r
            .route("/admin", get(web::redirect_to_index))
            .route("/admin/", get(web::index))
            .route("/admin/{*path}", get(web::asset));
    }
    if app.cfg.mcp {
        r = r.route("/mcp", get(mcp::handle_get).post(mcp::handle_post));
    }
    r.fallback(proxy::handle).with_state(app)
}
