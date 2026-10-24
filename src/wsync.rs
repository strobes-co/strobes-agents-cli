//! The workspace, as the agent sees it, on the machine running the CLI.
//!
//! Every agent, prompt and skill names `~/.strobes/workspace`. On a cloud
//! sandbox that path IS the workspace (an S3 Files mount); here the workspace is
//! mirrored into `<config>/workspaces/<id>/` — and nothing used to connect the
//! two. An agent opened `~/.strobes/workspace/docs/scope.md`, found nothing,
//! and had to search the disk for the mirror; what it wrote under
//! `~/.strobes/workspace` landed in the user's real home (shared by every
//! workspace) and never reached the workspace; and the mirror was downloaded
//! once, so a document uploaded afterwards never arrived.
//!
//! Three things, the same model the SSH and bridge shells use:
//!
//! * **A per-workspace HOME** for local tool commands:
//!   `<config>/homes/<id>/` whose `.strobes/workspace` links to the mirror and
//!   whose other entries link to the real home's (`.ssh`, `.aws`, `Library`…),
//!   so `~/.strobes/workspace` means this workspace and the user's own keys and
//!   tools still work.
//! * **Pull** before a tool call: whatever the mirror lacks or holds at a
//!   different size, at most every `PULL_EVERY`.
//! * **Push** after a tool call: files the agent created or changed in the
//!   mirror, bounded per pass.
//!
//! Only the workspace mirror is ever synced — never a `--sandbox-id` directory,
//! which CI fills with the user's own codebase. Dotfiles, the CLI's own scratch
//! files, symlinks and multiply-linked files never leave the machine.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::{api, config};

const PULL_EVERY: Duration = Duration::from_secs(30);
const MAX_PUSH_FILES: usize = 200;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

static PROFILE: OnceLock<config::Profile> = OnceLock::new();
static LAST_PULL: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

/// Called once per connection with the profile the API calls use.
pub fn set_profile(p: &config::Profile) {
    let _ = PROFILE.set(p.clone());
}

fn valid_ws(ws: &str) -> Option<String> {
    uuid::Uuid::parse_str(ws).ok().map(|u| u.hyphenated().to_string())
}

/// The directory a workspace is mirrored into.
pub fn mirror_dir(ws: &str) -> PathBuf {
    config::config_dir().join("workspaces").join(ws)
}

/// Is `sandbox` this workspace's mirror (and so allowed to sync)?
fn is_mirror(ws: &str, sandbox: &Path) -> bool {
    let a = std::fs::canonicalize(sandbox).ok();
    let b = std::fs::canonicalize(mirror_dir(ws)).ok();
    a.is_some() && a == b
}

/// The HOME local commands for workspace `ws` run with, created on demand.
/// `None` outside a workspace mirror or on Windows (no HOME there).
pub fn workspace_home(ws: &str, sandbox: &Path) -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let ws = valid_ws(ws)?;
    if !is_mirror(&ws, sandbox) {
        return None;
    }
    let real = dirs::home_dir()?;
    let home = config::config_dir().join("homes").join(&ws);
    let strobes = home.join(".strobes");
    std::fs::create_dir_all(&strobes).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link = strobes.join("workspace");
        let want = std::fs::canonicalize(sandbox).ok()?;
        let ok = std::fs::read_link(&link).map(|t| t == want).unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_file(&link);
            symlink(&want, &link).ok()?;
        }
        if let Ok(entries) = std::fs::read_dir(&real) {
            for e in entries.flatten() {
                let name = e.file_name();
                if name == ".strobes" {
                    continue;
                }
                let l = home.join(&name);
                if std::fs::symlink_metadata(&l).is_err() {
                    let _ = symlink(e.path(), &l);
                }
            }
        }
    }
    Some(home)
}

/// Relative path → safe components, or None if it could leave the mirror.
fn safe_rel(rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() { None } else { Some(out) }
}

/// Never synced: dotfiles and the CLI's own scratch (code snippets, todos).
fn is_private(name: &str) -> bool {
    name.starts_with('.') || name.starts_with("snippet-") || name == "todos.json"
}

/// Regular, singly-linked files under `root`, with sizes. No symlinks followed.
fn local_files(root: &Path) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if is_private(&name) {
                continue;
            }
            let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            if md.is_dir() {
                stack.push((e.path(), rel));
            } else if md.is_file() && single_link(&md) {
                out.insert(rel, md.len());
            }
        }
    }
    out
}

