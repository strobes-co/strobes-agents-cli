//! Sandbox pack integration — the CLI half of what the bridge already does.
//!
//! A "sandbox pack" is a self-contained, relocatable directory that reproduces the
//! Strobes cloud sandbox runtime on a user's machine: a standalone Python with the
//! agent packages baked in (boto3, reportlab, curl_cffi, cryptography, …) plus CLI
//! security tools in `bin/` (nuclei, httpx, subfinder, dnsx, ffuf, gobuster, nmap).
//! No Docker, no root, no system Python, no internet at runtime.
//!
//! The bridge daemon has shipped this for a while (`strobes_shell_agent/pack.py`).
//! The CLI executes agent code on the *same kind of machine* and had none of it, so
//! an agent working through the CLI got whatever happened to be on the user's PATH
//! — which in a live run meant no `strobes_pt`, no scanners, and several wasted
//! turns discovering that. Same problem, same fix.
//!
//! Resolution order (matches the bridge so one machine can share one pack):
//!   1. `STROBES_PACK_PATH` — an already-extracted pack directory.
//!   2. `STROBES_PACK_DIR/<triple>` — explicit root.
//!   3. `~/.strobes-ai/pack/<triple>` — this CLI's own install dir.
//!   4. `~/.strobes-shell-agent/pack/<triple>` — the BRIDGE's pack. Deliberate: if
//!      you already installed the bridge on this box, the CLI should use what is
//!      there rather than make you download a second copy of the same thing.
//!
//! Everything degrades gracefully: with no pack, behaviour is exactly what it was
//! before this module existed.

use std::path::{Path, PathBuf};

const PACK_PATH_ENV: &str = "STROBES_PACK_PATH";
const PACK_DIR_ENV: &str = "STROBES_PACK_DIR";
const PACK_DISABLE_ENV: &str = "STROBES_PACK_DISABLE";

fn truthy(v: &str) -> bool {
    !matches!(v.trim().to_lowercase().as_str(), "" | "0" | "false" | "no" | "off")
}

/// Platform triple used for pack naming, e.g. `macos-aarch64`.
pub fn triple() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        std::env::consts::OS
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// A directory is a pack if it carries the manifest the builder writes.
fn is_pack(p: &Path) -> bool {
    p.join("pack.manifest.json").is_file()
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

fn candidates() -> Vec<PathBuf> {
    let t = triple();
    let mut out = Vec::new();
    if let Ok(p) = std::env::var(PACK_PATH_ENV) {
        if !p.trim().is_empty() {
            out.push(PathBuf::from(p));
        }
    }
    if let Ok(root) = std::env::var(PACK_DIR_ENV) {
        if !root.trim().is_empty() {
            out.push(PathBuf::from(root).join(&t));
        }
    }
    if let Some(h) = home() {
        out.push(h.join(".strobes-ai").join("pack").join(&t));
        // The bridge's pack — same machine, same artifact, no reason to duplicate.
        out.push(h.join(".strobes-shell-agent").join("pack").join(&t));
    }
    out
}

/// The pack to use, if any.
pub fn find_pack() -> Option<PathBuf> {
    if std::env::var(PACK_DISABLE_ENV).map(|v| truthy(&v)).unwrap_or(false) {
        return None;
    }
    candidates().into_iter().find(|p| is_pack(p))
}

/// The pack's `bin/` directory, if the pack has one.
pub fn pack_bin(pack: &Path) -> Option<PathBuf> {
    let b = pack.join("bin");
    if b.is_dir() {
        Some(b)
    } else {
        None
    }
}

/// The pack's standalone interpreter, located by walking `python/`.
///
/// The directory carries the CPython build's own version in its name
/// (`python/cpython-3.12.x/...`), so it is discovered rather than hardcoded — a
/// pinned path would silently stop resolving the next time the pack is rebuilt.
pub fn pack_python(pack: &Path) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "python.exe" } else { "python3" };
    let root = pack.join("python");
    if !root.is_dir() {
        return None;
    }
    let mut stack = vec![root];
    let mut depth = 0;
    while let Some(dir) = stack.pop() {
        if depth > 512 {
            break; // pathological tree; stop rather than hang the tool call
        }
        depth += 1;
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if matches!(name, "bin" | "lib" | "Scripts") || name.starts_with("cpython") {
                    stack.push(p);
                }
            } else if p.file_name().and_then(|s| s.to_str()) == Some(exe) {
                return Some(p);
            }
        }
    }
    None
}

