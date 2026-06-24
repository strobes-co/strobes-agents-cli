use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::api::ApiClient;
use crate::config::Profile;
use crate::pulse::{self, AppEvent, StreamItem};
use crate::workflow::{interpolate, PhaseDef, TaskDef, WorkflowDef};
use crate::workflow_state::{
    self, PhaseRecord, RunRecord, RunStatus, TaskRecord, TaskRunStatus,
};

/// Convert any StreamItem into a displayable string for the workflow output pane.
/// Returns None only for events that carry no meaningful user-visible content.
fn format_stream_item(item: &StreamItem) -> Option<String> {
    match item.kind.as_str() {
        "token" => item.text.clone(),
        "thinking" => item.text.as_ref().map(|t| format!("💭 {t}")),
        "tool_start" => {
            let name = item.tool_name.as_deref().unwrap_or("?");
            let detail = item.detail.as_deref().unwrap_or("");
            if detail.is_empty() {
                Some(format!("\n▶ {name}\n"))
            } else {
                Some(format!("\n▶ {name}({detail})\n"))
            }
        }
        "tool_output" => {
            let name = item.tool_name.as_deref().unwrap_or("?");
            let detail = item.detail.as_deref().unwrap_or("");
            if detail.is_empty() {
                None
            } else {
                Some(format!("◀ {name}: {detail}\n"))
            }
        }
        "tool_failed" => {
            let name = item.tool_name.as_deref().unwrap_or("?");
            let err = item.detail.as_deref().unwrap_or("unknown error");
            Some(format!("✗ {name}: {err}\n"))
        }
        "task" => item.text.as_ref().map(|t| {
            let status = item.status.as_deref().unwrap_or("");
            if status.is_empty() {
                format!("[task] {t}\n")
            } else {
                format!("[task:{status}] {t}\n")
            }
        }),
        "note" | "system" => item.text.as_ref().map(|t| format!("ℹ {t}\n")),
        "approval" => item.text.as_ref().map(|t| format!("[auto-approved] {t}\n")),
        _ => item.text.clone(),
    }
}

#[derive(Debug, Clone)]
pub enum WfEvent {
    WorkspaceReady { id: String, name: String },
    /// Workspace setup thread started — TUI inserts a dedicated setup task entry.
    SetupStarted { thread_id: String },
    PhaseStarted { phase: String },
    TaskStarted { phase: String, task: String, thread_id: String },
    TaskOutput { task: String, text: String },
    TaskDone { task: String },
    TaskFailed { task: String, reason: String },
    /// Task was already completed in a prior run and is being skipped on resume.
    TaskSkipped { task: String },
    WorkflowDone,
    WorkflowFailed { reason: String },
    Log(String),
}

type Shared<T> = Arc<Mutex<T>>;

