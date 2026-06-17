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

    /// REST path prefix. Real Strobes deployments are fronted by nginx/ALB and
    /// expose the API under `/api/v1` — so that's the default and you don't need
    /// to set anything. Use `deployment=direct` only when hitting the Django app
    /// directly (no proxy), which serves at the bare `/v1`.
    pub fn api_prefix(&self) -> &'static str {
        match self.deployment.as_str() {
            "direct" | "v1" => "/v1",
            _ => "/api/v1",
        }
    }

    /// Normalized HTTP origin+scheme (adds https:// if missing, trims slash).
    pub fn http_base(&self) -> Result<String> {
        let mut base = self.base_url.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err(anyhow!("base_url is empty — run `strobes login` first"));
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

    /// Host portion of the base URL (no scheme or port).
    pub fn host(&self) -> Option<String> {
        let base = self.http_base().ok()?;
        url::Url::parse(&base).ok()?.host_str().map(|s| s.to_string())
    }

    /// Dev-mode fast path: a static IP to dial for the configured host,
    /// bypassing the OS resolver. This sidesteps the ~5s macOS mDNS timeout for
    /// `.local` names (which `getaddrinfo` resolves via multicast and which
    /// ignore `/etc/hosts`). Resolution order:
    ///   1. `STROBES_AI_RESOLVE` env — `"1.2.3.4"` or `"host:1.2.3.4"`.
    ///   2. for a `.local` host, the IP read straight out of `/etc/hosts`.
    /// Returns `None` for ordinary hosts, so normal DNS is used unchanged.
    pub fn resolve_override(&self) -> Option<std::net::IpAddr> {
        let host = self.host()?;
        if let Ok(raw) = std::env::var("STROBES_AI_RESOLVE") {
            let raw = raw.trim();
            if !raw.is_empty() {
                // "host:ip" (host must match) or bare "ip".
                let ip_str = match raw.rsplit_once(':') {
                    Some((h, ip)) if h.eq_ignore_ascii_case(&host) => ip,
                    Some((h, _)) if !h.is_empty() => return None, // override targets another host
                    _ => raw,
                };
                if let Ok(ip) = ip_str.trim().parse() {
                    return Some(ip);
                }
            }
        }
        if host.ends_with(".local") {
            return hosts_file_lookup(&host);
        }
        None
    }
}

/// Parse the OS hosts file for the first IP statically mapped to `host`.
fn hosts_file_lookup(host: &str) -> Option<std::net::IpAddr> {
    let path = if cfg!(windows) {
        r"C:\Windows\System32\drivers\etc\hosts"
    } else {
        "/etc/hosts"
    };
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let mut cols = line.split_whitespace();
        let ip = match cols.next() {
            Some(ip) => ip,
            None => continue,
        };
        if cols.any(|name| name.eq_ignore_ascii_case(host)) {
            if let Ok(addr) = ip.parse() {
                return Some(addr);
            }
        }
    }
    None
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
        self.profile_for(&self.current_profile)
    }

    /// Resolve a named tenant's profile with environment-variable overrides
    /// applied. Unknown names yield an empty profile (overridable by env).
    pub fn profile_for(&self, name: &str) -> Profile {
        let mut p = self.profiles.get(name).cloned().unwrap_or_default();
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

    /// Names of configured (credential-complete) tenants, sorted.
    pub fn tenants(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .profiles
            .iter()
            .filter(|(_, p)| p.is_complete())
            .map(|(n, _)| n.clone())
            .collect();
        names.sort();
        names
    }

    /// Whether the current default tenant has usable credentials.
    pub fn has_default(&self) -> bool {
        self.profiles
            .get(&self.current_profile)
            .map(|p| p.is_complete())
            .unwrap_or(false)
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