/// The interpreter agent code should run under: the pack's, else the host's.
pub fn python_interpreter() -> String {
    find_pack()
        .and_then(|p| pack_python(&p))
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| if cfg!(windows) { "python".into() } else { "python3".into() })
}

/// Just the pack's own directories, joined — no inherited PATH.
///
/// Needed separately from [`path_with_pack`] because the shell path prepends
/// inside a login shell (`export PATH=<prefix>:"$PATH"`), where re-appending the
/// parent's PATH would duplicate every entry.
pub fn pack_path_prefix() -> Option<String> {
    let pack = find_pack()?;
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut parts: Vec<String> = Vec::new();
    if let Some(bin) = pack_bin(&pack) {
        parts.push(bin.to_string_lossy().to_string());
    }
    if let Some(py) = pack_python(&pack) {
        if let Some(dir) = py.parent() {
            parts.push(dir.to_string_lossy().to_string());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(sep))
    }
}

/// PATH with the pack's tools in front, or None when there is no pack.
///
/// Prepended, not replaced: the user's own tooling stays reachable, the pack just
/// wins ties. That keeps a host that already has a newer `nmap` working the way
/// its owner expects.
pub fn path_with_pack() -> Option<String> {
    let prefix = pack_path_prefix()?;
    let sep = if cfg!(windows) { ";" } else { ":" };
    match std::env::var("PATH") {
        Ok(existing) if !existing.is_empty() => Some(format!("{prefix}{sep}{existing}")),
        _ => Some(prefix),
    }
}

/// One-line description for `strobes status`, so a user can tell at a glance
/// whether the agent has the toolset or is running bare.
pub fn status_line() -> String {
    match find_pack() {
        None => "pack        (none — agent uses host tools)".into(),
        Some(p) => {
            let tools = pack_bin(&p)
                .and_then(|b| std::fs::read_dir(b).ok())
                .map(|d| d.flatten().count())
                .unwrap_or(0);
            let py = pack_python(&p).is_some();
            format!(
                "pack        {} ({} tools, python: {})",
                p.display(),
                tools,
                if py { "bundled" } else { "host" }
            )
        }
    }
}

/// Where packs are published. The BRIDGE already builds one per platform in its
/// `build_sandbox_pack.yml` and attaches the tarballs + `.sha256` to a `pack-v*`
/// release. The CLI consumes those same artifacts rather than building its own:
/// the pack is a property of the machine, not of which client is driving it, and
/// two pipelines producing "the same" runtime is how they drift apart.
///
/// Overridable with `STROBES_PACK_URL`, the same env var the bridge honours.
const PACK_REPO: &str = "strobes-co/strobes-bridge";

/// Packs are published on their OWN `pack-v*` releases, not on the agent's
/// release. `releases/latest/download` therefore resolves to the newest AGENT
/// release, which carries no pack at all — measured: 404 for the pack tarball on
/// `latest`, 200 on `pack-v0.1.0`. So the tag is resolved rather than assumed.
const PACK_TAG_FALLBACK: &str = "pack-v0.1.0";

fn pack_base_for_tag(tag: &str) -> String {
    format!("https://github.com/{PACK_REPO}/releases/download/{tag}")
}

/// Newest `pack-v*` tag, via the public releases API; falls back to a known-good
/// tag so a rate-limited or offline API cannot break installation outright.
async fn latest_pack_tag(client: &reqwest::Client) -> String {
    let url = format!("https://api.github.com/repos/{PACK_REPO}/releases?per_page=50");
    let resp = client
        .get(&url)
        .header("User-Agent", "strobes-cli")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await;
    if let Ok(r) = resp {
        if let Ok(items) = r.json::<serde_json::Value>().await {
            if let Some(arr) = items.as_array() {
                for item in arr {
                    if let Some(tag) = item.get("tag_name").and_then(|v| v.as_str()) {
                        if tag.starts_with("pack-v") {
                            return tag.to_string();
                        }
                    }
                }
            }
        }
    }
    PACK_TAG_FALLBACK.to_string()
}

