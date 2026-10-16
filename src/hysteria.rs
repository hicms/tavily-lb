use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

pub(crate) struct HysteriaProcess {
    child: Child,
    binary: PathBuf,
    config: PathBuf,
    proxy: String,
    id: String,
}

impl HysteriaProcess {
    pub fn start(binary: &Path, config: &Path, proxy: &str, id: &str) -> Result<Self> {
        let url = reqwest::Url::parse(proxy)?;
        let addr = SocketAddr::new(
            url.host_str().unwrap().trim_matches(['[', ']']).parse()?,
            url.port().unwrap(),
        );
        // Never adopt an unrelated process already listening on our configured port.
        drop(
            TcpListener::bind(addr)
                .with_context(|| format!("egress {id} listener already in use"))?,
        );
        let raw =
            std::fs::read(config).with_context(|| format!("read egress {id} client config"))?;
        let cfg: serde_json::Value =
            serde_json::from_slice(&raw).context("Hysteria client config must be JSON")?;
        let mode = if url.scheme() == "http" {
            "http"
        } else {
            "socks5"
        };
        ensure!(
            cfg[mode]["listen"].as_str() == Some(&addr.to_string()),
            "egress {id} listener differs from client config"
        );
        let mut command = Command::new(binary);
        command
            .args([
                "client",
                "--disable-update-check",
                "--log-level",
                "error",
                "--config",
            ])
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = Self {
            child: command
                .spawn()
                .with_context(|| format!("start Hysteria egress {id}"))?,
            binary: binary.to_owned(),
            config: config.to_owned(),
            proxy: proxy.to_owned(),
            id: id.to_owned(),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.child.try_wait()? {
                bail!("Hysteria egress {id} exited with {status}");
            }
            if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
                return Ok(child);
            }
            ensure!(
                Instant::now() < deadline,
                "Hysteria egress {id} listener did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn ensure_running(&mut self) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            tracing::warn!(egress = %self.id, %status, "restarting exited Hysteria client");
            *self = Self::start(&self.binary, &self.config, &self.proxy, &self.id)?;
        }
        Ok(())
    }
}

impl Drop for HysteriaProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
