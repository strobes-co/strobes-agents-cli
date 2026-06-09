//! Profile/config storage — shares `~/.config/strobes-ai/config.json` with the
//! Python CLI, so `strobes-ai login` already configures this Rust client too.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub org_id: String,
    #[serde(default)]
    pub master_key: String,
    #[serde(default = "default_deployment")]
    pub deployment: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub shell_bridge_id: Option<String>,
    #[serde(default)]
    pub browser_id: Option<String>,
    #[serde(default = "default_true")]
    pub verify_tls: bool,
}

fn default_deployment() -> String {
    "saas".into()
}
fn default_true() -> bool {
    true
}

impl Profile {
    pub fn is_complete(&self) -> bool {
        !self.base_url.is_empty() && !self.org_id.is_empty() && !self.master_key.is_empty()
    }

    /// REST/GraphQL path prefix. SaaS mounts under `/v1`, enterprise under
    /// `/api/v1` (which is also what an nginx-fronted deployment needs, since
    /// nginx strips the external `/api` prefix).
    pub fn api_prefix(&self) -> &'static str {
        if self.deployment == "enterprise" {
            "/api/v1"
        } else {
            "/v1"
        }
    }

    /// Normalized HTTP origin+scheme (adds https:// if missing, trims slash).
    pub fn http_base(&self) -> Result<String> {
        let mut base = self.base_url.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err(anyhow!("base_url is empty — run `strobes-ai login` first"));
        }
        if !base.contains("://") {
            base = format!("https://{base}");
        }
        Ok(base)
    }

    /// ws:// or wss:// origin derived from the configured base URL.
    pub fn ws_base(&self) -> Result<String> {
        let http = self.http_base()?;
        let parsed = url::Url::parse(&http)?;
        let scheme = if parsed.scheme() == "https" { "wss" } else { "ws" };
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow!("base_url has no host"))?;
        let port = parsed
            .port()
            .map(|p| format!(":{p}"))
            .unwrap_or_default();
        Ok(format!("{scheme}://{host}{port}"))
    }

    /// Pulse WebSocket URL for a thread, with the api_key query param.
    pub fn pulse_ws_url(&self, thread_id: &str) -> Result<String> {
        Ok(format!(
            "{}/ws/{}/pulse/{}/?api_key={}",
            self.ws_base()?,
            self.org_id,
            thread_id,
            self.master_key
        ))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_profile_name")]
    pub current_profile: String,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// Local folders bound to workspaces (workspace_id -> absolute path).
    #[serde(default)]
    pub workspace_dirs: BTreeMap<String, String>,
}

fn default_profile_name() -> String {
    "default".into()
}

pub fn config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("STROBES_AI_HOME") {
        return PathBuf::from(home);
    }
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("strobes-ai")
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        let mut cfg: Config = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if cfg.profiles.is_empty() {
            cfg.profiles.insert("default".into(), Profile::default());
        }
        if cfg.current_profile.is_empty() {
            cfg.current_profile = "default".into();
        }
        cfg
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)?;
        let path = config_path();
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Active profile with environment-variable overrides applied.
    pub fn current(&self) -> Profile {
        let mut p = self
            .profiles
            .get(&self.current_profile)
            .cloned()
            .unwrap_or_default();
        if let Ok(v) = std::env::var("STROBES_AI_BASE_URL") {
            p.base_url = v;
        }
        if let Ok(v) = std::env::var("STROBES_AI_ORG_ID") {
            p.org_id = v;
        }
        if let Ok(v) = std::env::var("STROBES_AI_MASTER_KEY") {
            p.master_key = v;
        }
        if let Ok(v) = std::env::var("STROBES_AI_DEPLOYMENT") {
            p.deployment = v;
        }
        p
    }

    pub fn profile_mut(&mut self, name: &str) -> &mut Profile {
        self.profiles.entry(name.to_string()).or_default()
    }
}

pub fn redact(secret: &str) -> String {
    if secret.is_empty() {
        return "(unset)".into();
    }
    if secret.len() <= 8 {
        return "*".repeat(secret.len());
    }
    format!("{}…{}", &secret[..4], &secret[secret.len() - 4..])
}