/// Download, verify and extract a pack for this platform.
///
/// Returns the installed pack directory. Verification is not optional: this
/// unpacks executables that the agent will then run as the user, so a tarball
/// whose sha256 does not match the published one is discarded rather than used.
pub async fn install(dest_root: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    use anyhow::{anyhow, Context};
    use sha2::{Digest, Sha256};

    let t = triple();
    let name = format!("strobes-sandbox-pack-{t}.tar.gz");

    let root = dest_root.unwrap_or_else(|| {
        home().unwrap_or_else(|| PathBuf::from(".")).join(".strobes-ai").join("pack")
    });
    let dest = root.join(&t);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1800))
        .build()?;
    let base = match std::env::var("STROBES_PACK_URL") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => pack_base_for_tag(&latest_pack_tag(&client).await),
    };
    let url = format!("{}/{}", base.trim_end_matches('/'), name);
    println!("↓ downloading {url}");
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "pack not available for {t} ({}). Set STROBES_PACK_URL to a base URL \
             that hosts {name}, or install the Strobes bridge, which ships one.",
            resp.status()
        ));
    }
    let bytes = resp.bytes().await?;

    // Published checksum, when there is one. Absence is a warning, not a failure —
    // a self-hosted STROBES_PACK_URL may serve only the tarball.
    let sha_url = format!("{url}.sha256");
    match client.get(&sha_url).send().await {
        Ok(r) if r.status().is_success() => {
            let text = r.text().await.unwrap_or_default();
            let expected = text.split_whitespace().next().unwrap_or("").to_lowercase();
            let actual: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if !expected.is_empty() && expected != actual {
                return Err(anyhow!(
                    "checksum mismatch for {name}: expected {expected}, got {actual}. \
                     Refusing to install — this pack's binaries would run as you."
                ));
            }
            println!("✔ sha256 verified");
        }
        _ => println!("⚠ no published .sha256 — installing unverified"),
    }

    // Extract to a temp dir first, then swap. A half-extracted pack left in place
    // would satisfy find_pack() (the manifest may already be written) and hand the
    // agent a broken runtime.
    let staging = root.join(format!(".{t}.incoming"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).context("create staging dir")?;
    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(&bytes[..]));
    tar::Archive::new(decoder)
        .unpack(&staging)
        .context("extract pack")?;

    // The tarball may contain the triple dir itself, or the pack contents directly.
    let inner = if is_pack(&staging) {
        staging.clone()
    } else {
        std::fs::read_dir(&staging)?
            .flatten()
            .map(|e| e.path())
            .find(|p| is_pack(p))
            .ok_or_else(|| anyhow!("archive contains no pack.manifest.json"))?
    };

    let _ = std::fs::remove_dir_all(&dest);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&inner, &dest).context("install pack")?;
    let _ = std::fs::remove_dir_all(&staging);

    println!("✔ pack installed to {}", dest.display());
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env vars are PROCESS-global and cargo runs tests in parallel, so a test
    /// that sets `STROBES_PACK_DISABLE` was changing what an unrelated test saw
    /// mid-assertion. Measured 2 flaky runs in 6 before this. Every test that
    /// touches the environment takes this lock.
    static ENV: Mutex<()> = Mutex::new(());

    #[test]
    fn triple_is_os_dash_arch() {
        let t = triple();
        assert!(t.contains('-'), "triple should look like macos-aarch64: {t}");
    }

    #[test]
    fn a_directory_without_a_manifest_is_not_a_pack() {
        let tmp = std::env::temp_dir().join("strobes-pack-test-empty");
        let _ = std::fs::create_dir_all(&tmp);
        assert!(!is_pack(&tmp));
    }

    #[test]
    fn disable_env_wins_over_everything() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        // Ordering matters more than the value: a user who sets DISABLE expects
        // the host's own environment, even with a perfectly good pack installed.
        std::env::set_var(PACK_DISABLE_ENV, "1");
        let found = find_pack();
        std::env::remove_var(PACK_DISABLE_ENV);
        assert!(found.is_none());
    }

    #[test]
    fn packs_come_from_the_bridge_repo_not_a_cli_specific_one() {
        // One runtime, one pipeline. If this ever points somewhere CLI-specific,
        // the two clients have started drifting.
        assert!(pack_base_for_tag(PACK_TAG_FALLBACK).contains("strobes-bridge"));
    }

    #[test]
    fn the_pack_url_never_uses_latest_download() {
        // `releases/latest/download` resolves to the newest AGENT release, which
        // carries no pack — measured 404. Packs live on their own pack-v* tags.
        let url = pack_base_for_tag(PACK_TAG_FALLBACK);
        assert!(!url.contains("/latest/"), "must not use the latest release: {url}");
        assert!(url.contains("pack-v"));
    }

    #[test]
    fn no_pack_means_host_python_and_untouched_path() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(PACK_DISABLE_ENV, "1");
        let interp = python_interpreter();
        let path = path_with_pack();
        std::env::remove_var(PACK_DISABLE_ENV);
        assert!(interp.ends_with("python3") || interp.ends_with("python"));
        assert!(path.is_none(), "PATH must be left alone when there is no pack");
    }
}
