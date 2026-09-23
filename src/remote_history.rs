//! Local, CLI-side memory of remote workflows.
//!
//! The backend only ever exposes the workflow instance currently attached to
//! a workspace: `GET /cli/workspaces/<id>/workflow/`. When a new workflow is
//! attached over a finished one (completed/failed/cancelled), the backend
//! deletes the old row outright (`_replace_terminal_or_conflict`) — there is
//! no server-side history endpoint for the CLI's MasterKey auth to call.
//!
//! So the CLI keeps its own lightweight record of every workflow state it has
//! ever observed, keyed by workflow id, and updates it every time it polls a
//! workspace's workflow (via the workflow list/picker and the live watch
//! TUI). That's what lets `strobes workflow remote watch` still show a
//! workflow that has since been superseded and deleted server-side — clearly
//! labeled as an archived, read-only local record, not live data.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::api::WorkflowState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub workspace_id: String,
    pub workspace_name: String,
    pub state: WorkflowState,
    /// RFC 3339 timestamp of the last time the CLI actually observed this
    /// workflow (may be well after `state.completed_at` if it sat idle).
    pub last_seen_at: String,
}

fn history_dir() -> std::path::PathBuf {
    crate::config::config_dir().join("remote-workflow-history")
}

fn path_for(workflow_id: &str) -> std::path::PathBuf {
    // Workflow ids are server-issued UUIDs — safe to use directly as a filename.
    history_dir().join(format!("{workflow_id}.json"))
}

/// Persist/refresh the local record for an observed workflow. Best-effort:
/// a failure to write history must never block the caller's real work.
pub fn record(workspace_id: &str, workspace_name: &str, state: &WorkflowState) {
    if state.workflow_id.is_empty() {
        return;
    }
    let rec = HistoryRecord {
        workspace_id: workspace_id.to_string(),
        workspace_name: workspace_name.to_string(),
        state: state.clone(),
        last_seen_at: crate::workflow_state::current_ts(),
    };
    let _ = save(&rec);
}

fn save(rec: &HistoryRecord) -> Result<()> {
    let dir = history_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(rec)?;
    std::fs::write(path_for(&rec.state.workflow_id), json)?;
    Ok(())
}

/// All locally recorded workflows, newest-observed first.
pub fn list_all() -> Vec<HistoryRecord> {
    let dir = history_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut records: Vec<HistoryRecord> = rd
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| {
            let json = std::fs::read_to_string(e.path()).ok()?;
            serde_json::from_str::<HistoryRecord>(&json).ok()
        })
        .collect();
    records.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
    records
}

/// Locally recorded workflows for one workspace, excluding `live_workflow_id`
/// (the one still returned live by the backend, if any) — i.e. just the
/// superseded/archived ones — newest-observed first.
pub fn archived_for_workspace(workspace_id: &str, live_workflow_id: Option<&str>) -> Vec<HistoryRecord> {
    list_all()
        .into_iter()
        .filter(|r| r.workspace_id == workspace_id)
        .filter(|r| Some(r.state.workflow_id.as_str()) != live_workflow_id)
        .collect()
}
