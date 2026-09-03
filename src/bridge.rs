//! Ensure a Strobes Bridge is installed, running, and connected for a
//! workspace — so `workflow remote attach/create` never silently falls back
//! to the cloud sandbox for browser/command execution.
//!
//! The `strobes-bridge` repo itself is never modified: its published release
//! binaries are downloaded from GitHub and run exactly as they are, via the
//! binary's own `--daemon` mode (which backgrounds correctly on every OS —
//! no platform-specific process code needed here).
//!
//! Bridge management (list/create shells, attach to a workspace) goes through
//! a narrowly-scoped `BridgeAPIKey` (see `api.rs::create_bridge_key`), kept
//! separate from the profile's general MasterKey — a leaked bridge key can't
//! do anything but bridge management, and vice versa.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::api::ApiClient;
use crate::config::{self, Profile};

const BRIDGE_REPO: &str = "strobes-co/strobes-bridge";
const CONNECT_TIMEOUT_SECS: u64 = 45;
const POLL_INTERVAL_SECS: u64 = 3;

/// OS/arch string matching THIS repo's own release-asset naming
/// (`strobes-shell-agent-<os>-<arch>[.exe]`) — deliberately not
/// `pack::triple()`'s `x86_64`/`aarch64` convention. Confirmed by inspecting
/// the actual published release (`v0.4.0`, 2026-09-02): assets are
/// `linux-amd64`, `linux-arm64`, `macos-arm64` (no `macos-amd64` — Apple
/// Silicon only), `windows-amd64.exe`.
fn bridge_platform() -> Result<(&'static str, &'static str, &'static str)> {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        return Err(anyhow!(
            "no strobes-bridge binary published for OS '{}'",
            std::env::consts::OS
        ));
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        other => {
            return Err(anyhow!("no strobes-bridge binary published for arch '{other}'"))
        }
    };
    if os == "macos" && arch == "amd64" {
        return Err(anyhow!(
            "no strobes-bridge binary is published for macos-amd64 (Intel Mac) — \
             the maintainers currently only ship Apple Silicon binaries. Ask your \
             team to build/run the bridge manually, or use it from an Apple \
             Silicon Mac / Linux / Windows machine instead."
        ));
    }
    let ext = if os == "windows" { ".exe" } else { "" };
    Ok((os, arch, ext))
}

fn install_dir() -> PathBuf {
    config::config_dir().join("bridge")
}

fn installed_binary_path() -> PathBuf {
    let name = if cfg!(windows) { "strobes-shell-agent.exe" } else { "strobes-shell-agent" };
    install_dir().join(name)
}

fn find_installed() -> Option<PathBuf> {
    let p = installed_binary_path();
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// Download and atomically install the latest bridge release for this
/// platform, straight from GitHub — the repo is public, so this needs no
/// backend endpoint at all, matching how `cmd_update()` already fetches CLI
/// releases directly and how `pack::install()` fetches the sandbox pack.
async fn install_latest() -> Result<PathBuf> {
    use sha2::{Digest, Sha256};

    let (os, arch, ext) = bridge_platform()?;
    let asset_name = format!("strobes-shell-agent-{os}-{arch}{ext}");

    let client = reqwest::Client::builder()
        .user_agent("strobes-cli")
        .timeout(Duration::from_secs(300))
        .build()?;
    let releases_url = format!("https://api.github.com/repos/{BRIDGE_REPO}/releases/latest");
    let release: serde_json::Value = client
        .get(&releases_url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("could not reach GitHub to check the latest strobes-bridge release")?
        .error_for_status()
        .context("GitHub releases API request failed")?
        .json()
        .await
        .context("could not parse the GitHub releases response")?;

    let assets = release
        .get("assets")
        .and_then(|a| a.as_array())
        .ok_or_else(|| anyhow!("release response had no assets"))?;
    let asset = assets
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(asset_name.as_str()))
        .ok_or_else(|| anyhow!("no asset named '{asset_name}' in the latest strobes-bridge release"))?;
    let download_url = asset
        .get("browser_download_url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("asset '{asset_name}' has no download URL"))?;

    println!("↓ downloading strobes-bridge ({asset_name})…");
    let bytes = client
        .get(download_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    // Verify against the published .sha256 sibling asset when present — same
    // "verify when available, warn when not" policy as pack::install().
    let sha_name = format!("{asset_name}.sha256");
    let sha_asset = assets
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(sha_name.as_str()));
    match sha_asset.and_then(|a| a.get("browser_download_url")).and_then(|u| u.as_str()) {
        Some(sha_url) => {
            let text = client.get(sha_url).send().await?.text().await.unwrap_or_default();
            let expected = text.split_whitespace().next().unwrap_or("").to_lowercase();
            let actual: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
            if !expected.is_empty() && expected != actual {
                return Err(anyhow!(
                    "checksum mismatch for {asset_name}: expected {expected}, got {actual}. \
                     Refusing to install — this binary would run as you."
                ));
            }
            println!("✔ sha256 verified");
        }
        None => println!("⚠ no published .sha256 for {asset_name} — installing unverified"),
    }

    // Atomic install: write to a staging path, chmod, then swap — same
    // reasoning as pack::install()'s "a half-extracted result left in place
    // would hand the agent a broken runtime."
    let root = install_dir();
    std::fs::create_dir_all(&root).context("create bridge install dir")?;
    let staging = root.join(format!(".{asset_name}.incoming"));
    std::fs::write(&staging, &bytes).context("write staged bridge binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
    }
    let dest = installed_binary_path();
    let _ = std::fs::remove_file(&dest);
    std::fs::rename(&staging, &dest).context("install bridge binary")?;
    println!("✔ strobes-bridge installed to {}", dest.display());
    Ok(dest)
}

/// Get (or mint and cache on the profile) a BridgeAPIKey for this tenant.
async fn bridge_key(client: &ApiClient, profile: &Profile, tenant: &str) -> Result<String> {
    if let Some(k) = &profile.bridge_api_key {
        if !k.is_empty() {
            return Ok(k.clone());
        }
    }
    let key = client
        .create_bridge_key("cli-auto-bridge")
        .await
        .context("failed to mint a bridge key (BridgeAPIKey) — is this backend build up to date?")?;
    let mut cfg = config::Config::load();
    cfg.profile_mut(tenant).bridge_api_key = Some(key.clone());
    cfg.save()?;
    Ok(key)
}

/// Launch the installed bridge binary as a background daemon, pinned to the
/// exact `bridge_id` the Shell record was created with (rather than letting
/// the daemon self-assign one) so the connection we poll for is guaranteed to
/// be the one we just created.
fn spawn_daemon(binary: &Path, profile: &Profile, api_key: &str, bridge_id: &str) -> Result<()> {
    let dir = install_dir();
    std::fs::create_dir_all(&dir)?;
    let pid_file = dir.join("bridge.pid");
    let log_file = dir.join("bridge.log");
    let ws_base = profile.ws_base()?;

    std::process::Command::new(binary)
        .arg("connect")
        .arg("--url")
        .arg(&ws_base)
        .arg("--api-key")
        .arg(api_key)
        .arg("--org-id")
        .arg(&profile.org_id)
        .arg("--bridge-id")
        .arg(bridge_id)
        .arg("--daemon")
        .arg("--pid-file")
        .arg(&pid_file)
        .arg("--log-file")
        .arg(&log_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch {}", binary.display()))?;
    Ok(())
}

/// Poll until the shell with `shell_id` reports `bridge_connected: true`, or
/// the timeout elapses. Never falls back to proceeding unconfirmed.
async fn wait_for_shell_connection(client: &ApiClient, shell_id: &str, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let shells = client.list_shells().await?;
        if shells.iter().any(|s| s.id == shell_id && s.bridge_connected == Some(true)) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "bridge did not report as connected within {}s — check {}",
                timeout.as_secs(),
                install_dir().join("bridge.log").display()
            ));
        }
        tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }
}

