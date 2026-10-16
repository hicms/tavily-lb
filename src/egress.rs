use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::hysteria::HysteriaProcess;
use crate::pool::KeyEntry;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EgressConfig {
    pub hysteria_binary: Option<PathBuf>,
    pub nodes: Vec<EgressNode>,
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
    clients: HashMap<String, (reqwest::Client, String)>,
    _processes: Vec<HysteriaProcess>,
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
            clients: HashMap::new(),
            _processes: Vec::new(),
        };
        for node in &cfg.nodes {
            if let Some(path) = &node.hysteria_config {
                result._processes.push(HysteriaProcess::start(
                    cfg.hysteria_binary.as_ref().unwrap(),
                    path,
                    &node.proxy_url,
                    &node.id,
                )?);
            }
            result.clients.insert(
                node.id.clone(),
                (
                    client(Some(&node.proxy_url))?,
                    node.exit_ip.to_canonical().to_string(),
                ),
            );
        }
        Ok(result)
    }

    pub fn route(&self, key: &KeyEntry) -> Result<(&reqwest::Client, &str)> {
        match key.egress_id() {
            Some(id) => self
                .clients
                .get(&id)
                .map(|(client, ip)| (client, ip.as_str()))
                .with_context(|| format!("key #{} has unavailable egress {id}", key.id)),
            None => self
                .direct
                .as_ref()
                .map(|c| (c, "direct"))
                .context("key has no assigned egress; direct fallback is disabled"),
        }
    }
}
