use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::hysteria::HysteriaProcess;
use crate::pool::KeyEntry;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EgressConfig {
    pub hysteria_binary: Option<PathBuf>,
    pub nodes: Vec<EgressNode>,
    #[serde(
        deserialize_with = "crate::config::de_duration",
        serialize_with = "crate::config::ser_duration"
    )]
    pub health_check_interval: Duration,
    #[serde(
        deserialize_with = "crate::config::de_duration",
        serialize_with = "crate::config::ser_duration"
    )]
    pub health_check_timeout: Duration,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            hysteria_binary: None,
            nodes: Vec::new(),
            health_check_interval: Duration::from_secs(30),
            health_check_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EgressNode {
    pub id: String,
    pub exit_ip: IpAddr,
    pub proxy_url: String,
    pub hysteria_config: Option<PathBuf>,
}

impl EgressConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.health_check_interval.is_zero() && !self.health_check_timeout.is_zero(),
            "egress health check interval and timeout must be greater than zero"
        );
        let mut ids = HashSet::new();
        let mut ips = HashSet::new();
        let mut listeners = HashSet::new();
        for n in &self.nodes {
            ensure!(
                !n.id.is_empty() && n.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "egress id must contain only ASCII letters, digits or hyphens"
            );
            ensure!(ids.insert(&n.id), "duplicate egress id {}", n.id);
            ensure!(
                ips.insert(n.exit_ip.to_canonical()),
                "duplicate exit IP for egress {}",
                n.id
            );
            let url = reqwest::Url::parse(&n.proxy_url).context("invalid egress proxy URL")?;
            let host: IpAddr = url
                .host_str()
                .unwrap_or("")
                .trim_matches(['[', ']'])
                .parse()
                .context("egress proxy must use a loopback IP")?;
            ensure!(
                host.is_loopback() && url.port().is_some(),
                "egress proxy must use loopback IP and explicit port"
            );
            ensure!(
                matches!(url.scheme(), "socks5h" | "http"),
                "egress proxy must use socks5h or http"
            );
            ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && matches!(url.path(), "" | "/")
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "egress proxy URL must not contain credentials, path, query or fragment"
            );
            ensure!(
                listeners.insert((host, url.port().unwrap())),
                "duplicate egress listener"
            );
            ensure!(
                n.hysteria_config.is_none() || self.hysteria_binary.is_some(),
                "managed egress needs hysteria_binary"
            );
        }
        ensure!(
            self.hysteria_binary.is_none() || !self.nodes.is_empty(),
            "hysteria_binary configured without egress nodes"
        );
        Ok(())
    }
}

pub struct EgressPool {
    direct: Option<reqwest::Client>,
    nodes: HashMap<String, Arc<Node>>,
}

struct Node {
    id: String,
    client: reqwest::Client,
    exit_ip: String,
    health: Mutex<Health>,
    process: Option<Mutex<HysteriaProcess>>,
}

struct Health {
    available: bool,
    epoch: u64,
    revision: u64,
}

impl Node {
    fn fail(&self, epoch: u64) {
        let mut health = self.health.lock().unwrap();
        // Requests from before a successful recovery must not quarantine the new epoch.
        if health.epoch == epoch {
            if health.available {
                warn!(egress = %self.id, "egress quarantined after proxy failure");
            }
            health.available = false;
            health.revision += 1;
        }
    }

    fn apply_probe(&self, revision: u64, available: bool) {
        let mut health = self.health.lock().unwrap();
        // An in-flight failure after this probe began takes precedence over its result.
        if health.revision != revision {
            return;
        }
        if health.available != available {
            if available {
                health.epoch += 1;
                info!(egress = %self.id, "egress recovered");
            } else {
                warn!(egress = %self.id, "egress quarantined after failed health check");
            }
        }
        health.available = available;
        health.revision += 1;
    }

    async fn probe(self: &Arc<Self>, url: &str, timeout: Duration) {
        let revision = self.health.lock().unwrap().revision;
        if self.process.is_some() {
            let node = self.clone();
            let result = tokio::task::spawn_blocking(move || {
                node.process
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .ensure_running()
            })
            .await;
            match result {
                Ok(Ok(())) => {}
                other => {
                    warn!(egress = %self.id, error = ?other, "could not restart Hysteria client");
                    self.apply_probe(revision, false);
                    return;
                }
            }
        }
        // No key, request body, or /usage call: only prove that the tunnel reaches the origin.
        let result = self.client.head(url).timeout(timeout).send().await;
        let available = match result {
            Ok(response) => {
                !response.status().is_server_error()
                    && response.status() != reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED
            }
            Err(error) => {
                warn!(egress = %self.id, %error, "egress health check failed");
                false
            }
        };
        self.apply_probe(revision, available);
    }
}

