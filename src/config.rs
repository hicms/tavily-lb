use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: String,
    pub upstream_base_url: String,
    #[serde(deserialize_with = "de_duration", serialize_with = "ser_duration")]
    pub upstream_timeout: Duration,
    #[serde(deserialize_with = "de_duration", serialize_with = "ser_duration")]
    pub request_deadline: Duration,
    /// 0 means try every candidate key.
    pub max_attempts: usize,
    #[serde(deserialize_with = "de_duration", serialize_with = "ser_duration")]
    pub cooldown_on_429: Duration,
    pub max_body_bytes: usize,
    #[serde(skip_serializing)]
    pub master_key: Option<String>,
    pub data_file: PathBuf,
    pub keys_file: Option<PathBuf>,
    pub default_total_quota: u32,
    #[serde(deserialize_with = "de_duration", serialize_with = "ser_duration")]
    pub flush_interval: Duration,
    pub monthly_reset: bool,
    pub admin_ui: bool,
    /// Serves an MCP (streamable HTTP) endpoint at `/mcp`.
    pub mcp: bool,
    pub usage_sync: UsageSyncConfig,
    pub egress: crate::egress::EgressConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageSyncConfig {
    pub enabled: bool,
    #[serde(deserialize_with = "de_duration", serialize_with = "ser_duration")]
    pub interval: Duration,
    #[serde(deserialize_with = "de_duration", serialize_with = "ser_duration")]
    pub request_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            upstream_base_url: "https://api.tavily.com".into(),
            upstream_timeout: Duration::from_secs(150),
            request_deadline: Duration::from_secs(180),
            max_attempts: 5,
            cooldown_on_429: Duration::from_secs(60),
            max_body_bytes: 10 * 1024 * 1024,
            master_key: None,
            data_file: PathBuf::from("./data/state.json"),
            keys_file: None,
            default_total_quota: 1000,
            flush_interval: Duration::from_secs(10),
            monthly_reset: true,
            admin_ui: true,
            mcp: true,
            usage_sync: UsageSyncConfig::default(),
            egress: Default::default(),
        }
    }
}

impl Default for UsageSyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Duration::from_secs(3600),
            request_interval: Duration::from_secs(60),
        }
    }
}

impl Config {
    /// Loads the TOML file (if any) and then applies environment overrides.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut cfg = match path {
            Some(p) => {
                let raw = std::fs::read_to_string(p)
                    .with_context(|| format!("read config file {}", p.display()))?;
                toml::from_str(&raw)
                    .with_context(|| format!("parse config file {}", p.display()))?
            }
            None => Config::default(),
        };
        cfg.apply_env(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) -> Result<()> {
        if let Some(v) = get("LISTEN_ADDR") {
            self.listen = v;
        } else if let Some(port) = get("PORT") {
            self.listen = format!("0.0.0.0:{port}");
        }
        if let Some(v) = get("TAVILY_BASE_URL") {
            self.upstream_base_url = v;
        }
        if let Some(v) = get("UPSTREAM_TIMEOUT") {
            self.upstream_timeout = parse_duration(&v).context("UPSTREAM_TIMEOUT")?;
        }
        if let Some(v) = get("REQUEST_DEADLINE") {
            self.request_deadline = parse_duration(&v).context("REQUEST_DEADLINE")?;
        }
        if let Some(v) = get("MAX_ATTEMPTS") {
            self.max_attempts = v.parse().context("MAX_ATTEMPTS")?;
        }
        if let Some(v) = get("COOLDOWN_ON_429") {
            self.cooldown_on_429 = parse_duration(&v).context("COOLDOWN_ON_429")?;
        }
        if let Some(v) = get("MASTER_KEY") {
            self.master_key = Some(v);
        }
        if let Some(v) = get("DATA_FILE") {
            self.data_file = PathBuf::from(v);
        }
        if let Some(v) = get("KEYS_FILE") {
            self.keys_file = Some(PathBuf::from(v));
        }
        if let Some(v) = get("ADMIN_UI") {
            self.admin_ui = parse_bool(&v).context("ADMIN_UI")?;
        }
        if let Some(v) = get("MCP_ENABLED") {
            self.mcp = parse_bool(&v).context("MCP_ENABLED")?;
        }
        if let Some(v) = get("MONTHLY_RESET") {
            self.monthly_reset = parse_bool(&v).context("MONTHLY_RESET")?;
        }
        if let Some(v) = get("USAGE_SYNC_ENABLED") {
            self.usage_sync.enabled = parse_bool(&v).context("USAGE_SYNC_ENABLED")?;
        }
        if let Some(v) = get("USAGE_SYNC_INTERVAL") {
            self.usage_sync.interval = parse_duration(&v).context("USAGE_SYNC_INTERVAL")?;
        }
        Ok(())
    }

    pub fn validate(&mut self) -> Result<()> {
        self.egress.validate()?;
        if self.usage_sync.request_interval < Duration::from_secs(60) {
            bail!("usage_sync.request_interval must be at least 60s");
        }
        self.upstream_base_url = self.upstream_base_url.trim_end_matches('/').to_string();
        if !self.upstream_base_url.starts_with("http://")
            && !self.upstream_base_url.starts_with("https://")
        {
            bail!("upstream_base_url must start with http:// or https://");
        }
        if self.default_total_quota == 0 {
            bail!("default_total_quota must be greater than 0");
        }
        if self.usage_sync.interval < Duration::from_secs(60) {
            bail!("usage_sync.interval must be at least 60s");
        }
        if let Some(k) = &self.master_key
            && k.trim().len() < 16
        {
            bail!("master_key must be at least 16 characters");
        }
        Ok(())
    }
}

/// Accepts `500ms`, `30s`, `10m`, `1h`, `1d`, or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .with_context(|| format!("invalid duration {s:?}"))?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .with_context(|| format!("invalid duration {s:?}"))?;
    let d = match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        "d" => Duration::from_secs(n * 86400),
        _ => bail!("invalid duration unit in {s:?}"),
    };
    Ok(d)
}

fn parse_bool(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("invalid boolean {s:?}"),
    }
}

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Secs(u64),
        Text(String),
    }
    match Raw::deserialize(d)? {
        Raw::Secs(n) => Ok(Duration::from_secs(n)),
        Raw::Text(s) => parse_duration(&s).map_err(serde::de::Error::custom),
    }
}

fn ser_duration<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    if d.subsec_millis() != 0 {
        s.serialize_str(&format!("{}ms", d.as_millis()))
    } else {
        s.serialize_str(&format!("{}s", d.as_secs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("150s").unwrap(), Duration::from_secs(150));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert!(parse_duration("10x").is_err());
    }

    #[test]
    fn toml_and_env_override() {
        let mut cfg: Config = toml::from_str(
            r#"
            listen = "127.0.0.1:9000"
            upstream_timeout = "20s"
            max_attempts = 2
            [usage_sync]
            enabled = true
            interval = "2h"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.upstream_timeout, Duration::from_secs(20));
        assert!(cfg.usage_sync.enabled);
        cfg.apply_env(|k| match k {
            "PORT" => Some("7000".into()),
            "MAX_ATTEMPTS" => Some("0".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(cfg.listen, "0.0.0.0:7000");
        assert_eq!(cfg.max_attempts, 0);
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(toml::from_str::<Config>("nope = 1").is_err());
    }

    #[test]
    fn example_config_matches_defaults() {
        let mut cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&cfg).unwrap(),
            serde_json::to_value(Config::default()).unwrap()
        );
    }
}