pub async fn run(
    def: WorkflowDef,
    profile: Profile,
    ev_tx: mpsc::UnboundedSender<WfEvent>,
    extra_vars: HashMap<String, String>,
    resume: Option<RunRecord>,
    workflow_file: String,
) -> Result<()> {
    // Merge variables: workflow-level defaults, then caller overrides.
    let mut vars = def.variables.clone();
    vars.extend(extra_vars);

    let is_resume = resume.is_some();

    // ── Build or restore run record ───────────────────────────────────────────
    let (run_id, started_at) = if let Some(ref r) = resume {
        (r.id.clone(), r.started_at.clone())
    } else {
        workflow_state::new_run_id(&def.name)
    };

    let phase_records: Vec<PhaseRecord> = def
        .phases
        .iter()
        .map(|p| PhaseRecord {
            name: p.name.clone(),
            tasks: p
                .tasks
                .iter()
                .map(|t| {
                    let (status, thread_id) = resume
                        .as_ref()
                        .and_then(|r| r.phases.iter().find(|pr| pr.name == p.name))
                        .and_then(|pr| pr.tasks.iter().find(|tr| tr.name == t.name))
                        .map(|tr| (tr.status.clone(), tr.thread_id.clone()))
                        .unwrap_or((TaskRunStatus::Pending, None));
                    TaskRecord { name: t.name.clone(), status, thread_id }
                })
                .collect(),
        })
        .collect();

    let wf_file = resume
        .as_ref()
        .map(|r| r.workflow_file.clone())
        .unwrap_or(workflow_file);

    let record: Shared<RunRecord> = Arc::new(Mutex::new(RunRecord::new(
        run_id,
        def.name.clone(),
        wf_file,
        started_at,
        vars.clone(),
        phase_records,
    )));

    let client = ApiClient::new(profile.clone())?;

    // ── Workspace ─────────────────────────────────────────────────────────────
    let (workspace_id, setup_thread) = if is_resume {
        // Reuse the workspace from the prior run.
        let ws_id = resume
            .as_ref()
            .and_then(|r| r.workspace_id.clone())
            .ok_or_else(|| anyhow!("resume record has no workspace_id"))?;
        let _ = ev_tx.send(WfEvent::Log(format!("resuming workspace {ws_id}")));
        let _ = ev_tx.send(WfEvent::WorkspaceReady {
            id: ws_id.clone(),
            name: ws_id.clone(),
        });
        (ws_id, None)
    } else {
        match &def.workspace {
            Some(ws) if ws.id.is_some() => {
                let id = ws.id.clone().unwrap();
                let _ = ev_tx.send(WfEvent::Log(format!("using workspace {id}")));
                let _ = ev_tx.send(WfEvent::WorkspaceReady { id: id.clone(), name: id.clone() });
                (id, None)
            }
            Some(ws) => {
                let name = interpolate(ws.name.as_deref().unwrap_or(&def.name), &vars);
                let _ = ev_tx.send(WfEvent::Log(format!("creating workspace: {name}")));
                let (id, setup) = client
                    .create_workspace(&name)
                    .await
                    .map_err(|e| anyhow!("create workspace: {e}"))?;
                let _ = ev_tx.send(WfEvent::WorkspaceReady { id: id.clone(), name });
                (id, setup)
            }
            None => match &profile.workspace_id {
                Some(id) => {
                    let _ = ev_tx.send(WfEvent::WorkspaceReady {
                        id: id.clone(),
                        name: id.clone(),
                    });
                    (id.clone(), None)
                }
                None => {
                    let name = interpolate(&def.name, &vars);
                    let _ = ev_tx.send(WfEvent::Log(format!("creating workspace: {name}")));
                    let (id, setup) = client
                        .create_workspace(&name)
                        .await
                        .map_err(|e| anyhow!("create workspace: {e}"))?;
                    let _ = ev_tx.send(WfEvent::WorkspaceReady { id: id.clone(), name });
                    (id, setup)
                }
            },
        }
    };

    // Persist workspace_id into the run record.
    {
        let mut rec = record.lock().unwrap();
        rec.workspace_id = Some(workspace_id.clone());
        let _ = workflow_state::save(&rec);
    }

    let mut ws_profile = profile.clone();
    ws_profile.workspace_id = Some(workspace_id.clone());

    // Run workspace setup only for fresh runs.
    if !is_resume {
        if let Some(thread_id) = setup_thread {
            run_workspace_setup(&thread_id, &def, &ws_profile, &ev_tx).await?;
        }
    }

    // ── Pre-populate completed set from resume record ─────────────────────────
    let pre_done: HashSet<String> = resume
        .as_ref()
        .map(|r| r.done_task_names())
        .unwrap_or_default();

    let completed: Shared<HashSet<String>> = Arc::new(Mutex::new(pre_done.clone()));
    let failed: Shared<HashSet<String>> = Arc::new(Mutex::new(HashSet::new()));

    // Emit skip events so the TUI marks prior-done tasks immediately.
    for task_name in &pre_done {
        let _ = ev_tx.send(WfEvent::TaskSkipped { task: task_name.clone() });
    }

    for phase in &def.phases {
        let _ = ev_tx.send(WfEvent::PhaseStarted { phase: phase.name.clone() });
        run_phase(
            phase,
            &vars,
            &ws_profile,
            &workspace_id,
            &ev_tx,
            completed.clone(),
            failed.clone(),
            record.clone(),
        )
        .await?;
    }

    // ── Finalise run record ───────────────────────────────────────────────────
    {
        let mut rec = record.lock().unwrap();
        let all_done = rec
            .phases
            .iter()
            .all(|p| p.tasks.iter().all(|t| t.status == TaskRunStatus::Done));
        rec.status = if all_done {
            RunStatus::Completed
        } else if rec.done_count() == 0 {
            RunStatus::Failed
        } else {
            RunStatus::Partial
        };
        rec.finished_at = Some(workflow_state::current_ts());
        let _ = workflow_state::save(&rec);
    }

    let _ = ev_tx.send(WfEvent::WorkflowDone);
    Ok(())
}