pub(crate) struct Route<'a> {
    pub client: &'a reqwest::Client,
    pub exit_ip: &'a str,
    node: Option<(&'a Node, u64)>,
}

pub(crate) struct EgressResponse {
    pub status: reqwest::StatusCode,
    pub headers: reqwest::header::HeaderMap,
    pub body: bytes::Bytes,
}

#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    #[error("egress {0} is unavailable")]
    Unavailable(String),
    #[error("proxy returned {0}")]
    ProxyStatus(reqwest::StatusCode),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

impl Route<'_> {
    pub async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<EgressResponse, EgressError> {
        let result = async {
            let response = request.send().await?;
            let status = response.status();
            if self.node.is_some() && matches!(status.as_u16(), 407 | 502 | 503 | 504) {
                return Err(EgressError::ProxyStatus(status));
            }
            let headers = response.headers().clone();
            let body = response.bytes().await?;
            Ok(EgressResponse {
                status,
                headers,
                body,
            })
        }
        .await;
        let proxy_failed = match &result {
            Err(EgressError::Http(error)) => !error.is_builder(),
            Err(EgressError::ProxyStatus(_)) => true,
            _ => false,
        };
        if proxy_failed && let Some((node, epoch)) = self.node {
            node.fail(epoch);
        }
        result
    }
}

fn client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never());
    if let Some(url) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(url)?);
    }
    builder.build().context("build egress HTTP client")
}

impl EgressPool {
    pub fn build(cfg: &EgressConfig) -> Result<Self> {
        cfg.validate()?;
        let mut result = Self {
            direct: if cfg.nodes.is_empty() {
                Some(client(None)?)
            } else {
                None
            },
            nodes: HashMap::new(),
        };
        for node in &cfg.nodes {
            let process = if let Some(path) = &node.hysteria_config {
                Some(Mutex::new(HysteriaProcess::start(
                    cfg.hysteria_binary.as_ref().unwrap(),
                    path,
                    &node.proxy_url,
                    &node.id,
                )?))
            } else {
                None
            };
            result.nodes.insert(
                node.id.clone(),
                Arc::new(Node {
                    id: node.id.clone(),
                    client: client(Some(&node.proxy_url))?,
                    exit_ip: node.exit_ip.to_canonical().to_string(),
                    health: Mutex::new(Health {
                        available: true,
                        epoch: 0,
                        revision: 0,
                    }),
                    process,
                }),
            );
        }
        Ok(result)
    }

    pub(crate) fn route(&self, key: &KeyEntry) -> Result<Route<'_>, EgressError> {
        match key.egress_id() {
            Some(id) => {
                let node = self
                    .nodes
                    .get(&id)
                    .ok_or_else(|| EgressError::Unavailable(id.clone()))?;
                let health = node.health.lock().unwrap();
                if !health.available {
                    return Err(EgressError::Unavailable(id));
                }
                Ok(Route {
                    client: &node.client,
                    exit_ip: &node.exit_ip,
                    node: Some((node, health.epoch)),
                })
            }
            None => self
                .direct
                .as_ref()
                .map(|client| Route {
                    client,
                    exit_ip: "direct",
                    node: None,
                })
                .ok_or_else(|| {
                    EgressError::Unavailable("unassigned (direct fallback disabled)".into())
                }),
        }
    }

    pub(crate) fn spawn_health_checks(
        &self,
        base_url: &str,
        cfg: &EgressConfig,
    ) -> Vec<JoinHandle<()>> {
        if self.nodes.is_empty() {
            return Vec::new();
        }
        let mut url = reqwest::Url::parse(base_url).expect("validated upstream base URL");
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_path("/");
        url.set_query(None);
        url.set_fragment(None);
        self.nodes
            .values()
            .map(|node| {
                let node = node.clone();
                let url = url.clone();
                let interval = cfg.health_check_interval;
                let timeout = cfg.health_check_timeout;
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(interval);
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tick.tick().await;
                        node.probe(url.as_str(), timeout).await;
                    }
                })
            })
            .collect()
    }
}