#[cfg(unix)]
fn single_link(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    md.nlink() == 1
}
#[cfg(not(unix))]
fn single_link(_md: &std::fs::Metadata) -> bool { true }

/// Does any component between `root` and `path` (inclusive) exist as a symlink?
fn crosses_link(root: &Path, rel: &Path) -> bool {
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        if std::fs::symlink_metadata(&cur).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return true;
        }
    }
    false
}

async fn remote_files(client: &api::ApiClient, ws: &str) -> anyhow::Result<HashMap<String, u64>> {
    Ok(client
        .list_workspace_files(ws, true)
        .await?
        .into_iter()
        .filter(|f| !f.is_folder && !f.path.is_empty())
        .map(|f| (f.path.trim_start_matches('/').to_string(), f.size))
        .collect())
}

/// Bring the mirror up to date before a local tool call. Never fails the call.
pub async fn before(ws: &str, sandbox: &Path) {
    let Some(ws) = valid_ws(ws) else { return };
    if !is_mirror(&ws, sandbox) {
        return;
    }
    {
        let m = LAST_PULL.get_or_init(|| Mutex::new(HashMap::new()));
        let mut g = m.lock().unwrap();
        if g.get(&ws).map(|t| t.elapsed() < PULL_EVERY).unwrap_or(false) {
            return;
        }
        g.insert(ws.clone(), Instant::now());
    }
    let Some(profile) = PROFILE.get() else { return };
    let Ok(client) = api::ApiClient::new(profile.clone()) else { return };
    let Ok(remote) = remote_files(&client, &ws).await else { return };
    let local = local_files(sandbox);
    for (path, size) in remote {
        if size > MAX_FILE_BYTES || local.get(&path) == Some(&size) {
            continue;
        }
        let Some(rel) = safe_rel(&path) else { continue };
        if crosses_link(sandbox, &rel) {
            continue;
        }
        if let Ok(Some(bytes)) = client.download_workspace_file(&ws, &path).await {
            let dest = sandbox.join(&rel);
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let tmp = dest.with_extension(format!("strobes-part-{}", uuid::Uuid::new_v4().simple()));
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &dest);
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// Send what the agent wrote to the mirror back to the workspace.
pub async fn after(ws: &str, sandbox: &Path) {
    let Some(ws) = valid_ws(ws) else { return };
    if !is_mirror(&ws, sandbox) {
        return;
    }
    let Some(profile) = PROFILE.get() else { return };
    let Ok(client) = api::ApiClient::new(profile.clone()) else { return };
    let local = local_files(sandbox);
    if local.is_empty() {
        return;
    }
    let Ok(remote) = remote_files(&client, &ws).await else { return };
    let mut changed: Vec<(String, u64)> = local
        .into_iter()
        .filter(|(p, s)| *s <= MAX_FILE_BYTES && remote.get(p) != Some(s))
        .collect();
    changed.sort();
    for (path, _) in changed.into_iter().take(MAX_PUSH_FILES) {
        let Some(rel) = safe_rel(&path) else { continue };
        if crosses_link(sandbox, &rel) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(sandbox.join(&rel)) {
            let _ = client.upload_workspace_file(&ws, &path, bytes).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_leave_the_mirror() {
        assert!(safe_rel("../x").is_none());
        assert!(safe_rel("/etc/passwd").is_none());
        assert!(safe_rel("a/../../b").is_none());
        assert_eq!(safe_rel("docs/scope.md").unwrap(), PathBuf::from("docs/scope.md"));
    }

    #[test]
    fn private_and_linked_files_are_not_listed() {
        let d = std::env::temp_dir().join(format!("wsync-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        let r = d.as_path();
        std::fs::create_dir_all(r.join("notes")).unwrap();
        std::fs::write(r.join("notes/a.md"), b"aa").unwrap();
        std::fs::write(r.join(".strobes-sync-complete"), b"").unwrap();
        std::fs::write(r.join("snippet-123.py"), b"x").unwrap();
        std::fs::write(r.join("todos.json"), b"[]").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hosts", r.join("leak")).unwrap();
            std::fs::hard_link(r.join("notes/a.md"), r.join("hard")).unwrap();
        }
        let files = local_files(r);
        let mut names: Vec<_> = files.keys().cloned().collect();
        names.sort();
        // a.md has two links now, so neither name is synced.
        #[cfg(unix)]
        assert!(names.is_empty(), "{names:?}");
        #[cfg(not(unix))]
        assert_eq!(names, vec!["notes/a.md".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
