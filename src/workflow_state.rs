// WORKFLOW FEATURE — part of the workflow system. Remove along with workflow*.rs to roll back.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Partial,
}

impl RunStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Partial => "partial",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatus {
    Pending,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub name: String,
    pub status: TaskRunStatus,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseRecord {
    pub name: String,
    pub tasks: Vec<TaskRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub workflow_name: String,
    /// Absolute path to the YAML file (for re-loading on resume).
    pub workflow_file: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: RunStatus,
    pub workspace_id: Option<String>,
    pub vars: HashMap<String, String>,
    pub phases: Vec<PhaseRecord>,
}

impl RunRecord {
    pub fn new(
        id: String,
        workflow_name: String,
        workflow_file: String,
        started_at: String,
        vars: HashMap<String, String>,
        phases: Vec<PhaseRecord>,
    ) -> Self {
        Self {
            id,
            workflow_name,
            workflow_file,
            started_at,
            finished_at: None,
            status: RunStatus::Running,
            workspace_id: None,
            vars,
            phases,
        }
    }

    pub fn task_mut(&mut self, task_name: &str) -> Option<&mut TaskRecord> {
        for phase in &mut self.phases {
            for task in &mut phase.tasks {
                if task.name == task_name {
                    return Some(task);
                }
            }
        }
        None
    }

    pub fn done_task_names(&self) -> HashSet<String> {
        self.phases
            .iter()
            .flat_map(|p| p.tasks.iter())
            .filter(|t| t.status == TaskRunStatus::Done)
            .map(|t| t.name.clone())
            .collect()
    }

    pub fn total_tasks(&self) -> usize {
        self.phases.iter().map(|p| p.tasks.len()).sum()
    }

    pub fn done_count(&self) -> usize {
        self.phases
            .iter()
            .flat_map(|p| p.tasks.iter())
            .filter(|t| t.status == TaskRunStatus::Done)
            .count()
    }
}

pub fn runs_dir() -> PathBuf {
    crate::config::config_dir().join("workflow-runs")
}

/// Generate a run ID: `YYYYMMDD-HHMMSS-<slug>` and an RFC 3339 timestamp.
pub fn new_run_id(workflow_name: &str) -> (String, String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ts = format_ts(now);
    let slug: String = workflow_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = if slug.len() > 24 { slug[..24].to_string() } else { slug };
    let id = format!("{ts}-{slug}");
    let started_at = ts_to_rfc3339(now);
    (id, started_at)
}

pub fn current_ts() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    ts_to_rfc3339(secs)
}

fn format_ts(secs: u64) -> String {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
}

fn ts_to_rfc3339(secs: u64) -> String {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut y = 1970u64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_days: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];
    let mut mo = 0u64;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        mo += 1;
    }
    (y, mo + 1, days + 1)
}

pub fn save(record: &RunRecord) -> Result<()> {
    let dir = runs_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", record.id));
    let json = serde_json::to_string_pretty(record)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn load(id: &str) -> Result<RunRecord> {
    let path = runs_dir().join(format!("{id}.json"));
    let json = std::fs::read_to_string(&path)
        .map_err(|_| anyhow!("run '{id}' not found in {}", runs_dir().display()))?;
    serde_json::from_str(&json).map_err(|e| anyhow!("corrupt run record {id}: {e}"))
}

/// List all run records, sorted newest-first.
pub fn list_runs() -> Vec<RunRecord> {
    let dir = runs_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut records: Vec<RunRecord> = rd
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                == Some("json")
        })
        .filter_map(|e| {
            let json = std::fs::read_to_string(e.path()).ok()?;
            serde_json::from_str::<RunRecord>(&json).ok()
        })
        .collect();
    records.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    records
}
