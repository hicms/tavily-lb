use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use tavily_lb::{AppState, Config, MasterKeySource, jobs, router};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
tavily-lb - Tavily API key pool load balancer

USAGE:
    tavily-lb [--config <path>]

OPTIONS:
    -c, --config <path>   TOML config file (default: $CONFIG, or ./config.toml if present)
    -h, --help            Print this help
    -V, --version         Print version

Environment overrides: LISTEN_ADDR, PORT, TAVILY_BASE_URL, UPSTREAM_TIMEOUT, REQUEST_DEADLINE,
MAX_ATTEMPTS, COOLDOWN_ON_429, MASTER_KEY, DATA_FILE, KEYS_FILE, ADMIN_UI, MCP_ENABLED,
MONTHLY_RESET, USAGE_SYNC_ENABLED, USAGE_SYNC_INTERVAL, RUST_LOG.";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn config_path() -> Result<Option<PathBuf>> {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                path = Some(PathBuf::from(args.next().context("--config needs a path")?))
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("tavily-lb {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    if path.is_none() {
        path = std::env::var_os("CONFIG").map(PathBuf::from);
    }
    if path.is_none() && std::path::Path::new("config.toml").exists() {
        path = Some(PathBuf::from("config.toml"));
    }
    Ok(path)
}

async fn run() -> Result<()> {
    let path = config_path()?;
    let cfg = Config::load(path.as_deref())?;
    let app = AppState::build(cfg)?;
    app.persister.save_now(&app.pool).await?;

    match app.master_key_source {
        MasterKeySource::Generated => warn!(
            master_key = %app.master_key,
            state_file = %app.persister.path().display(),
            "generated a new master key; set MASTER_KEY to pin it"
        ),
        MasterKeySource::Stored => {
            info!(state_file = %app.persister.path().display(), "using master key from state file")
        }
        MasterKeySource::Config => info!("using master key from config"),
    }

    let tasks = jobs::spawn_all(&app);
    let listener = tokio::net::TcpListener::bind(&app.cfg.listen)
        .await
        .with_context(|| format!("bind {}", app.cfg.listen))?;
    info!(
        addr = %app.cfg.listen,
        upstream = %app.cfg.upstream_base_url,
        keys = app.pool.list().len(),
        admin_ui = app.cfg.admin_ui,
        mcp = app.cfg.mcp,
        "tavily-lb listening"
    );

    axum::serve(listener, router(app.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    for t in tasks {
        t.abort();
    }
    app.persister.save_now(&app.pool).await?;
    info!("state saved, bye");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    // Ctrl+Break, closing the console window, logoff and system shutdown would otherwise
    // terminate the process without the final save.
    #[cfg(windows)]
    let term = async {
        use tokio::signal::windows;
        match (
            windows::ctrl_break(),
            windows::ctrl_close(),
            windows::ctrl_logoff(),
            windows::ctrl_shutdown(),
        ) {
            (Ok(mut brk), Ok(mut close), Ok(mut logoff), Ok(mut shutdown)) => {
                tokio::select! {
                    _ = brk.recv() => {},
                    _ = close.recv() => {},
                    _ = logoff.recv() => {},
                    _ = shutdown.recv() => {},
                }
            }
            _ => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(any(unix, windows)))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    info!("shutting down");
}
