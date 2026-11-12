//! On-disk cache of the last-seen workspace and thread lists, so the pickers
//! can paint instantly from cache and let the user act immediately, while a
//! background refetch updates the view in place (stale-while-revalidate).
//!
//! The cache is best-effort: any read/parse error just yields `None` (the
//! picker then shows a loader and waits for the live fetch), and write errors
//! are ignored. Nothing here is authoritative — the server always wins once the
//! refetch lands.

use crate::api::{Thread, Workspace};
use std::path::PathBuf;

/// `<config_dir>/cache`, created on demand.
fn cache_dir() -> PathBuf {
    crate::config::config_dir().join("cache")
}

/// Keep a tenant name safe to use as a file-name component.
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn workspaces_path(tenant: &str) -> PathBuf {
    cache_dir().join(format!("workspaces-{}.json", slug(tenant)))
}

/// Threads are cached per workspace scope ("none" for the all-threads view), so
/// switching scope doesn't show another scope's stale threads.
fn threads_path(tenant: &str, workspace: Option<&str>) -> PathBuf {
    let scope = workspace.map(slug).unwrap_or_else(|| "none".to_string());
    cache_dir().join(format!("threads-{}-{}.json", slug(tenant), scope))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_json<T: serde::Serialize>(path: &PathBuf, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(value) {
        let _ = std::fs::write(path, bytes);
    }
}

pub fn load_workspaces(tenant: &str) -> Option<Vec<Workspace>> {
    read_json(&workspaces_path(tenant))
}

pub fn save_workspaces(tenant: &str, workspaces: &[Workspace]) {
    write_json(&workspaces_path(tenant), &workspaces);
}

pub fn load_threads(tenant: &str, workspace: Option<&str>) -> Option<Vec<Thread>> {
    read_json(&threads_path(tenant, workspace))
}

pub fn save_threads(tenant: &str, workspace: Option<&str>, threads: &[Thread]) {
    write_json(&threads_path(tenant, workspace), &threads);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_and_cold_miss() {
        let dir = std::env::temp_dir().join(format!("strobes-cache-test-{}", uuid::Uuid::new_v4().simple()));
        let path = dir.join("workspaces.json");
        // Cold file → None (no panic).
        assert!(read_json::<Vec<Workspace>>(&path).is_none());

        let ws = vec![Workspace {
            id: "w1".into(),
            name: "Alpha".into(),
            status: "active".into(),
            created_at: None,
        }];
        write_json(&path, &ws);
        let got: Vec<Workspace> = read_json(&path).expect("cached workspaces");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "w1");
        assert_eq!(got[0].name, "Alpha");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn thread_cache_paths_are_scoped() {
        // Each workspace scope (and the all-threads "none" scope) maps to a
        // distinct cache file, so one scope never shows another's threads.
        let w1 = threads_path("acme", Some("w1"));
        let w2 = threads_path("acme", Some("w2"));
        let none = threads_path("acme", None);
        assert_ne!(w1, w2);
        assert_ne!(w1, none);
        assert!(none.to_string_lossy().contains("threads-acme-none.json"));
        // Tenant names are slugged so they're always safe path components.
        assert!(workspaces_path("a/b").to_string_lossy().contains("workspaces-a_b.json"));
    }
}