async fn run_phase(
    phase: &PhaseDef,
    vars: &HashMap<String, String>,
    profile: &Profile,
    workspace_id: &str,
    ev_tx: &mpsc::UnboundedSender<WfEvent>,
    completed: Shared<HashSet<String>>,
    failed: Shared<HashSet<String>>,
    run_record: Shared<RunRecord>,
) -> Result<()> {
    let mut remaining: Vec<TaskDef> = phase.tasks.clone();
    let mut running: JoinSet<()> = JoinSet::new();

    loop {
        let (comp_snap, fail_snap): (HashSet<String>, HashSet<String>) = {
            let c = completed.lock().unwrap().clone();
            let f = failed.lock().unwrap().clone();
            (c, f)
        };

        let mut to_start: Vec<TaskDef> = Vec::new();
        let mut to_fail: Vec<TaskDef> = Vec::new();
        let mut new_remaining: Vec<TaskDef> = Vec::new();

        for task in remaining.drain(..) {
            if task.depends_on.iter().any(|d| fail_snap.contains(d)) {
                to_fail.push(task);
            } else if task.depends_on.iter().all(|d| comp_snap.contains(d)) {
                to_start.push(task);
            } else {
                new_remaining.push(task);
            }
        }
        remaining = new_remaining;

        for task in to_fail {
            failed.lock().unwrap().insert(task.name.clone());
            let _ = ev_tx.send(WfEvent::TaskFailed {
                task: task.name.clone(),
                reason: "dependency failed".into(),
            });
        }

        for task in to_start {
            let phase_name = interpolate(&phase.name, vars);
            let task_name = interpolate(&task.name, vars);
            let mut merged = vars.clone();
            merged.extend(task.vars.clone());
            let prompt = interpolate(&task.prompt, &merged);
            let model = task.model;
            let profile = profile.clone();
            let workspace_id = workspace_id.to_string();
            let ev_tx2 = ev_tx.clone();
            let completed2 = completed.clone();
            let failed2 = failed.clone();
            let run_record2 = run_record.clone();

            running.spawn(async move {
                let result = run_task(
                    &phase_name,
                    &task_name,
                    &prompt,
                    &profile,
                    &workspace_id,
                    &ev_tx2,
                    model,
                    run_record2.clone(),
                )
                .await;

                match result {
                    Ok(()) => {
                        completed2.lock().unwrap().insert(task_name.clone());
                        {
                            let mut rec = run_record2.lock().unwrap();
                            if let Some(tr) = rec.task_mut(&task_name) {
                                tr.status = TaskRunStatus::Done;
                            }
                            let _ = workflow_state::save(&rec);
                        }
                        let _ = ev_tx2.send(WfEvent::TaskDone { task: task_name });
                    }
                    Err(reason) => {
                        failed2.lock().unwrap().insert(task_name.clone());
                        {
                            let mut rec = run_record2.lock().unwrap();
                            if let Some(tr) = rec.task_mut(&task_name) {
                                tr.status = TaskRunStatus::Failed;
                            }
                            let _ = workflow_state::save(&rec);
                        }
                        let _ = ev_tx2.send(WfEvent::TaskFailed { task: task_name, reason });
                    }
                }
            });
        }

        if running.is_empty() && remaining.is_empty() {
            break;
        }

        if running.is_empty() {
            return Err(anyhow!(
                "workflow stuck: circular dependency or all remaining tasks blocked"
            ));
        }

        running.join_next().await;
    }

    Ok(())
}

const SETUP_TASK: &str = "workspace-setup";