/// A client authenticated with a working BridgeAPIKey — minting one via
/// `client` (the caller's normal MasterKey client; `/cli/bridge-keys/`
/// requires the general MasterKey by design) if none is cached, and
/// transparently re-minting if the cached one turns out to be
/// revoked/expired. Every shell/bridge management call
/// (`list_shells`/`create_bridge_shell`/`attach_shell_to_workspace`) must go
/// through the client this returns, never the caller's own — those endpoints
/// only accept `BridgeKeyAuthentication`, not the general MasterKey (see
/// today's `shells.py` change).
pub async fn scoped_client(
    client: &ApiClient,
    profile: &Profile,
    tenant: &str,
) -> Result<(ApiClient, String, Vec<crate::api::Shell>)> {
    let mut key = bridge_key(client, profile, tenant).await?;
    let mut bridge_profile = profile.clone();
    bridge_profile.master_key = key.clone();
    let mut bridge_client = ApiClient::new(bridge_profile)?;

    // The cached key may have been revoked/expired since it was last used —
    // mint a fresh one and retry once rather than failing outright.
    let shells = match bridge_client.list_shells().await {
        Ok(s) => s,
        Err(_) => {
            key = client.create_bridge_key("cli-auto-bridge").await
                .context("cached bridge key was rejected and minting a replacement also failed")?;
            let mut cfg = config::Config::load();
            cfg.profile_mut(tenant).bridge_api_key = Some(key.clone());
            cfg.save()?;
            let mut retried_profile = profile.clone();
            retried_profile.master_key = key.clone();
            bridge_client = ApiClient::new(retried_profile)?;
            bridge_client.list_shells().await?
        }
    };
    Ok((bridge_client, key, shells))
}

/// The main entry point: guarantee a connected bridge exists and is attached
/// to `workspace_id`, installing/launching one if needed — or return an
/// error. Callers (workflow attach/create) must treat an `Err` here as a hard
/// stop, never a reason to proceed against the cloud sandbox instead.
pub async fn ensure_local_bridge(
    client: &ApiClient,
    profile: &Profile,
    tenant: &str,
    workspace_id: &str,
) -> Result<()> {
    let (bridge_client, key, shells) = scoped_client(client, profile, tenant).await?;
    if let Some(existing) = shells
        .iter()
        .find(|s| s.shell_type == "bridge" && s.bridge_connected == Some(true))
    {
        bridge_client.attach_shell_to_workspace(workspace_id, &existing.id).await?;
        println!("✔ using already-connected bridge '{}'", existing.name);
        return Ok(());
    }

    println!("no connected bridge for this workspace — setting one up locally…");

    let binary = match find_installed() {
        Some(p) => p,
        None => install_latest().await?,
    };

    let shell_name = format!("cli-{}", &workspace_id[..8.min(workspace_id.len())]);
    let shell = bridge_client.create_bridge_shell(&shell_name).await?;
    let bridge_id = shell
        .bridge_id
        .clone()
        .ok_or_else(|| anyhow!("created shell '{}' has no bridge_id", shell.id))?;

    spawn_daemon(&binary, profile, &key, &bridge_id)?;

    println!("waiting for the bridge to connect…");
    wait_for_shell_connection(&bridge_client, &shell.id, Duration::from_secs(CONNECT_TIMEOUT_SECS)).await?;

    bridge_client.attach_shell_to_workspace(workspace_id, &shell.id).await?;
    println!("✔ bridge connected and attached to workspace");
    Ok(())
}
