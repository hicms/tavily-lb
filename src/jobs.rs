use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

use crate::app::{AppState, current_month};

const MONTH_CHECK_INTERVAL: Duration = Duration::from_secs(60);

pub fn spawn_all(app: &AppState) -> Vec<JoinHandle<()>> {
    let mut tasks = vec![spawn_flush(app.clone())];
    if app.cfg.monthly_reset {
        tasks.push(spawn_month_watch(app.clone()));
    }
    if app.cfg.usage_sync.enabled {
        tasks.push(spawn_usage_sync(app.clone()));
    }
    tasks
}

fn spawn_flush(app: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(app.cfg.flush_interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = app.persister.flush_if_dirty(&app.pool).await {
                warn!(error = %e, "failed to persist state");
            }
        }
    })
}

/// Polls the local month instead of sleeping until midnight, so clock changes and
/// suspended hosts still trigger the reset.
fn spawn_month_watch(app: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(MONTH_CHECK_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if app.pool.roll_month(&current_month()) {
                info!("monthly quota reset");
                if let Err(e) = app.persister.save_now(&app.pool).await {
                    warn!(error = %e, "failed to persist state after monthly reset");
                }
            }
        }
    })
}

fn spawn_usage_sync(app: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(app.cfg.usage_sync.interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if app.sync.start(app.clone()).is_err() {
                info!("skipping scheduled usage sync; previous run still in progress");
            }
        }
    })
}