async fn run_workspace_setup(
    thread_id: &str,
    def: &WorkflowDef,
    profile: &Profile,
    ev_tx: &mpsc::UnboundedSender<WfEvent>,
) -> Result<()> {
    let _ = ev_tx.send(WfEvent::SetupStarted { thread_id: thread_id.to_string() });

    let phase_list = def
        .phases
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let task_names: Vec<&str> = p.tasks.iter().map(|t| t.name.as_str()).collect();
            format!(
                "  {}. {} — {} task(s): {}",
                i + 1,
                p.name,
                p.tasks.len(),
                task_names.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let desc_line = if def.description.is_empty() {
        String::new()
    } else {
        format!("\n{}\n", def.description)
    };

    let msg = format!(
        "This workspace is being used to run an automated workflow.\n\
         \n\
         Workflow: {}{}\n\
         Phases:\n{}\n\
         \n\
         Please set up the workspace environment so it is ready to execute \
         these tasks. Configure any required tools, dependencies, or context \
         that the agents will need. Once setup is complete, the workflow phases \
         will begin automatically.",
        def.name, desc_line, phase_list
    );

    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<AppEvent>();
    let handle = pulse::connect(profile, thread_id, app_tx, None)
        .await
        .map_err(|e| anyhow!("workspace setup connect: {e}"))?;

    handle.send_user_message(&msg);

    loop {
        match app_rx.recv().await {
            Some(AppEvent::RunFinished(label)) => {
                if label.starts_with("failed") {
                    let _ = ev_tx.send(WfEvent::TaskFailed {
                        task: SETUP_TASK.into(),
                        reason: label,
                    });
                    let _ = ev_tx
                        .send(WfEvent::Log("workspace setup failed — continuing".into()));
                } else {
                    let _ = ev_tx.send(WfEvent::TaskDone { task: SETUP_TASK.into() });
                    let _ = ev_tx
                        .send(WfEvent::Log("✔ workspace setup complete".into()));
                }
                break;
            }
            Some(AppEvent::Error(e)) => {
                let _ = ev_tx.send(WfEvent::TaskFailed {
                    task: SETUP_TASK.into(),
                    reason: e.clone(),
                });
                let _ = ev_tx.send(WfEvent::Log(format!(
                    "workspace setup warning: {e} — continuing"
                )));
                break;
            }
            Some(AppEvent::Stream(item)) => {
                if let Some(text) = format_stream_item(&item) {
                    let _ = ev_tx.send(WfEvent::TaskOutput {
                        task: SETUP_TASK.into(),
                        text,
                    });
                }
            }
            Some(_) => {}
            None => break,
        }
    }

    Ok(())
}

async fn run_task(
    phase: &str,
    task: &str,
    prompt: &str,
    profile: &Profile,
    workspace_id: &str,
    ev_tx: &mpsc::UnboundedSender<WfEvent>,
    model: Option<i64>,
    run_record: Shared<RunRecord>,
) -> Result<(), String> {
    let client = ApiClient::new(profile.clone()).map_err(|e| e.to_string())?;
    let title = format!("[{phase}] {task}");
    let thread_id = client
        .create_thread(&title, Some(workspace_id))
        .await
        .map_err(|e| format!("create_thread: {e}"))?;

    // Persist thread_id so resume can reference it.
    {
        let mut rec = run_record.lock().unwrap();
        if let Some(tr) = rec.task_mut(task) {
            tr.thread_id = Some(thread_id.clone());
        }
        let _ = workflow_state::save(&rec);
    }

    let _ = ev_tx.send(WfEvent::TaskStarted {
        phase: phase.to_string(),
        task: task.to_string(),
        thread_id: thread_id.clone(),
    });

    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<AppEvent>();
    let handle = pulse::connect(profile, &thread_id, app_tx, model)
        .await
        .map_err(|e| format!("connect: {e}"))?;

    handle.send_user_message(prompt);

    let task_name = task.to_string();
    let ev_tx = ev_tx.clone();

    loop {
        match app_rx.recv().await {
            Some(AppEvent::RunFinished(label)) => {
                if label.starts_with("failed") {
                    return Err(label);
                }
                return Ok(());
            }
            Some(AppEvent::Error(e)) => return Err(e),
            Some(AppEvent::Stream(item)) => {
                if let Some(text) = format_stream_item(&item) {
                    let _ = ev_tx.send(WfEvent::TaskOutput {
                        task: task_name.clone(),
                        text,
                    });
                }
            }
            Some(_) => {}
            None => return Err("connection closed unexpectedly".into()),
        }
    }
}
