//! `strobes` — a Ratatui terminal client for Strobes Agents AI.
//!
//! Reuses the same `~/.config/strobes-ai/config.json` as the Python CLI, so
//! `strobes-ai login` configures this client too. The `chat` subcommand opens
//! an interactive Ratatui session that streams a remote agent run and executes
//! its tools locally (the user's machine is the sandbox).

mod api;
mod app;
mod browser;
mod config;
mod local;
mod markdown;
mod picker;
mod pulse;
mod workflow;
mod remote_wf_tui;
mod workflow_runner;
mod workflow_state;
mod workflow_tui;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use app::App;
use config::Config;

/// Capture mouse events so trackpad/wheel scroll reaches the transcript.
/// (Native click-drag selection still works while holding Option/Shift.)
pub fn enable_mouse() {
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
}

pub fn disable_mouse() {
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
}

/// Alert the user that a response finished, by writing terminal escape codes the
/// emulator interprets — cross-platform (macOS/Linux/Windows terminals) and no
/// external processes. Most terminals only surface these when the window is
/// unfocused. Tunables:
///   STROBES_AI_NOTIFY=off            disable entirely
///   STROBES_AI_NOTIFY_MIN_SECS=<n>   only notify for runs ≥ n seconds (default 4)
fn notify_response_done(secs: u64) {
    let disabled = std::env::var("STROBES_AI_NOTIFY")
        .map(|v| v.eq_ignore_ascii_case("off") || v == "0")
        .unwrap_or(false);
    let min = std::env::var("STROBES_AI_NOTIFY_MIN_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(4);
    if let Some(bytes) = notify_done_bytes(secs, disabled, min) {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    }
}

/// The escape bytes for a done-notification, or None when suppressed (disabled,
/// or the run was shorter than `min` seconds). BEL = universal attention;
/// OSC 9 = desktop notification (iTerm2/WezTerm/Windows Terminal); OSC 777 =
/// the notify form used by urxvt and others. Terminals ignore what they can't grok.
fn notify_done_bytes(secs: u64, disabled: bool, min: u64) -> Option<Vec<u8>> {
    if disabled || secs < min {
        return None;
    }
    let msg = "Strobes Agents — response ready";
    Some(
        format!("\x07\x1b]9;{msg}\x07\x1b]777;notify;Strobes Agents;response ready\x07")
            .into_bytes(),
    )
}

#[derive(Parser)]
#[command(name = "strobes", version, about = "Ratatui client for Strobes Agents AI")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Tenant to use for this run (defaults to the configured default tenant).
    #[arg(long, visible_alias = "profile", global = true)]
    tenant: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive chat TUI (default).
    Chat {
        #[arg(long, short)]
        thread: Option<String>,
        #[arg(long, short)]
        workspace: Option<String>,
        #[arg(long, short)]
        model: Option<i64>,
        /// Force the thread picker / create a new thread instead of resuming.
        #[arg(long)]
        new: bool,
    },
    /// Configure credentials interactively (or pass flags to skip prompts).
    Login {
        /// Deployment base URL, e.g. https://app.strobes.co
        #[arg(long)]
        base_url: Option<String>,
        /// Organization UUID.
        #[arg(long)]
        org_id: Option<String>,
        /// 40-char MasterKey.
        #[arg(long)]
        master_key: Option<String>,
        /// Path style: blank/proxy = /api/v1, `direct` = bare /v1.
        #[arg(long)]
        deployment: Option<String>,
        /// Skip the post-save connectivity check.
        #[arg(long)]
        no_verify: bool,
    },
    /// Update the CLI to the latest release (downloads + replaces the binary).
    Update {
        /// Reinstall even if already on the latest version.
        #[arg(long)]
        force: bool,
    },
    /// List configured tenants (the default is marked with ★).
    Tenants,
    /// Show AI credit usage (org total + per-workspace breakdown).
    Credits {
        /// Scope to a single workspace.
        #[arg(long, short)]
        workspace: Option<String>,
        /// Scope to a single chat thread.
        #[arg(long, short)]
        thread: Option<String>,
    },
    /// Show the active profile and connectivity.
    Status,
    /// List remote workspaces.
    Workspaces,
    /// List your chat threads.
    Threads,
    /// Pick (or create) a workspace to bind, optionally downloading it locally.
    Bind {
        #[arg(long, short)]
        workspace: Option<String>,
        #[arg(long)]
        new: bool,
        #[arg(long, default_value = "CLI Workspace")]
        name: String,
        /// Also download the workspace files to a local folder.
        #[arg(long)]
        download: bool,
        #[arg(long)]
        dir: Option<String>,
    },
    /// Download a workspace's files to a local folder (binds the folder).
    Pull {
        #[arg(long, short)]
        workspace: Option<String>,
        #[arg(long)]
        dir: Option<String>,
    },
    /// Upload local file(s) to a workspace.
    Push {
        /// Local file paths to upload.
        files: Vec<String>,
        #[arg(long, short)]
        workspace: Option<String>,
        /// Destination dir/prefix inside the workspace (default: file name at root).
        #[arg(long)]
        dir: Option<String>,
    },
    /// Headless probe: connect, stream events to stdout, run local tools.
    /// Used to verify the WS + local-execution path without the TUI.
    Probe {
        #[arg(long, short)]
        thread: String,
        /// Optionally send a prompt on connect.
        #[arg(long)]
        send: Option<String>,
        /// Seconds to stay connected.
        #[arg(long, default_value = "200")]
        secs: u64,
        /// LLM model picker id (e.g. 4 = Haiku, 18 = Sonnet 4.6).
        #[arg(long, short)]
        model: Option<i64>,
    },
    /// Run YAML-based offline workflows (sequence, parallel, DAG).
    Workflow {
        #[command(subcommand)]
        sub: WorkflowCmd,
    },
    /// Create a thread, send one message, stream the response to stdout, then exit.
    /// Useful for scripting and for launching orchestrator prompts that spawn sub-agents.
    Send {
        /// Message to send (the full prompt text).
        message: String,
        /// Attach the thread to this existing workspace ID.
        #[arg(long, short)]
        workspace: Option<String>,
        /// Create a new workspace with this name and attach the thread to it.
        #[arg(long)]
        new_workspace: Option<String>,
        /// Thread title (defaults to first 60 chars of the message).
        #[arg(long, short)]
        title: Option<String>,
        /// LLM model picker id.
        #[arg(long, short)]
        model: Option<i64>,
    },
}

#[derive(Subcommand)]
enum WorkflowCmd {
    /// Run a YAML workflow file.
    Run {
        /// Path to the workflow YAML file.
        file: String,
        /// Override a workflow variable (KEY=VALUE). Repeatable.
        #[arg(long, short = 'v', value_name = "KEY=VALUE")]
        var: Vec<String>,
        /// Print events to stdout instead of opening the TUI.
        #[arg(long)]
        no_tui: bool,
    },
    /// List workflow YAML files (.yaml/.yml with phases:) in the current directory.
    List,
    /// Write a starter workflow template (defaults to stdout).
    Init {
        /// Write to this file instead of stdout.
        #[arg(long, short)]
        output: Option<String>,
    },
    /// Show history of locally recorded workflow runs.
    History,
    /// Resume a previously interrupted workflow run.
    Resume {
        /// Run ID shown by `strobes workflow history`.
        id: String,
        /// Print events to stdout instead of opening the TUI.
        #[arg(long)]
        no_tui: bool,
    },
    /// Manage remote workflows via the Strobes GraphQL API.
    Remote {
        #[command(subcommand)]
        sub: RemoteWorkflowCmd,
    },
}

#[derive(Subcommand)]
enum RemoteWorkflowCmd {
    /// List available workflow templates (built-in and custom:).
    Templates,
    /// Show the workflow currently attached to a workspace.
    Status {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Attach a workflow template to a workspace and start it.
    Attach {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Template slug, e.g. "web-pentest" or "custom:my-template".
        /// Omit to pick interactively.
        #[arg(long, short)]
        template: Option<String>,
        /// Set a workflow variable (KEY=VALUE). Repeatable.
        #[arg(long, short = 'v', value_name = "KEY=VALUE")]
        var: Vec<String>,
    },
    /// Detach (cancel + remove) the workflow from a workspace.
    Detach {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Create a new remote workflow from a local YAML file.
    Create {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Local workflow YAML file to push.
        #[arg(long, short)]
        file: String,
        /// Override a workflow variable (KEY=VALUE). Repeatable.
        #[arg(long, short = 'v', value_name = "KEY=VALUE")]
        var: Vec<String>,
    },
    /// Edit the existing remote workflow from a local YAML file.
    /// Not allowed while the workflow is running.
    Edit {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Local workflow YAML file.
        #[arg(long, short)]
        file: String,
    },
    /// Smart sync: push a local YAML to remote — creates if none, edits if one exists.
    Sync {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Local workflow YAML file.
        #[arg(long, short)]
        file: String,
        /// Override a workflow variable (KEY=VALUE). Repeatable.
        #[arg(long, short = 'v', value_name = "KEY=VALUE")]
        var: Vec<String>,
    },
    /// Save the current remote workflow as a reusable custom template.
    Save {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Display name for the template (gets a custom: prefix automatically).
        #[arg(long, short)]
        name: String,
        /// Template description.
        #[arg(long, short = 'd')]
        description: Option<String>,
        /// Emoji icon shown next to the template.
        #[arg(long, short = 'i', default_value = "🔒")]
        icon: String,
    },
    /// Delete a custom workflow template (only custom: slugs can be deleted).
    DeleteTemplate {
        /// Template slug, e.g. "custom:my-template".
        slug: String,
    },
    /// Pause the running workflow.
    Pause {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Resume a paused workflow.
    Resume {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Cancel the running or paused workflow.
    Cancel {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Restart the workflow (from the beginning, or from a specific phase).
    Restart {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Restart from this phase key rather than from the beginning.
        #[arg(long)]
        from_phase: Option<String>,
    },
    /// Advance past a manual-gate phase.
    Advance {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Open a live TUI showing workflow phase status with pause/resume/detach controls.
    Watch {
        #[arg(long, short)]
        workspace: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = Config::load();
    // Select the tenant for this run without mutating the stored default.
    let tenant = cli.tenant.clone().unwrap_or_else(|| cfg.current_profile.clone());
    let profile = cfg.profile_for(&tenant);

    match cli.cmd.unwrap_or(Cmd::Chat { thread: None, workspace: None, model: None, new: false }) {
        Cmd::Login { base_url, org_id, master_key, deployment, no_verify } => {
            cmd_login(&mut cfg, &tenant, base_url, org_id, master_key, deployment, no_verify).await
        }
        Cmd::Update { force } => cmd_update(force).await,
        Cmd::Tenants => cmd_tenants(&cfg),
        Cmd::Credits { workspace, thread } => cmd_credits(&profile, workspace, thread).await,
        Cmd::Status => cmd_status(&profile).await,
        Cmd::Workspaces => cmd_workspaces(&profile).await,
        Cmd::Threads => cmd_threads(&profile).await,
        Cmd::Bind { workspace, new, name, download, dir } => {
            cmd_bind(&mut cfg, &tenant, profile, workspace, new, name, download, dir).await
        }
        Cmd::Pull { workspace, dir } => cmd_pull(&mut cfg, &profile, workspace, dir).await,
        Cmd::Push { files, workspace, dir } => cmd_push(&cfg, &profile, files, workspace, dir).await,
        Cmd::Chat { thread, workspace, model, new } => {
            // Enter the alternate screen ONCE for the whole interactive flow
            // (pickers + chat) and restore ONCE, so switching between workspace,
            // thread, and chat never flashes the normal terminal.
            let mut terminal = ratatui::init();
            // Capture the mouse so trackpad/wheel scroll moves the transcript.
            // Native text selection/copy still works via Option/Shift-drag, and
            // ^Y copies the whole transcript.
            //
            // NOTE: we deliberately do NOT enable the kitty keyboard protocol
            // here. On terminals that support it, it changes ESC[-sequence
            // parsing and collides with SGR mouse reports — leaking raw scroll
            // sequences (e.g. `[<64;..M`) into the input. Newlines use Ctrl+J.
            enable_mouse();
            let r = chat_flow(&mut terminal, &mut cfg, &tenant, profile, thread, workspace, new, model).await;
            disable_mouse();
            ratatui::restore();
            r
        }
        Cmd::Probe { thread, send, secs, model } => cmd_probe(&profile, &thread, send, secs, model).await,
        Cmd::Workflow { sub } => cmd_workflow(profile, sub, &tenant).await,
        Cmd::Send { message, workspace, new_workspace, title, model } => {
            cmd_send(&profile, message, workspace, new_workspace, title, model).await
        }
    }
}

/// The interactive chat flow: pick a workspace → thread (unless given), persist
/// the binding, then run the chat UI — all on one already-initialized terminal.
async fn chat_flow(
    terminal: &mut ratatui::DefaultTerminal,
    cfg: &mut Config,
    tenant: &str,
    mut profile: config::Profile,
    thread: Option<String>,
    workspace: Option<String>,
    new: bool,
    model: Option<i64>,
) -> Result<()> {
    if let Some(w) = &workspace {
        profile.workspace_id = Some(w.clone());
    }
    // Only an explicit --thread skips the pickers; a plain run always offers
    // workspace → thread selection (the stored thread_id is not auto-resumed).
    let explicit = if new { None } else { thread };
    // `initial_msg` is auto-sent on connect (used to seed a new workspace's
    // setup chat with the user's prompt).
    let mut initial_msg: Option<String> = None;
    let thread_id = match explicit {
        Some(t) => t,
        None => {
            // Esc on the thread picker steps back to the workspace picker; Esc on
            // the workspace picker (or ^C anywhere) quits cleanly.
            let ws_flag = workspace.is_some();
            loop {
                if !ws_flag {
                    match resolve_workspace_interactive(terminal, tenant, &profile, cfg).await? {
                        WsChoice::Pick(w) => {
                            // Count this open for "recent" ranking next time.
                            if let Some(id) = &w {
                                cfg.record_workspace_open(id);
                            }
                            profile.workspace_id = w;
                        }
                        WsChoice::Created { id, setup_thread, prompt } => {
                            cfg.record_workspace_open(&id);
                            profile.workspace_id = Some(id);
                            if let Some(t) = setup_thread {
                                // Jump straight into the setup chat with the prompt.
                                initial_msg = prompt;
                                break t;
                            }
                            // No setup thread → fall through to the thread picker.
                        }
                        WsChoice::Quit => return Ok(()),
                    }
                }
                match resolve_thread_interactive(terminal, tenant, &profile).await? {
                    ThreadChoice::Pick(t) => break t,
                    ThreadChoice::Quit => return Ok(()),
                    // No workspace picker to go back to when --workspace was given.
                    ThreadChoice::Back if ws_flag => return Ok(()),
                    ThreadChoice::Back => continue,
                }
            }
        }
    };
    // Persist the binding to the selected tenant's profile.
    {
        let p = cfg.profile_mut(tenant);
        p.thread_id = Some(thread_id.clone());
        if profile.workspace_id.is_some() {
            p.workspace_id = profile.workspace_id.clone();
        }
        let _ = cfg.save();
    }
    run_chat(terminal, tenant, profile, thread_id, model, initial_msg).await
}

/// Show a workspace picker over the available workspaces and return the chosen
/// workspace id. A leading "No workspace" entry (and an empty list) yields None,
/// leaving the chat unscoped. The currently-bound workspace is marked with ✓.
/// First-screen workspace choice. Esc/^C both quit the app cleanly (there's no
/// earlier screen to step back to).
enum WsChoice {
    Pick(Option<String>),
    /// A freshly-created workspace: bind it, open its setup thread, and send the
    /// optional setup prompt as the first message.
    Created {
        id: String,
        setup_thread: Option<String>,
        prompt: Option<String>,
    },
    Quit,
}

/// One-line "authenticated …" subtitle shown under the banner art.
fn auth_line(p: &config::Profile, tenant: &str) -> String {
    let org = if p.org_id.len() > 8 { &p.org_id[..8] } else { &p.org_id };
    format!("✔ authenticated · tenant: {tenant} · {} · org {org}", p.base_url)
}

async fn resolve_workspace_interactive(
    terminal: &mut ratatui::DefaultTerminal,
    tenant: &str,
    profile: &config::Profile,
    cfg: &Config,
) -> Result<WsChoice> {
    require_complete(profile)?;
    let auth = auth_line(profile, tenant);
    let client = api::ApiClient::new(profile.clone())?;
    let mut workspaces = client.list_workspaces().await.unwrap_or_default();
    // Surface frequently-opened workspaces first (most opens, then most recent).
    // Stable sort keeps never-opened ones in the server's order below.
    workspaces.sort_by(|a, b| {
        cfg.workspace_open_count(&b.id)
            .cmp(&cfg.workspace_open_count(&a.id))
            .then(cfg.workspace_last_opened(&b.id).cmp(&cfg.workspace_last_opened(&a.id)))
    });

    // Fetch workflow state for every workspace concurrently so we can badge rows.
    let wf_states: Vec<Option<api::WorkflowState>> = {
        let futs: Vec<_> = workspaces.iter().map(|w| client.workspace_workflow(&w.id)).collect();
        futures_util::future::join_all(futs)
            .await
            .into_iter()
            .map(|r| r.ok().flatten())
            .collect()
    };

    let cur = profile.workspace_id.as_deref();
    // Item 0 = create new, item 1 = no workspace, then existing workspaces (offset 2).
    let mut labels = vec![
        "➕  New workspace".to_string(),
        "↪  No workspace (all threads)".to_string(),
    ];
    for (w, wf) in workspaces.iter().zip(wf_states.iter()) {
        let name = if w.name.is_empty() { "(unnamed)" } else { &w.name };
        let mark = if Some(w.id.as_str()) == cur { "  ✓" } else { "" };
        let count = cfg.workspace_open_count(&w.id);
        let recent = if count > 0 { format!("  · ↻ ×{count}") } else { String::new() };
        let wf_badge = match wf.as_ref().map(|s| s.status.as_str()) {
            Some("running") => "  ⚙ running",
            Some("paused") => "  ⚙ paused",
            Some("pending") => "  ⚙ pending",
            Some("failed") => "  ⚙ failed",
            Some("completed") => "  ⚙ done",
            _ => "",
        };
        labels.push(format!("{name}   [{}]{mark}{recent}{wf_badge}", w.status));
    }

    // Add Tab hint as a context line below the auth banner.
    let auth_with_hint = format!("{auth}\n↹ Tab on a workspace: open live workflow TUI");

    loop {
        match picker::select_with(terminal, "Select a workspace", &labels, &auth_with_hint).await? {
            picker::Nav::Item(0) => {
                // Ask for a name and an optional setup prompt; Esc on the name
                // cancels back to the picker.
                let name = match picker::prompt_text(terminal, "New workspace — name", "", &auth).await? {
                    Some(n) => n.trim().to_string(),
                    None => continue,
                };
                let name = if name.is_empty() { "CLI Workspace".to_string() } else { name };
                let prompt = picker::prompt_text(
                    terminal,
                    "Setup prompt — what should this workspace do? (optional)",
                    "",
                    &auth,
                )
                .await?
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
                let (id, setup_thread) = client.create_workspace(&name).await?;
                return Ok(WsChoice::Created { id, setup_thread, prompt });
            }
            picker::Nav::Item(1) => return Ok(WsChoice::Pick(None)),
            picker::Nav::Item(i) => return Ok(WsChoice::Pick(Some(workspaces[i - 2].id.clone()))),
            // Tab on a workspace row → open the Watch TUI inline if a workflow exists.
            picker::Nav::Shortcut(i) if i >= 2 => {
                let ws = &workspaces[i - 2];
                if wf_states[i - 2].is_some() {
                    let _ = remote_wf_tui::run(terminal, &client, ws.id.clone(), profile.clone(), tenant.to_string()).await;
                    terminal.clear()?;
                }
                // Fall through to re-show the workspace picker.
            }
            picker::Nav::Shortcut(_) => {} // Tab on "New" / "No workspace" — ignore.
            // Esc on the first screen quits cleanly (no prior screen to return to).
            picker::Nav::Back | picker::Nav::Quit => return Ok(WsChoice::Quit),
        }
    }
}

/// Second-screen thread choice. Esc steps back to the workspace picker; ^C quits.
enum ThreadChoice {
    Pick(String),
    Back,
    Quit,
}

/// Show a thread picker (with a "new thread" option) and return the chosen
/// thread id, creating one via REST if requested.
async fn resolve_thread_interactive(
    terminal: &mut ratatui::DefaultTerminal,
    tenant: &str,
    profile: &config::Profile,
) -> Result<ThreadChoice> {
    require_complete(profile)?;
    let auth = auth_line(profile, tenant);
    let client = api::ApiClient::new(profile.clone())?;
    // Scope to the bound workspace if there is one.
    let threads = client
        .list_threads(profile.workspace_id.as_deref())
        .await
        .unwrap_or_default();

    // Context lines under the ASCII art: which workspace these threads belong
    // to, plus its headline counts (threads · credits · findings · files).
    let auth = match profile.workspace_id.as_deref() {
        Some(id) => {
            // Resolve the name and the stats counts concurrently.
            let (workspaces, (credits, findings, files)) =
                tokio::join!(client.list_workspaces(), fetch_workspace_stats(&client, id));
            let name = workspaces
                .ok()
                .and_then(|ws| ws.into_iter().find(|w| w.id.as_str() == id).map(|w| w.name))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("{}…", &id[..8.min(id.len())]));
            let stats = format!(
                "{} threads  ·  ◈ {credits:.2} credits  ·  ⚠ {findings} findings  ·  {files} files",
                threads.len()
            );
            format!("{auth}\n⊞ workspace: {name}\n{stats}")
        }
        None => format!("{auth}\n⊞ workspace: (none — all threads)"),
    };

    let mut labels = vec!["➕  New thread".to_string()];
    for t in &threads {
        let title = if t.title.is_empty() { "(untitled)".into() } else { t.title.clone() };
        let last = if t.last_message.is_empty() { String::new() } else { format!("  — {}", trunc(&t.last_message, 50)) };
        labels.push(format!("{title}   [{}]{last}", t.status));
    }

    match picker::select_with(terminal, "Select a thread", &labels, &auth).await? {
        picker::Nav::Item(0) => {
            let id = client
                .create_thread("CLI session", profile.workspace_id.as_deref())
                .await?;
            Ok(ThreadChoice::Pick(id))
        }
        picker::Nav::Item(i) => Ok(ThreadChoice::Pick(threads[i - 1].id.clone())),
        picker::Nav::Back | picker::Nav::Shortcut(_) => Ok(ThreadChoice::Back),
        picker::Nav::Quit => Ok(ThreadChoice::Quit),
    }
}

async fn cmd_bind(
    cfg: &mut Config,
    tenant: &str,
    profile: config::Profile,
    workspace: Option<String>,
    new: bool,
    name: String,
    download: bool,
    dir: Option<String>,
) -> Result<()> {
    require_complete(&profile)?;
    let client = api::ApiClient::new(profile.clone())?;

    let ws_id = if new {
        let (id, _setup) = client.create_workspace(&name).await?;
        println!("✔ created workspace {id} ({name})");
        id
    } else if let Some(w) = workspace {
        w
    } else {
        let workspaces = client.list_workspaces().await?;
        if workspaces.is_empty() {
            return Err(anyhow!("no workspaces — use --new to create one"));
        }
        let labels: Vec<String> = workspaces
            .iter()
            .map(|w| format!("{}   [{}]", if w.name.is_empty() { "(unnamed)" } else { &w.name }, w.status))
            .collect();
        match picker::select("Select a workspace", &labels).await? {
            picker::Nav::Item(i) => workspaces[i].id.clone(),
            _ => return Ok(()),
        }
    };

    cfg.profile_mut(tenant).workspace_id = Some(ws_id.clone());
    cfg.save()?;
    println!("✔ bound workspace {ws_id}");

    if download {
        download_workspace(cfg, &profile, &ws_id, dir).await?;
    } else {
        println!("(run `strobes pull` to download its files locally)");
    }
    Ok(())
}

async fn cmd_pull(
    cfg: &mut Config,
    profile: &config::Profile,
    workspace: Option<String>,
    dir: Option<String>,
) -> Result<()> {
    require_complete(profile)?;
    let ws_id = workspace
        .or_else(|| profile.workspace_id.clone())
        .ok_or_else(|| anyhow!("no workspace — pass --workspace <UUID> or run `bind` first"))?;
    download_workspace(cfg, profile, &ws_id, dir).await
}

async fn cmd_push(
    cfg: &Config,
    profile: &config::Profile,
    files: Vec<String>,
    workspace: Option<String>,
    dir: Option<String>,
) -> Result<()> {
    require_complete(profile)?;
    if files.is_empty() {
        return Err(anyhow!("nothing to upload — usage: strobes push <file…> [--workspace <id>] [--dir <dest>]"));
    }
    let ws = workspace
        .or_else(|| profile.workspace_id.clone())
        .ok_or_else(|| anyhow!("no workspace — pass --workspace <UUID> or run `bind` first"))?;
    let client = api::ApiClient::new(profile.clone())?;
    let prefix = dir
        .map(|d| format!("{}/", d.trim_matches('/')))
        .filter(|d| d.len() > 1)
        .unwrap_or_default();
    let sync_roots = workspace_sync_roots(cfg, &ws);
    for f in &files {
        let dest = upload_one(&client, &ws, f, &prefix, &sync_roots).await?;
        println!("✔ {f} → {dest}");
    }
    for root in &sync_roots {
        println!("↕ mirrored into {}", root.display());
    }
    println!("done — {} file(s) → workspace {}", files.len(), &ws[..8.min(ws.len())]);
    Ok(())
}

/// Read a local file and upload it to `ws` under `prefix`. Returns the dest path.
///
/// After the remote upload, the file is mirrored into each of `sync_roots`
/// (the active workspace sandbox and/or a bound local folder) at `dest`, so the
/// local copy the agent sees stays in sync — unless the source already *is*
/// that file.
async fn upload_one(
    client: &api::ApiClient,
    ws: &str,
    local: &str,
    prefix: &str,
    sync_roots: &[std::path::PathBuf],
) -> Result<String> {
    let path = std::path::Path::new(local);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("bad file name: {local}"))?;
    let bytes = std::fs::read(path).map_err(|e| anyhow!("read {local}: {e}"))?;
    let dest = format!("{prefix}{name}");
    client.upload_workspace_file(ws, &dest, bytes.clone()).await?;
    for root in sync_roots {
        mirror_into_folder(root, &dest, path, &bytes);
    }
    Ok(dest)
}

/// Local folders that mirror a workspace: the per-workspace sandbox the chat
/// agent reads from (`config_dir()/workspaces/<ws>`) plus any explicitly bound
/// folder. Only dirs that already exist are returned, so a stray upload never
/// pre-creates the sandbox and tricks `spawn_workspace_sync` into skipping the
/// initial download. Deduplicated.
fn workspace_sync_roots(cfg: &Config, ws: &str) -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    let sandbox = config::config_dir().join("workspaces").join(ws);
    if sandbox.is_dir() {
        roots.push(sandbox);
    }
    if let Some(d) = cfg.workspace_dirs.get(ws) {
        let p = std::path::PathBuf::from(d);
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    roots
}

/// Write `bytes` into `root/dest`, creating parent dirs. No-op if the source
/// file already resolves to that destination (avoids copying onto itself).
fn mirror_into_folder(root: &std::path::Path, dest: &str, src: &std::path::Path, bytes: &[u8]) {
    let target = root.join(dest);
    if let (Ok(a), Ok(b)) = (src.canonicalize(), target.canonicalize()) {
        if a == b {
            return; // already in place
        }
    }
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&target, bytes);
}

/// Download a workspace zip and extract it to a local folder, recording the
/// folder binding in config.
/// Download the workspace zip and extract it into `target`. Returns file count.
async fn extract_workspace_zip(
    client: &api::ApiClient,
    ws_id: &str,
    target: &std::path::Path,
) -> Result<usize> {
    std::fs::create_dir_all(target)?;
    let url = client.workspace_download_url(ws_id).await?;
    let bytes = reqwest::Client::new().get(&url).send().await?.bytes().await?;
    let reader = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)?;
    let mut count = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let out = match entry.enclosed_name() {
            Some(p) => target.join(p),
            None => continue,
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut f)?;
            count += 1;
        }
    }
    Ok(count)
}

async fn download_workspace(
    cfg: &mut Config,
    profile: &config::Profile,
    ws_id: &str,
    dir: Option<String>,
) -> Result<()> {
    let client = api::ApiClient::new(profile.clone())?;
    let target = dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config::config_dir().join("workspaces").join(ws_id));
    println!("• downloading workspace {ws_id} → {}", target.display());
    let count = extract_workspace_zip(&client, ws_id, &target).await?;
    cfg.workspace_dirs.insert(ws_id.to_string(), target.to_string_lossy().to_string());
    cfg.save()?;
    println!("✔ {count} files extracted to {} (folder bound to workspace)", target.display());
    Ok(())
}

/// Sync the bound workspace's files into a local folder and point the agent's
/// local sandbox there, so its (locally-proxied) workspace_get_meta /
/// execute_command see the real workspace files. Mirrors the cloud's S3→sandbox
/// sync. Re-downloads only if the folder is missing/empty.
/// Extract zip bytes into `target`, returning the file count (blocking).
fn extract_zip_bytes(bytes: Vec<u8>, target: &std::path::Path) -> Result<usize> {
    std::fs::create_dir_all(target)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut count = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let out = match entry.enclosed_name() {
            Some(p) => target.join(p),
            None => continue,
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::io::copy(&mut entry, &mut std::fs::File::create(&out)?)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Point the agent's local sandbox at the workspace folder IMMEDIATELY (so the
/// next tool call uses it), then download + extract the files in the BACKGROUND
/// so the UI never freezes. Progress is reported as Notice events on `tx`.
/// Fetch a workspace's headline counts (credits, findings, files) concurrently.
/// Returned as `(credits, findings, files)`; failures degrade to 0.
async fn fetch_workspace_stats(client: &api::ApiClient, ws_id: &str) -> (f64, usize, usize) {
    let (credits, findings, files) = tokio::join!(
        client.get_credits(Some(ws_id), None),
        client.list_workspace_findings(ws_id),
        client.list_workspace_files(ws_id, true),
    );
    (
        credits.map(|c| c.credits).unwrap_or(0.0),
        findings.map(|f| f.len()).unwrap_or(0),
        files.map(|f| f.iter().filter(|x| !x.is_folder).count()).unwrap_or(0),
    )
}

fn spawn_workspace_sync(
    profile: config::Profile,
    ws_id: String,
    tx: mpsc::UnboundedSender<pulse::AppEvent>,
) {
    let dir = config::config_dir().join("workspaces").join(&ws_id);
    // Set the sandbox path right away (instant, non-blocking).
    std::env::set_var("STROBES_AI_SANDBOX", &dir);
    std::env::set_var("STROBES_AI_WORKSPACE_ID", &ws_id);

    let already = dir.is_dir()
        && std::fs::read_dir(&dir).map(|mut d| d.next().is_some()).unwrap_or(false);
    if already {
        let _ = tx.send(pulse::AppEvent::Notice(format!(
            "workspace files at {} (cached)", dir.display()
        )));
        return;
    }

    tokio::spawn(async move {
        let _ = tx.send(pulse::AppEvent::Notice("syncing workspace files locally…".into()));
        let client = match api::ApiClient::new(profile) {
            Ok(c) => c,
            Err(e) => { let _ = tx.send(pulse::AppEvent::Error(e.to_string())); return; }
        };
        match client.download_workspace_bytes(&ws_id).await {
            Ok(bytes) => {
                let d = dir.clone();
                let res = tokio::task::spawn_blocking(move || extract_zip_bytes(bytes, &d)).await;
                match res {
                    Ok(Ok(n)) => {
                        let _ = tx.send(pulse::AppEvent::Notice(format!(
                            "✔ synced {n} workspace files → {}", dir.display()
                        )));
                    }
                    Ok(Err(e)) => { let _ = tx.send(pulse::AppEvent::Error(format!("workspace extract failed: {e}"))); }
                    Err(e) => { let _ = tx.send(pulse::AppEvent::Error(format!("extract task failed: {e}"))); }
                }
            }
            Err(e) => {
                // An empty workspace (nothing to zip) is benign — show a notice,
                // not a red error. Detect the backend's "no files" case / 404.
                let msg = e.to_string();
                if msg.contains("No files found to download") || msg.contains("HTTP 404") {
                    let _ = tx.send(pulse::AppEvent::Notice(
                        "workspace has no files yet — nothing to sync".into(),
                    ));
                } else {
                    let _ = tx.send(pulse::AppEvent::Error(format!("workspace download failed: {e}")));
                }
            }
        }
    });
}

async fn cmd_probe(p: &config::Profile, thread_id: &str, send: Option<String>, secs: u64, model: Option<i64>) -> Result<()> {
    require_complete(p)?;
    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let handle = pulse::connect(p, thread_id, tx, model).await?;
    println!("[probe] connected to {}", p.pulse_ws_url(thread_id)?.split('?').next().unwrap_or(""));
    if let Some(text) = send {
        handle.send_user_message(&text);
        println!("[probe] sent: {text}");
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => { println!("[probe] timeout"); break; }
            maybe = rx.recv() => match maybe {
                None => break,
                Some(ev) => {
                    println!("[probe] {}", describe(&ev));
                    match ev {
                        pulse::AppEvent::RunFinished(_) => break,
                        pulse::AppEvent::Interrupt { id, fields, .. } => {
                            // Auto-answer every field with a canned value.
                            let mut resp = serde_json::Map::new();
                            for f in &fields {
                                resp.insert(f.key.clone(), serde_json::Value::from("STROBES_TEST_7788"));
                            }
                            handle.respond_interrupt(&id, serde_json::Value::Object(resp));
                            println!("[probe] auto-answered interrupt {id}");
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

/// Create a new thread, send one message, stream the response to stdout.
///
/// Tokens are printed inline (no trailing newline until a natural one arrives),
/// tool events are prefixed with ▶ / ◀ / ✗, and the process exits cleanly on
/// `RunFinished`. `Interrupt` events prompt the user interactively.
async fn cmd_send(
    p: &config::Profile,
    message: String,
    workspace: Option<String>,
    new_workspace: Option<String>,
    title: Option<String>,
    model: Option<i64>,
) -> Result<()> {
    require_complete(p)?;
    let client = api::ApiClient::new(p.clone())?;

    // Resolve workspace: prefer explicit --workspace, then create one if --new-workspace given.
    let workspace_id: Option<String> = match (workspace, new_workspace) {
        (Some(ws), _) => Some(ws),
        (None, Some(name)) => {
            let (id, _) = client.create_workspace(&name).await?;
            eprintln!("workspace: {id}");
            Some(id)
        }
        (None, None) => None,
    };

    let thread_title = title.unwrap_or_else(|| {
        let truncated: String = message.chars().take(60).collect();
        if message.chars().count() > 60 {
            format!("{truncated}…")
        } else {
            truncated
        }
    });

    let thread_id = client
        .create_thread(&thread_title, workspace_id.as_deref())
        .await?;
    eprintln!("thread: {thread_id}");

    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let handle = pulse::connect(p, &thread_id, tx, model).await?;
    handle.send_user_message(&message);

    let mut needs_newline = false;
    loop {
        match rx.recv().await {
            None => {
                if needs_newline {
                    println!();
                }
                break;
            }
            Some(ev) => match ev {
                pulse::AppEvent::RunFinished(_) => {
                    if needs_newline {
                        println!();
                    }
                    break;
                }
                pulse::AppEvent::Stream(item) => match item.kind.as_str() {
                    "token" => {
                        if let Some(text) = &item.text {
                            print!("{text}");
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                            needs_newline = !text.ends_with('\n');
                        }
                    }
                    "thinking" => {
                        if let Some(text) = &item.text {
                            if needs_newline {
                                println!();
                                needs_newline = false;
                            }
                            println!("💭 {text}");
                        }
                    }
                    "tool_start" => {
                        if needs_newline {
                            println!();
                            needs_newline = false;
                        }
                        let name = item.tool_name.as_deref().unwrap_or("?");
                        let detail = item.detail.as_deref().unwrap_or("");
                        if detail.is_empty() {
                            println!("▶ {name}");
                        } else {
                            println!("▶ {name}({detail})");
                        }
                    }
                    "tool_output" => {
                        if needs_newline {
                            println!();
                            needs_newline = false;
                        }
                        let name = item.tool_name.as_deref().unwrap_or("?");
                        let detail = item.detail.as_deref().unwrap_or("");
                        if !detail.is_empty() {
                            println!("◀ {name}: {detail}");
                        }
                    }
                    "tool_failed" => {
                        if needs_newline {
                            println!();
                            needs_newline = false;
                        }
                        let name = item.tool_name.as_deref().unwrap_or("?");
                        let err = item.detail.as_deref().unwrap_or("error");
                        println!("✗ {name}: {err}");
                    }
                    "note" | "system" => {
                        if needs_newline {
                            println!();
                            needs_newline = false;
                        }
                        if let Some(text) = &item.text {
                            println!("ℹ {text}");
                        }
                    }
                    _ => {
                        if let Some(text) = &item.text {
                            print!("{text}");
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                            needs_newline = !text.ends_with('\n');
                        }
                    }
                },
                pulse::AppEvent::Error(e) => {
                    if needs_newline {
                        println!();
                        needs_newline = false;
                    }
                    eprintln!("error: {e}");
                    break;
                }
                pulse::AppEvent::Interrupt {
                    id,
                    title,
                    message: msg,
                    fields,
                } => {
                    if needs_newline {
                        println!();
                        needs_newline = false;
                    }
                    println!("\n[interrupt] {title}");
                    if !msg.is_empty() {
                        println!("{msg}");
                    }
                    let mut resp = serde_json::Map::new();
                    for f in &fields {
                        let secret = f.ftype == "password";
                        let val = prompt_line(&f.label, "", secret)?;
                        resp.insert(f.key.clone(), serde_json::Value::from(val));
                    }
                    handle.respond_interrupt(&id, serde_json::Value::Object(resp));
                }
                _ => {}
            },
        }
    }
    Ok(())
}

fn describe(ev: &pulse::AppEvent) -> String {
    use pulse::AppEvent::*;
    match ev {
        Connected => "connected".into(),
        Disconnected(w) => format!("disconnected: {w}"),
        RunStarted => "run started".into(),
        RunFinished(l) => format!("run finished: {l}"),
        Notice(n) => format!("notice: {n}"),
        Credits { credits, tokens, final_run } => format!("credits {credits:.4} tokens {tokens} final={final_run}"),
        Error(e) => format!("error: {e}"),
        LocalToolDone { name, ms, exit, err } => {
            format!("local tool {name} done {ms}ms exit={exit:?} err={err:?}")
        }
        Interrupt { id, title, fields, .. } => {
            format!("interrupt requested id={id} title={title:?} fields={}", fields.len())
        }
        Stream(it) => {
            let mut s = format!("event[{}]", it.kind);
            if let Some(n) = &it.tool_name { s.push_str(&format!(" tool={n}")); }
            if it.local { s.push_str(" LOCAL"); }
            if let Some(t) = &it.text { s.push_str(&format!(" text={:?}", trunc(t, 80))); }
            if let Some(d) = &it.detail { s.push_str(&format!(" detail={:?}", trunc(d, 80))); }
            s
        }
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() }
}

/// Read one line from stdin, showing the current value (redacted if secret) as
/// the default. An empty entry keeps the current value.
fn prompt_line(label: &str, current: &str, secret: bool) -> Result<String> {
    use std::io::Write;
    let hint = if current.is_empty() {
        String::new()
    } else if secret {
        format!(" [{}]", config::redact(current))
    } else {
        format!(" [{current}]")
    };
    print!("{label}{hint}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim().to_string();
    Ok(if line.is_empty() { current.to_string() } else { line })
}

/// Configure the active profile's credentials. Any field passed as a flag is
/// used verbatim; the rest are prompted for interactively.
async fn cmd_login(
    cfg: &mut Config,
    tenant: &str,
    base_url: Option<String>,
    org_id: Option<String>,
    master_key: Option<String>,
    deployment: Option<String>,
    no_verify: bool,
) -> Result<()> {
    let pname = tenant.to_string();
    let had_default = cfg.has_default();
    let cur = cfg.profile_mut(&pname).clone();
    let interactive = base_url.is_none() && org_id.is_none() && master_key.is_none();
    if interactive {
        println!("Configuring tenant '{pname}' — press Enter to keep the current value.");
    }

    let base_url = match base_url {
        Some(v) => v,
        None => prompt_line("Base URL (e.g. https://app.strobes.co)", &cur.base_url, false)?,
    };
    let org_id = match org_id {
        Some(v) => v,
        None => prompt_line("Org ID (UUID)", &cur.org_id, false)?,
    };
    let master_key = match master_key {
        Some(v) => v,
        None => prompt_line("Master key", &cur.master_key, true)?,
    };
    let deployment = match deployment {
        Some(v) => v,
        None if interactive => {
            prompt_line("Deployment (blank = proxy /api/v1, 'direct' = /v1)", &cur.deployment, false)?
        }
        None => cur.deployment.clone(),
    };

    {
        let p = cfg.profile_mut(&pname);
        p.base_url = base_url.trim().trim_end_matches('/').to_string();
        p.org_id = org_id.trim().to_string();
        p.master_key = master_key.trim().to_string();
        p.deployment = deployment.trim().to_string();
    }
    cfg.save()?;
    let path = config::config_dir().join("config.json");
    println!("\n✔ saved tenant '{pname}' → {}", path.display());

    let saved = cfg.profile_mut(&pname).clone();
    // The first tenant with usable credentials becomes the default.
    if saved.is_complete() && !had_default {
        cfg.current_profile = pname.clone();
        cfg.save()?;
        println!("★ '{pname}' is now the default tenant.");
    } else if saved.is_complete() && cfg.current_profile != pname {
        println!(
            "• default tenant remains '{}' — run any command with `--tenant {pname}` to use this one.",
            cfg.current_profile
        );
    }
    if std::env::var("STROBES_AI_BASE_URL").is_ok()
        || std::env::var("STROBES_AI_MASTER_KEY").is_ok()
        || std::env::var("STROBES_AI_ORG_ID").is_ok()
    {
        println!("⚠ STROBES_AI_* env vars are set and will override this file at runtime.");
    }
    if no_verify {
        return Ok(());
    }
    if !saved.is_complete() {
        println!("⚠ profile still incomplete (need base_url, org_id and master_key).");
        return Ok(());
    }
    use std::io::Write;
    print!("Testing connection… ");
    std::io::stdout().flush()?;
    match api::ApiClient::new(saved)?.ping().await {
        Ok(_) => println!("✔ connection OK"),
        Err(e) => println!("✗ connection failed: {e}"),
    }
    Ok(())
}

/// List configured tenants, marking the default with ★.
fn cmd_tenants(cfg: &Config) -> Result<()> {
    let names = cfg.tenants();
    if names.is_empty() {
        println!("no tenants configured — run `strobes --tenant <name> login`");
        return Ok(());
    }
    for name in &names {
        let p = cfg.profile_for(name);
        let mark = if *name == cfg.current_profile { "★" } else { " " };
        println!("{mark} {name}\t{}\t{}", p.org_id, p.base_url);
    }
    println!("\n★ = default tenant. Override per-run with `--tenant <name>`.");
    Ok(())
}

async fn cmd_status(p: &config::Profile) -> Result<()> {
    println!("base_url    {}", if p.base_url.is_empty() { "(unset)" } else { &p.base_url });
    println!("org_id      {}", if p.org_id.is_empty() { "(unset)" } else { &p.org_id });
    println!("master_key  {}", config::redact(&p.master_key));
    println!("deployment  {}", p.deployment);
    if let (Some(ip), Some(host)) = (p.resolve_override(), p.host()) {
        println!("resolve     {host} → {ip} (DNS bypass)");
    }
    println!("workspace   {}", p.workspace_id.clone().unwrap_or_else(|| "(none)".into()));
    println!("thread      {}", p.thread_id.clone().unwrap_or_else(|| "(none)".into()));
    if p.is_complete() {
        let client = api::ApiClient::new(p.clone())?;
        let (ping, latest) = tokio::join!(client.ping(), latest_release_version());
        match ping {
            Ok(_) => println!("\n✔ connection OK"),
            Err(e) => println!("\n✗ connection failed: {e}"),
        }
        match latest {
            Some(v) if version_is_newer(&v, env!("CARGO_PKG_VERSION")) => {
                println!("⬆ update available: v{v} (current v{}) — run `strobes update`", env!("CARGO_PKG_VERSION"));
            }
            _ => println!("✔ up to date (v{})", env!("CARGO_PKG_VERSION")),
        }
    }
    Ok(())
}

/// Compact token count (e.g. 1.8k, 2.5M).
fn fmt_tok(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Map of workspace id → lifetime credits consumed (one credits API call).
async fn workspace_credits_map(client: &api::ApiClient) -> std::collections::HashMap<String, f64> {
    client
        .get_credits(None, None)
        .await
        .map(|s| s.by_workspace.into_iter().map(|w| (w.workspace_id, w.credits)).collect())
        .unwrap_or_default()
}

async fn cmd_credits(
    p: &config::Profile,
    workspace: Option<String>,
    thread: Option<String>,
) -> Result<()> {
    require_complete(p)?;
    let client = api::ApiClient::new(p.clone())?;
    let sum = client.get_credits(workspace.as_deref(), thread.as_deref()).await?;

    let scope = match (&workspace, &thread) {
        (_, Some(t)) => format!("thread {}", &t[..8.min(t.len())]),
        (Some(w), _) => format!("workspace {}", &w[..8.min(w.len())]),
        _ => "organization".into(),
    };
    println!(
        "◈ {scope}:  {:.3} credits · {} tokens · {} runs",
        sum.credits,
        fmt_tok(sum.tokens),
        sum.runs
    );

    if !sum.by_workspace.is_empty() {
        let names: std::collections::HashMap<String, String> = client
            .list_workspaces()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|w| (w.id, w.name))
            .collect();
        println!("\nby workspace:");
        for w in &sum.by_workspace {
            let name = match names.get(&w.workspace_id) {
                Some(n) if !n.is_empty() => n.clone(),
                Some(_) => "(unnamed)".into(),
                None => "(deleted)".into(),
            };
            println!(
                "  {:>8.3} cr · {:>7} tok · {:>3} runs   {}",
                w.credits,
                fmt_tok(w.tokens),
                w.runs,
                name
            );
        }
    }
    Ok(())
}

async fn cmd_workspaces(p: &config::Profile) -> Result<()> {
    require_complete(p)?;
    let client = api::ApiClient::new(p.clone())?;
    let rows = client.list_workspaces().await?;
    if rows.is_empty() {
        println!("(no workspaces)");
    }
    let (counts, credits) =
        tokio::join!(workspace_thread_counts(&client, &rows), workspace_credits_map(&client));
    for (i, w) in rows.iter().enumerate() {
        let bound = if Some(&w.id) == p.workspace_id.as_ref() { " ●" } else { "" };
        let tcount = match counts.get(i).copied().flatten() {
            Some(n) => format!("{n} threads"),
            None => "? threads".into(),
        };
        let cr = credits.get(&w.id).copied().unwrap_or(0.0);
        println!("{}  {}{}  [{}]  {}  · ◈ {cr:.2} cr", w.id, w.name, bound, w.status, tcount);
    }
    Ok(())
}

async fn cmd_threads(p: &config::Profile) -> Result<()> {
    require_complete(p)?;
    let rows = api::ApiClient::new(p.clone())?.list_threads(None).await?;
    if rows.is_empty() {
        println!("(no threads)");
    }
    for t in rows {
        let title = if t.title.is_empty() { "(untitled)" } else { &t.title };
        println!("{}  {}  [{}]  {}", t.id, title, t.status, t.last_message);
    }
    Ok(())
}

fn require_complete(p: &config::Profile) -> Result<()> {
    if !p.is_complete() {
        return Err(anyhow!("profile incomplete — run `strobes login` first"));
    }
    Ok(())
}

/// What a keypress deferred to after the select block (these need `.await`
/// and/or to reassign `rx`, which can't happen while select borrows it).
enum Defer {
    Workspaces,
    Threads,
    Findings,
    Approvals,
    Files,
    OpenFiles,
    UploadFiles(Vec<std::path::PathBuf>),
    SwitchThread(String),
    BindWorkspace(String),
    Models,
}

/// If the input is one or more existing local file paths (e.g. a file dragged
/// onto the terminal, which inserts its path), return them — so a plain run can
/// upload instead of sending the path as a chat message. Returns None for
/// normal text. Handles `~`, quotes and backslash-escaped spaces.
fn parse_dragged_paths(input: &str) -> Option<Vec<std::path::PathBuf>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Tokenize respecting quotes and backslash escapes (how terminals quote
    // dropped paths with spaces).
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let (mut sq, mut dq) = (false, false);
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if !sq => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' if !dq => sq = !sq,
            '"' if !sq => dq = !dq,
            c if c.is_whitespace() && !sq && !dq => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    if tokens.is_empty() {
        return None;
    }
    let mut paths = Vec::new();
    for t in tokens {
        // Only treat path-like tokens (with a separator or ~) as drops, so a
        // normal word/bare filename isn't mistaken for an upload.
        let pathy = t.contains('/') || t.contains('\\') || t.starts_with('~');
        if !pathy {
            return None;
        }
        let p = if let Some(rest) = t.strip_prefix("~/") {
            dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| std::path::PathBuf::from(&t))
        } else {
            std::path::PathBuf::from(&t)
        };
        if !p.is_file() {
            return None;
        }
        paths.push(p);
    }
    Some(paths)
}

/// Load a thread's history, active-run state and title into the app, running
/// the independent round-trips concurrently. Used when switching threads.
async fn load_thread_data(
    client: &api::ApiClient,
    thread_id: &str,
    ws_scope: Option<&str>,
    app: &mut App,
) {
    let (events_res, threads_res, run_res, credits_res) = tokio::join!(
        client.get_thread_events(thread_id, 0, 2000),
        client.list_threads(ws_scope),
        client.get_thread_history(thread_id, 1),
        client.get_credits(None, Some(thread_id)),
    );
    if let Ok(c) = credits_res {
        app.set_thread_credits(c.credits, c.tokens);
    }
    match events_res {
        Ok(events) if !events.is_empty() => app.seed_history_events(events),
        _ => {
            if let Ok(hist) = client.get_thread_history(thread_id, 100).await {
                app.seed_history(hist.messages);
            }
        }
    }
    if let Ok(hist) = run_res {
        if let Some(run) = hist.active_run {
            app.note_active_run(&run.status);
        }
    }
    if let Ok(threads) = threads_res {
        if let Some(t) = threads.into_iter().find(|t| t.id == thread_id) {
            if !t.title.is_empty() {
                app.set_title(t.title);
            }
        }
    }
}

pub async fn run_chat(
    terminal: &mut ratatui::DefaultTerminal,
    tenant: &str,
    profile: config::Profile,
    thread_id: String,
    model: Option<i64>,
    initial_msg: Option<String>,
) -> Result<()> {
    require_complete(&profile)?;

    let mut profile = profile;
    let client = api::ApiClient::new(profile.clone())?;
    let mut app = App::new(thread_id.clone(), profile.base_url.clone(), tenant.to_string(), profile.org_id.clone());
    app.has_workspace = profile.workspace_id.is_some();
    if let Some(m) = model {
        app.set_model(Some(m));
    }

    // Fire the independent startup round-trips (history, title, slash-commands,
    // active-run, and the WebSocket connect) concurrently so the UI appears in
    // ~one round-trip instead of four-plus sequential ones.
    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let mut app_tx = tx.clone(); // for background tasks (workspace sync) to post UI events
    let ws_scope = profile.workspace_id.clone();
    let (conn, events_res, threads_res, cmds_res, run_res, ws_res, credits_res) = tokio::join!(
        pulse::connect(&profile, &thread_id, tx, model),
        client.get_thread_events(&thread_id, 0, 2000),
        client.list_threads(ws_scope.as_deref()),
        client.list_slash_commands(),
        client.get_thread_history(&thread_id, 1),
        client.list_workspaces(),
        client.get_credits(None, Some(&thread_id)),
    );
    let mut handle = conn?;
    // Resolve the bound workspace's name for the top-right indicator.
    if let (Some(wid), Ok(wss)) = (ws_scope.as_deref(), &ws_res) {
        if let Some(w) = wss.iter().find(|w| w.id == wid) {
            app.set_workspace_name(w.name.clone());
        }
    }
    // Seed this thread's lifetime credit usage.
    if let Ok(c) = credits_res {
        app.set_thread_credits(c.credits, c.tokens);
    }

    // Apply the fetched data (in-memory, fast).
    match events_res {
        Ok(events) if !events.is_empty() => app.seed_history_events(events),
        _ => {
            if let Ok(hist) = client.get_thread_history(&thread_id, 100).await {
                app.seed_history(hist.messages);
            }
        }
    }
    if let Ok(hist) = run_res {
        if let Some(run) = hist.active_run {
            app.note_active_run(&run.status);
        }
    }
    if let Ok(threads) = threads_res {
        if let Some(t) = threads.into_iter().find(|t| t.id == thread_id) {
            if !t.title.is_empty() {
                app.set_title(t.title);
            }
        }
    }
    if let Ok(cmds) = cmds_res {
        app.set_slash_commands(cmds);
    }

    if let Some(ws) = profile.workspace_id.clone() {
        spawn_workspace_sync(profile.clone(), ws, app_tx.clone());
    }

    // Background: suggest an update if a newer release is published.
    {
        let tx = app_tx.clone();
        tokio::spawn(async move {
            if let Some(latest) = latest_release_version().await {
                if version_is_newer(&latest, env!("CARGO_PKG_VERSION")) {
                    let _ = tx.send(pulse::AppEvent::Notice(format!(
                        "⬆ update available: v{latest} (you have v{}) — run `strobes update`",
                        env!("CARGO_PKG_VERSION")
                    )));
                }
            }
        });
    }

    // Auto-send the setup prompt for a freshly-created workspace.
    if let Some(msg) = initial_msg {
        let msg = msg.trim().to_string();
        if !msg.is_empty() {
            app.echo_user(&msg);
            handle.send_user_message(&msg);
            app.running = true;
            app.status = "sending…".into();
        }
    }

    let mut events = EventStream::new();
    let mut viewport_h: u16 = 20;
    // Drives the working-spinner animation while a turn is running.
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(120));

    let res = loop {
        terminal.draw(|f| {
            viewport_h = f.area().height.saturating_sub(7).max(1);
            app.draw(f);
        })?;

        let mut defer: Option<Defer> = None;
        let mut quit = false;

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                        if app.overlay_active() {
                            // ----- overlay navigation -----
                            match k.code {
                                KeyCode::Esc => {
                                    // Back-stack: detail → list, chat → threads →
                                    // workspaces → exit. Findings/approvals just
                                    // close back to chat.
                                    if app.overlay_detail_open() {
                                        app.overlay_esc();
                                    } else if app.overlay_kind() == Some(app::OverlayKind::Threads) {
                                        defer = Some(Defer::Workspaces);
                                    } else if app.overlay_kind() == Some(app::OverlayKind::Workspaces) {
                                        quit = true;
                                    } else {
                                        app.overlay_esc();
                                    }
                                }
                                KeyCode::Up => app.overlay_move(true),
                                KeyCode::Down => app.overlay_move(false),
                                KeyCode::PageUp => app.overlay_page(true, viewport_h),
                                KeyCode::PageDown => app.overlay_page(false, viewport_h),
                                KeyCode::Enter => {
                                    match app.overlay_enter() {
                                        Some((app::OverlayKind::Workspaces, id)) => defer = Some(Defer::BindWorkspace(id)),
                                        Some((app::OverlayKind::Threads, id)) => defer = Some(Defer::SwitchThread(id)),
                                        Some((app::OverlayKind::Models, id)) => {
                                            let model_id = match id.parse::<i64>().ok() {
                                                Some(0) => None,
                                                n => n,
                                            };
                                            handle.set_model(model_id);
                                            app.set_model(model_id);
                                            let name = api::model_name(model_id);
                                            app.notice(&format!("⚙ model → {name}"));
                                            app.close_overlay();
                                        }
                                        _ => {}
                                    }
                                }
                                // ^C exits the app from any picker/overlay (cancels
                                // an in-flight run first if one is active).
                                KeyCode::Char('c') if ctrl => { if app.running { handle.cancel(); } else { quit = true; } }
                                // ^P toggles the model picker even while another overlay is shown.
                                KeyCode::Char('p') if ctrl => {
                                    if app.overlay_kind() == Some(app::OverlayKind::Models) {
                                        app.close_overlay();
                                    } else {
                                        defer = Some(Defer::Models);
                                    }
                                }
                                // Type-to-search in the workspace/thread/model pickers.
                                KeyCode::Backspace if app.overlay_searchable() => app.overlay_filter_pop(),
                                KeyCode::Char(c) if !ctrl && app.overlay_searchable() => app.overlay_filter_push(c),
                                // vim-style j/k nav only where there's no search.
                                KeyCode::Char('k') if !ctrl => app.overlay_move(true),
                                KeyCode::Char('j') if !ctrl => app.overlay_move(false),
                                _ => {}
                            }
                        } else if app.awaiting_input() {
                            match k.code {
                                KeyCode::Esc => quit = true,
                                KeyCode::Char(c) if !ctrl => app.input_insert_char(c),
                                KeyCode::Backspace => app.input_backspace(),
                                KeyCode::Left => app.input_left(),
                                KeyCode::Right => app.input_right(),
                                KeyCode::Home => app.input_home(),
                                KeyCode::End => app.input_end(),
                                KeyCode::Enter => {
                                    let raw = app.input.clone();
                                    app.input_clear();
                                    if let Some((id, data)) = app.submit_interrupt_value(raw.trim()) {
                                        handle.respond_interrupt(&id, data);
                                    }
                                }
                                _ => {}
                            }
                        } else if app.slash_open()
                            && matches!(k.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Enter)
                            && !ctrl
                        {
                            // ----- slash-command autocomplete popup -----
                            match k.code {
                                KeyCode::Up => app.slash_move(true),
                                KeyCode::Down => app.slash_move(false),
                                KeyCode::Tab | KeyCode::Enter => app.slash_complete(),
                                _ => {}
                            }
                        } else {
                            // ----- normal chat -----
                            match k.code {
                                // Esc: if scrolled up, snap back to the live
                                // bottom first; otherwise step back into
                                // navigation (threads → workspaces). ^C quits.
                                KeyCode::Esc => {
                                    if app.is_pinned() {
                                        app.jump_to_bottom();
                                    } else {
                                        defer = Some(Defer::Threads);
                                    }
                                }
                                KeyCode::Char('c') if ctrl => { if app.running { handle.cancel(); } else { quit = true; } }
                                KeyCode::Char('t') if ctrl => app.show_thinking = !app.show_thinking,
                                KeyCode::Char('r') if ctrl => app.markdown = !app.markdown,
                                KeyCode::Char('s') if ctrl => {
                                    app.select_mode = !app.select_mode;
                                    if app.select_mode { disable_mouse(); } else { enable_mouse(); }
                                }
                                KeyCode::Char('w') if ctrl => defer = Some(Defer::Workspaces),
                                KeyCode::Char('o') if ctrl => defer = Some(Defer::Threads),
                                KeyCode::Char('p') if ctrl => defer = Some(Defer::Models),
                                KeyCode::Char('f') if ctrl => defer = Some(Defer::Findings),
                                KeyCode::Char('a') if ctrl => defer = Some(Defer::Approvals),
                                KeyCode::Char('l') if ctrl => defer = Some(Defer::Files),
                                KeyCode::Char('e') if ctrl => defer = Some(Defer::OpenFiles),
                                KeyCode::Char('y') if ctrl => {
                                    let text = app.transcript_plaintext();
                                    match copy_to_clipboard(&text) {
                                        Ok(_) => app.notice("copied transcript to clipboard"),
                                        Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("copy failed: {e}"))),
                                    }
                                }
                                // Newline: Shift/Alt+Enter (needs kitty protocol)
                                // or Ctrl+J (works on every terminal). Plain
                                // Enter sends.
                                KeyCode::Char('j') if ctrl => app.input_newline(),
                                KeyCode::Enter if k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => {
                                    app.input_newline();
                                }
                                KeyCode::Enter => {
                                    let text = app.input.trim().to_string();
                                    if !text.is_empty() {
                                        // Dragged local file path(s) + a bound
                                        // workspace → upload instead of sending.
                                        match parse_dragged_paths(&text) {
                                            Some(paths) if app.has_workspace => {
                                                defer = Some(Defer::UploadFiles(paths));
                                                app.input_clear();
                                            }
                                            _ => {
                                                app.echo_user(&text);
                                                handle.send_user_message(&text);
                                                app.input_clear();
                                                app.running = true;
                                                app.status = "sending…".into();
                                            }
                                        }
                                    }
                                }
                                KeyCode::Backspace => app.input_backspace(),
                                KeyCode::Delete => app.input_delete(),
                                KeyCode::Left => app.input_left(),
                                KeyCode::Right => app.input_right(),
                                KeyCode::Home => app.input_home(),
                                KeyCode::End => app.input_end(),
                                KeyCode::PageUp => app.page(true, viewport_h),
                                KeyCode::PageDown => app.page(false, viewport_h),
                                // Up/Down move the cursor within a multiline
                                // message; at the top/bottom edge they scroll the
                                // transcript instead.
                                KeyCode::Up => { if !app.input_up() { app.scroll_line(true); } }
                                KeyCode::Down => { if !app.input_down() { app.scroll_line(false); } }
                                KeyCode::Char(c) if !ctrl => app.input_insert_char(c),
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Event::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => if app.overlay_active() { app.overlay_move(true) } else { app.scroll_lines(true, 3) },
                        MouseEventKind::ScrollDown => if app.overlay_active() { app.overlay_move(false) } else { app.scroll_lines(false, 3) },
                        _ => {}
                    },
                    // A transient parse error (e.g. a stray escape byte) must NOT
                    // kill the session — just ignore it. Only a closed stream quits.
                    Some(Err(_)) => {}
                    None => quit = true,
                    _ => {}
                }
            }
            maybe_app = rx.recv() => {
                match maybe_app {
                    Some(ev) => {
                        let finished = matches!(ev, pulse::AppEvent::RunFinished(_));
                        app.on_app_event(ev);
                        // Notify the (possibly away) user that the reply is ready.
                        if finished {
                            if let Some(secs) = app.last_run_secs() {
                                notify_response_done(secs);
                            }
                        }
                    }
                    None => quit = true,
                }
            }
            _ = ticker.tick() => {
                // Advance the spinner only while running; ratatui's diff means
                // an idle redraw writes nothing.
                if app.running {
                    app.tick();
                }
            }
        }

        if quit {
            break Ok(());
        }

        // Deferred (awaiting / reconnecting) actions happen here, after the
        // select block has released its borrow on `rx`.
        match defer {
            Some(Defer::Workspaces) => match workspace_overlay_items(&client, &profile).await {
                Ok(items) => app.open_overlay(app::OverlayKind::Workspaces, "Workspaces".into(), items),
                Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
            },
            Some(Defer::Threads) => {
                let in_ws = profile.workspace_id.is_some();
                match client.list_threads(profile.workspace_id.as_deref()).await {
                    Ok(ts) => {
                        let title = if in_ws { "Threads (this workspace)" } else { "Threads" };
                        app.open_overlay(app::OverlayKind::Threads, title.into(), thread_items(ts, &thread_id, in_ws));
                    }
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                }
            }
            Some(Defer::Findings) => match profile.workspace_id.clone() {
                Some(ws) => match client.list_workspace_findings(&ws).await {
                    Ok(fs) => app.open_overlay(app::OverlayKind::Findings, "Findings".into(), finding_items(fs)),
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
                None => match workspace_overlay_items(&client, &profile).await {
                    Ok(items) => {
                        app.notice("pick a workspace, then press ^F for its findings");
                        app.open_overlay(app::OverlayKind::Workspaces, "Workspaces".into(), items);
                    }
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
            },
            Some(Defer::Approvals) => match profile.workspace_id.clone() {
                Some(ws) => match client.list_workspace_approvals(&ws).await {
                    Ok(aps) => app.open_overlay(app::OverlayKind::Approvals, "Approvals".into(), approval_items(aps)),
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
                None => match workspace_overlay_items(&client, &profile).await {
                    Ok(items) => {
                        app.notice("pick a workspace, then press ^A for its approvals");
                        app.open_overlay(app::OverlayKind::Workspaces, "Workspaces".into(), items);
                    }
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
            },
            Some(Defer::Files) => {
                let dir = local::sandbox_dir();
                let items = file_items(&dir);
                if items.is_empty() {
                    app.notice(&format!(
                        "no local workspace files yet ({}) — bind a workspace to sync",
                        dir.display()
                    ));
                } else {
                    let title = format!("Files — {} ({} items)", dir.display(), items.len());
                    app.open_overlay(app::OverlayKind::Files, title, items);
                }
            }
            Some(Defer::Models) => {
                let models: Vec<(i64, String)> = api::BUILTIN_MODELS
                    .iter()
                    .map(|&(id, name)| (id, name.to_string()))
                    .collect();
                let current = app.current_model();
                let items: Vec<app::OverlayItem> = models
                    .into_iter()
                    .map(|(id, name)| {
                        let mark = if Some(id) == current || (current.is_none() && id == 0) {
                            " ●"
                        } else {
                            ""
                        };
                        let label = format!("{name}{mark}");
                        let detail = vec![
                            name.clone(),
                            format!("model id: {id}"),
                            String::new(),
                            "Press Enter to use this model for the current chat.".into(),
                            "Takes effect on the next message you send.".into(),
                        ];
                        app::OverlayItem { label, detail, action: Some(id.to_string()) }
                    })
                    .collect();
                app.open_overlay(app::OverlayKind::Models, "Select AI model  (^P toggle)".into(), items);
            }
            Some(Defer::OpenFiles) => {
                let dir = local::sandbox_dir();
                match open_in_file_manager(&dir) {
                    Ok(_) => app.notice(&format!("opened {} in your file manager", dir.display())),
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("open failed: {e}"))),
                }
            }
            Some(Defer::UploadFiles(paths)) => match profile.workspace_id.clone() {
                Some(ws) => {
                    // Mirror dropped files into the live workspace sandbox (what
                    // the agent reads) and any bound folder, so local + remote
                    // stay in sync.
                    let sync_roots = workspace_sync_roots(&Config::load(), &ws);
                    let mut ok = 0usize;
                    for path in &paths {
                        let p = path.to_string_lossy().to_string();
                        match upload_one(&client, &ws, &p, "", &sync_roots).await {
                            Ok(dest) => {
                                ok += 1;
                                app.notice(&format!("⬆ uploaded {dest}"));
                            }
                            Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("upload failed: {e}"))),
                        }
                    }
                    if ok > 0 {
                        let where_to = match sync_roots.first() {
                            Some(r) => format!(" (synced to {})", r.display()),
                            None => String::new(),
                        };
                        app.notice(&format!("✔ {ok} file(s) uploaded to the workspace{where_to}"));
                    }
                }
                None => app.notice("bind a workspace first, then drop files to upload"),
            },
            Some(Defer::BindWorkspace(sel)) if sel == "__new_ws__" => {
                // Create a workspace: ask for a name + optional setup prompt,
                // then jump into its setup chat (sending the prompt).
                let auth = auth_line(&profile, tenant);
                let name = match picker::prompt_text(terminal, "New workspace — name", "", &auth).await {
                    Ok(Some(n)) => {
                        let n = n.trim().to_string();
                        if n.is_empty() { "CLI Workspace".to_string() } else { n }
                    }
                    _ => {
                        app.close_overlay();
                        String::new()
                    }
                };
                if !name.is_empty() {
                    let prompt = picker::prompt_text(
                        terminal,
                        "Setup prompt — what should this workspace do? (optional)",
                        "",
                        &auth,
                    )
                    .await
                    .ok()
                    .flatten()
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty());

                    match client.create_workspace(&name).await {
                        Ok((new_id, setup)) => {
                            profile.workspace_id = Some(new_id.clone());
                            handle.set_workspace(Some(new_id.clone()));
                            app.has_workspace = true;
                            app.set_workspace_name(name.clone());
                            {
                                let mut c = Config::load();
                                c.record_workspace_open(&new_id);
                                let _ = c.save();
                            }
                            spawn_workspace_sync(profile.clone(), new_id.clone(), app_tx.clone());
                            app.notice(&format!("✔ created workspace {name} — opening setup chat…"));
                            match setup {
                                Some(setup_tid) => {
                                    // Switch the live connection to the setup thread.
                                    let (ntx, nrx) = mpsc::unbounded_channel::<pulse::AppEvent>();
                                    let nclone = ntx.clone();
                                    match pulse::connect(&profile, &setup_tid, ntx, model).await {
                                        Ok(nh) => {
                                            handle = nh;
                                            rx = nrx;
                                            app_tx = nclone;
                                            app.reset_for_thread(setup_tid.clone());
                                            load_thread_data(&client, &setup_tid, profile.workspace_id.as_deref(), &mut app).await;
                                            app.set_workspace_name(name.clone());
                                            if let Some(p) = &prompt {
                                                app.echo_user(p);
                                                handle.send_user_message(p);
                                                app.running = true;
                                                app.status = "sending…".into();
                                            }
                                        }
                                        Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("open setup chat failed: {e}"))),
                                    }
                                }
                                None => app.close_overlay(),
                            }
                        }
                        Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("create workspace failed: {e}"))),
                    }
                }
            }
            Some(Defer::BindWorkspace(id)) => {
                profile.workspace_id = Some(id.clone());
                handle.set_workspace(Some(id.clone()));
                app.has_workspace = true;
                // Count this open locally for "recent" ranking next time.
                {
                    let mut c = Config::load();
                    c.record_workspace_open(&id);
                    let _ = c.save();
                }
                if let Ok(wss) = client.list_workspaces().await {
                    if let Some(w) = wss.iter().find(|w| w.id == id) {
                        app.set_workspace_name(w.name.clone());
                    }
                }
                app.notice(&format!("✔ bound workspace {} — ^F findings · ^A approvals", &id[..8.min(id.len())]));
                // Sync the workspace files locally IN THE BACKGROUND so the
                // agent's local tools see the real files without freezing the UI.
                spawn_workspace_sync(profile.clone(), id.clone(), app_tx.clone());
                // Lead straight into choosing a new chat or an existing thread
                // for this workspace.
                match client.list_threads(Some(&id)).await {
                    Ok(ts) => app.open_overlay(
                        app::OverlayKind::Threads,
                        "New chat or existing thread".into(),
                        thread_items(ts, &thread_id, true),
                    ),
                    Err(_) => app.close_overlay(),
                }
            }
            Some(Defer::SwitchThread(sel)) => {
                // "__new__" means create a fresh thread (in the bound workspace).
                let target = if sel == "__new__" {
                    match client.create_thread("CLI session", profile.workspace_id.as_deref()).await {
                        Ok(id) => Some(id),
                        Err(e) => {
                            app.on_app_event(pulse::AppEvent::Error(format!("new chat failed: {e}")));
                            None
                        }
                    }
                } else {
                    Some(sel)
                };
                if let Some(new_thread) = target {
                    let (ntx, nrx) = mpsc::unbounded_channel::<pulse::AppEvent>();
                    let nclone = ntx.clone();
                    match pulse::connect(&profile, &new_thread, ntx, model).await {
                        Ok(nh) => {
                            handle = nh;
                            rx = nrx;
                            app_tx = nclone;
                            app.reset_for_thread(new_thread.clone());
                            load_thread_data(&client, &new_thread, profile.workspace_id.as_deref(), &mut app).await;
                        }
                        Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("switch failed: {e}"))),
                    }
                }
            }
            None => {}
        }
    };

    res
}

/// Build overlay items for the local workspace files under `dir` (recursive).
fn file_items(dir: &std::path::Path) -> Vec<app::OverlayItem> {
    let mut files: Vec<(String, u64)> = Vec::new();
    walk_files(dir, dir, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
        .into_iter()
        .map(|(rel, size)| {
            let label = format!("{rel}  · {}", human_size(size));
            let detail = vec![
                rel.clone(),
                format!("size: {}", human_size(size)),
                format!("path: {}", dir.join(&rel).display()),
                String::new(),
                "^E opens this folder in your file manager.".into(),
            ];
            app::OverlayItem { label, detail, action: None }
        })
        .collect()
}

/// Recursively collect (relative path, size) of files under `root` (capped).
fn walk_files(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, u64)>) {
    if out.len() >= 2000 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let ft = match entry.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };
        let path = entry.path();
        if ft.is_dir() {
            walk_files(root, &path, out);
        } else if ft.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((rel, size));
        }
        if out.len() >= 2000 {
            return;
        }
    }
}

fn human_size(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let f = n as f64;
    if f >= MB {
        format!("{:.1}M", f / MB)
    } else if f >= KB {
        format!("{:.1}K", f / KB)
    } else {
        format!("{n}B")
    }
}

/// Reveal `dir` in the OS file manager (Finder / Explorer / xdg-open).
fn open_in_file_manager(dir: &std::path::Path) -> Result<()> {
    if !dir.exists() {
        let _ = std::fs::create_dir_all(dir);
    }
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(dir)
        .spawn()
        .map_err(|e| anyhow!("could not launch {program}: {e}"))?;
    Ok(())
}

/// Copy text to the system clipboard (pbcopy / clip / wl-copy|xclip|xsel).
fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "windows") {
        &[("clip", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    };
    let mut last = anyhow!("no clipboard tool available");
    for (prog, args) in tools {
        match Command::new(prog)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(text.as_bytes()).ok();
                }
                let _ = child.wait();
                return Ok(());
            }
            Err(e) => last = anyhow!("{prog}: {e}"),
        }
    }
    Err(last)
}

/// Latest published release version (no leading `v`) from GitHub, or None.
async fn latest_release_version() -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("strobes-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let v: serde_json::Value = client
        .get("https://api.github.com/repos/strobes-co/strobes-agents-cli/releases/latest")
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .map(|s| s.trim_start_matches('v').to_string())
}

/// Release target triple for this platform (matches the published assets).
fn release_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// Self-update: download the latest release for this platform and replace the
/// running binary in place. Headless — no TUI.
async fn cmd_update(force: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("strobes v{current}");
    let latest = latest_release_version()
        .await
        .ok_or_else(|| anyhow!("could not reach GitHub to check the latest release"))?;
    if !force && !version_is_newer(&latest, current) {
        println!("✔ already up to date (v{current}).");
        return Ok(());
    }
    let target = release_target()
        .ok_or_else(|| anyhow!("no prebuilt release for {}/{} — build from source",
            std::env::consts::OS, std::env::consts::ARCH))?;
    let url = format!(
        "https://github.com/strobes-co/strobes-agents-cli/releases/latest/download/strobes-{target}.tar.gz"
    );
    println!("↓ downloading v{latest} ({target})…");
    let bytes = reqwest::Client::builder()
        .user_agent(concat!("strobes-cli/", env!("CARGO_PKG_VERSION")))
        .build()?
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    let tmp = std::env::temp_dir().join(format!("strobes-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let tgz = tmp.join("strobes.tar.gz");
    std::fs::write(&tgz, &bytes)?;
    // Extract via the system `tar` (present on macOS/Linux and Windows 10+).
    let ok = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&tmp)
        .status()
        .map_err(|e| anyhow!("tar not available ({e}); extract manually from {url}"))?
        .success();
    if !ok {
        return Err(anyhow!("failed to extract the release archive"));
    }
    let binname = if cfg!(windows) { "strobes.exe" } else { "strobes" };
    let newbin = tmp.join(format!("strobes-{target}")).join(binname);
    if !newbin.exists() {
        return Err(anyhow!("binary missing after extraction: {}", newbin.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&newbin, std::fs::Permissions::from_mode(0o755))?;
    }

    let cur_exe = std::env::current_exe()?;
    replace_running_exe(&newbin, &cur_exe)?;
    let _ = std::fs::remove_dir_all(&tmp);

    println!("✔ updated to v{latest} → {}", cur_exe.display());
    println!("  restart `strobes` to use it.");
    Ok(())
}

/// Replace the (possibly running) executable at `cur_exe` with `newbin`.
fn replace_running_exe(newbin: &std::path::Path, cur_exe: &std::path::Path) -> Result<()> {
    let dir = cur_exe.parent().ok_or_else(|| anyhow!("cannot resolve install dir"))?;
    if cfg!(windows) {
        // Windows can't overwrite a running .exe; move it aside first.
        let old = cur_exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(cur_exe, &old)
            .map_err(|e| anyhow!("cannot move current exe aside ({e})"))?;
        std::fs::copy(newbin, cur_exe).map_err(|e| anyhow!("cannot place new exe ({e})"))?;
        Ok(())
    } else {
        // Stage in the same dir, then atomic-rename over the target (works even
        // while the old binary is running).
        let staged = dir.join(".strobes-update.tmp");
        std::fs::copy(newbin, &staged).map_err(|e| {
            anyhow!("cannot write to {} ({e}). Re-run with sudo, or your install dir isn't writable.", dir.display())
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
        }
        std::fs::rename(&staged, cur_exe).map_err(|e| {
            let _ = std::fs::remove_file(&staged);
            anyhow!("cannot replace {} ({e}). The install dir likely needs sudo.", cur_exe.display())
        })?;
        Ok(())
    }
}

/// True if dotted-numeric `latest` is newer than `current`.
fn version_is_newer(latest: &str, current: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.split('.')
            .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0))
            .collect()
    }
    let (l, c) = (parts(latest), parts(current));
    for i in 0..l.len().max(c.len()) {
        let (lv, cv) = (l.get(i).copied().unwrap_or(0), c.get(i).copied().unwrap_or(0));
        if lv != cv {
            return lv > cv;
        }
    }
    false
}

fn finding_items(fs: Vec<api::Finding>) -> Vec<app::OverlayItem> {
    fs.into_iter()
        .map(|f| {
            let label = format!("[{}] {}  · {}", f.severity_label, trunc(&f.title, 80), f.state_label);
            let mut detail = vec![
                f.title.clone(),
                String::new(),
                format!("Severity: {}    State: {}    CVSS: {}",
                    f.severity_label, f.state_label,
                    f.cvss.map(|c| c.to_string()).unwrap_or_else(|| "-".into())),
            ];
            if let Some(a) = &f.asset {
                detail.push(format!("Asset: {a}"));
            }
            detail.push(format!("Finding ID: {}", f.id));
            if !f.description.trim().is_empty() {
                detail.push(String::new());
                detail.push("── Description ──".into());
                detail.extend(f.description.lines().map(|l| l.to_string()));
            }
            if !f.mitigation.trim().is_empty() {
                detail.push(String::new());
                detail.push("── Mitigation ──".into());
                detail.extend(f.mitigation.lines().map(|l| l.to_string()));
            }
            app::OverlayItem { label, detail, action: Some(f.id.to_string()) }
        })
        .collect()
}

fn approval_items(aps: Vec<api::Approval>) -> Vec<app::OverlayItem> {
    aps.into_iter()
        .map(|a| {
            let label = format!("[{}] {}  · {}", a.state, a.action_type, trunc(&a.summary, 70));
            let mut detail = vec![
                format!("Action: {}", a.action_type),
                format!("Module: {}    State: {}", a.module, a.state),
                String::new(),
                "── Summary ──".into(),
            ];
            detail.extend(a.summary.lines().map(|l| l.to_string()));
            let targets = serde_json::to_string(&a.target_ids).unwrap_or_default();
            if targets != "null" && !targets.is_empty() {
                detail.push(String::new());
                detail.push(format!("Targets: {targets}"));
            }
            app::OverlayItem { label, detail, action: Some(a.id) }
        })
        .collect()
}

fn thread_items(ts: Vec<api::Thread>, current: &str, in_workspace: bool) -> Vec<app::OverlayItem> {
    let new_label = if in_workspace {
        "➕  New chat (in this workspace)".to_string()
    } else {
        "➕  New chat".to_string()
    };
    let mut items = vec![app::OverlayItem {
        label: new_label,
        detail: vec!["Press Enter to start a new chat thread.".into()],
        action: Some("__new__".into()),
    }];
    items.extend(ts.into_iter().map(|t| {
        let here = if t.id == current { " ●" } else { "" };
        let title = if t.title.is_empty() { "(untitled)".into() } else { t.title.clone() };
        let label = format!("{title}{here}  · {}  {}", t.status, trunc(&t.last_message, 50));
        let detail = vec![title, format!("status: {}", t.status), format!("id: {}", t.id),
            String::new(), "Press Enter to switch to this thread.".into()];
        app::OverlayItem { label, detail, action: Some(t.id) }
    }));
    items
}

fn workspace_items(
    ws: Vec<api::Workspace>,
    counts: &[Option<usize>],
    credits: &std::collections::HashMap<String, f64>,
    profile: &config::Profile,
    cfg: &Config,
) -> Vec<app::OverlayItem> {
    let mut items = vec![app::OverlayItem {
        label: "➕  New workspace".to_string(),
        detail: vec![
            "Press Enter to create a new workspace.".into(),
            "The AI setup chat opens to configure & name it.".into(),
        ],
        action: Some("__new_ws__".into()),
    }];
    items.extend(ws.into_iter().enumerate().map(|(i, w)| {
        let bound = if Some(&w.id) == profile.workspace_id.as_ref() { " ●" } else { "" };
        let name = if w.name.is_empty() { "(unnamed)".into() } else { w.name.clone() };
        let tcount = match counts.get(i).copied().flatten() {
            Some(n) => format!("{n} threads"),
            None => "? threads".into(),
        };
        let cr = credits.get(&w.id).copied().unwrap_or(0.0);
        let opens = cfg.workspace_open_count(&w.id);
        let recent = if opens > 0 { format!(" · ↻ recent ×{opens}") } else { String::new() };
        let label = format!("{name}{bound}  · {} · {tcount} · ◈ {cr:.2} cr{recent}", w.status);
        let mut detail = vec![name, format!("status: {}", w.status), format!("threads: {tcount}"),
            format!("credits: {cr:.3}")];
        if opens > 0 {
            detail.push(format!("opened {opens}× from this machine"));
        }
        detail.push(format!("id: {}", w.id));
        detail.push(String::new());
        detail.push("Press Enter to bind this workspace (enables ^F / ^A).".into());
        app::OverlayItem { label, detail, action: Some(w.id) }
    }));
    items
}

/// Count threads per workspace concurrently (one `list_threads` call each).
async fn workspace_thread_counts(client: &api::ApiClient, ws: &[api::Workspace]) -> Vec<Option<usize>> {
    let futs: Vec<_> = ws.iter().map(|w| client.list_threads(Some(&w.id))).collect();
    futures_util::future::join_all(futs)
        .await
        .into_iter()
        .map(|r| r.ok().map(|t| t.len()))
        .collect()
}

/// List workspaces and build overlay items including per-workspace thread counts.
async fn workspace_overlay_items(
    client: &api::ApiClient,
    profile: &config::Profile,
) -> Result<Vec<app::OverlayItem>> {
    let cfg = Config::load();
    let mut ws = client.list_workspaces().await?;
    // Most-opened (then most-recent) first; never-opened keep server order.
    ws.sort_by(|a, b| {
        cfg.workspace_open_count(&b.id)
            .cmp(&cfg.workspace_open_count(&a.id))
            .then(cfg.workspace_last_opened(&b.id).cmp(&cfg.workspace_last_opened(&a.id)))
    });
    let (counts, credits) =
        tokio::join!(workspace_thread_counts(client, &ws), workspace_credits_map(client));
    Ok(workspace_items(ws, &counts, &credits, profile, &cfg))
}

async fn cmd_workflow(profile: config::Profile, sub: WorkflowCmd, tenant: &str) -> Result<()> {
    match sub {
        WorkflowCmd::Remote { sub } => return cmd_workflow_remote(profile, sub, tenant.to_string()).await,
        WorkflowCmd::Run { file, var, no_tui } => {
            require_complete(&profile)?;
            let def = workflow::load(&file)?;
            let abs_file = std::path::Path::new(&file)
                .canonicalize()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| file.clone());

            // Vars explicitly set via -v flags take priority.
            let cli_vars: std::collections::HashMap<String, String> = var
                .iter()
                .filter_map(|kv| {
                    let mut it = kv.splitn(2, '=');
                    Some((it.next()?.to_string(), it.next()?.to_string()))
                })
                .collect();

            // ── Interactive variable prompting ───────────────────────────────
            // Print a brief workflow summary, then prompt for every variable
            // that wasn't already supplied via -v, showing its YAML default.
            let total_tasks: usize = def.phases.iter().map(|p| p.tasks.len()).sum();
            println!(
                "\n  Workflow : {}\n  Phases   : {}  |  Tasks: {}",
                def.name,
                def.phases.len(),
                total_tasks,
            );
            if !def.description.is_empty() {
                println!("  {}", def.description);
            }

            let mut extra_vars = cli_vars;
            if !def.variables.is_empty() {
                let mut keys: Vec<&String> = def.variables.keys().collect();
                keys.sort();
                let all_provided = keys.iter().all(|k| extra_vars.contains_key(*k));
                if !all_provided {
                    println!("\n  Variables (Enter to keep default):");
                }
                for k in keys {
                    if extra_vars.contains_key(k) {
                        // Already set via -v; echo so the user sees the final value.
                        println!("  {k} = {}", extra_vars[k]);
                        continue;
                    }
                    let default = &def.variables[k];
                    let secret = ["key", "secret", "token", "password", "pass", "credential"]
                        .iter()
                        .any(|s| k.to_lowercase().contains(s));
                    let val = prompt_line(&format!("  {k}"), default, secret)?;
                    extra_vars.insert(k.clone(), val);
                }
            }
            println!();

            let (ev_tx, ev_rx) =
                mpsc::unbounded_channel::<workflow_runner::WfEvent>();

            if no_tui {
                // Headless: print events to stdout.
                let ev_tx2 = ev_tx.clone();
                let def2 = def.clone();
                let profile2 = profile.clone();
                let extra2 = extra_vars.clone();
                let abs_file2 = abs_file.clone();
                let runner = tokio::spawn(async move {
                    if let Err(e) =
                        workflow_runner::run(def2, profile2, ev_tx2.clone(), extra2, None, abs_file2).await
                    {
                        let _ = ev_tx2.send(workflow_runner::WfEvent::WorkflowFailed {
                            reason: e.to_string(),
                        });
                    }
                });
                let mut rx = ev_rx;
                let mut failed = false;
                while let Some(ev) = rx.recv().await {
                    use workflow_runner::WfEvent::*;
                    match &ev {
                        Log(m) => println!("{m}"),
                        WorkspaceReady { id, name } => println!("workspace: {name} [{id}]"),
                        SetupStarted { thread_id } => {
                            println!("▶ workspace-setup ({}…)", &thread_id[..8.min(thread_id.len())])
                        }
                        PhaseStarted { phase } => println!("▶ phase: {phase}"),
                        TaskStarted { task, thread_id, .. } => {
                            println!(
                                "▶ {task} ({}…)",
                                &thread_id[..8.min(thread_id.len())]
                            )
                        }
                        TaskOutput { task, text } => println!("[{task}] {text}"),
                        TaskDone { task } => println!("✔ {task}"),
                        TaskFailed { task, reason } => println!("✗ {task}: {reason}"),
                        TaskSkipped { task } => println!("↷ {task} (skipped)"),
                        WorkflowDone => println!("✔ workflow complete"),
                        WorkflowFailed { reason } => {
                            println!("✗ workflow failed: {reason}");
                            failed = true;
                        }
                    }
                }
                let _ = runner.await;
                if failed {
                    return Err(anyhow!("workflow failed"));
                }
            } else {
                // TUI mode — one terminal instance shared with any drill-down chat views.
                let ev_tx2 = ev_tx.clone();
                let def2 = def.clone();
                let profile2 = profile.clone();
                let extra2 = extra_vars.clone();
                let abs_file3 = abs_file.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        workflow_runner::run(def2, profile2, ev_tx2.clone(), extra2, None, abs_file3).await
                    {
                        let _ = ev_tx2.send(workflow_runner::WfEvent::WorkflowFailed {
                            reason: e.to_string(),
                        });
                    }
                });
                let mut terminal = ratatui::init();
                enable_mouse();
                let r = workflow_tui::run_tui(
                    &mut terminal,
                    def,
                    ev_rx,
                    profile.clone(),
                    tenant.to_string(),
                )
                .await;
                disable_mouse();
                ratatui::restore();
                r?;
            }
            Ok(())
        }
        WorkflowCmd::List => {
            let files = workflow::list_workflows(".");
            if files.is_empty() {
                println!(
                    "no workflow files (.yaml/.yml with 'phases:') found in current directory"
                );
            } else {
                for f in &files {
                    println!("{f}");
                }
                println!("\n{} file(s) found", files.len());
            }
            Ok(())
        }
        WorkflowCmd::Init { output } => {
            let tpl = workflow::starter_template();
            match output {
                Some(path) => {
                    std::fs::write(&path, tpl)?;
                    println!("✔ wrote {path}");
                    println!("edit the file, then run: strobes workflow run {path}");
                }
                None => print!("{tpl}"),
            }
            Ok(())
        }

        WorkflowCmd::History => {
            let runs = workflow_state::list_runs();
            if runs.is_empty() {
                println!("No workflow runs recorded yet.");
                println!("Runs are saved in: {}", workflow_state::runs_dir().display());
                return Ok(());
            }
            println!(
                "\n{:<38}  {:<26}  {:<10}  {:<6}  {}",
                "RUN ID", "WORKFLOW", "STATUS", "DONE", "STARTED"
            );
            println!("{}", "─".repeat(96));
            for r in &runs {
                let done = format!("{}/{}", r.done_count(), r.total_tasks());
                let name_trunc = if r.workflow_name.len() > 26 {
                    format!("{}…", &r.workflow_name[..25])
                } else {
                    r.workflow_name.clone()
                };
                let started = r
                    .started_at
                    .trim_end_matches('Z')
                    .replacen('T', " ", 1);
                let started = &started[..started.len().min(19)];
                println!(
                    "{:<38}  {:<26}  {:<10}  {:<6}  {}",
                    r.id, name_trunc, r.status.label(), done, started
                );
            }
            println!();
            Ok(())
        }

        WorkflowCmd::Resume { id, no_tui } => {
            require_complete(&profile)?;
            let resume_record = workflow_state::load(&id)?;

            // Validate we can still load the workflow file.
            let def = workflow::load(&resume_record.workflow_file).map_err(|e| {
                anyhow!(
                    "cannot reload workflow file '{}': {e}\n\
                     (if the file moved, update 'workflow_file' in {})",
                    resume_record.workflow_file,
                    workflow_state::runs_dir().join(format!("{id}.json")).display()
                )
            })?;

            let vars = resume_record.vars.clone();

            println!("\n  Resuming : {}", resume_record.workflow_name);
            println!("  Run ID   : {id}");
            println!(
                "  Progress : {}/{} tasks done",
                resume_record.done_count(),
                resume_record.total_tasks()
            );
            println!();

            let (ev_tx, ev_rx) = mpsc::unbounded_channel::<workflow_runner::WfEvent>();

            if no_tui {
                let ev_tx2 = ev_tx.clone();
                let def2 = def.clone();
                let profile2 = profile.clone();
                let resume2 = Some(resume_record);
                let wf_file = String::new(); // unused — taken from resume record
                let runner = tokio::spawn(async move {
                    if let Err(e) = workflow_runner::run(
                        def2, profile2, ev_tx2.clone(), vars, resume2, wf_file,
                    )
                    .await
                    {
                        let _ = ev_tx2.send(workflow_runner::WfEvent::WorkflowFailed {
                            reason: e.to_string(),
                        });
                    }
                });
                let mut rx = ev_rx;
                let mut failed = false;
                while let Some(ev) = rx.recv().await {
                    use workflow_runner::WfEvent::*;
                    match &ev {
                        Log(m) => println!("{m}"),
                        WorkspaceReady { id, name } => println!("workspace: {name} [{id}]"),
                        SetupStarted { thread_id } => println!(
                            "▶ workspace-setup ({}…)",
                            &thread_id[..8.min(thread_id.len())]
                        ),
                        PhaseStarted { phase } => println!("▶ phase: {phase}"),
                        TaskStarted { task, thread_id, .. } => println!(
                            "▶ {task} ({}…)",
                            &thread_id[..8.min(thread_id.len())]
                        ),
                        TaskOutput { task, text } => println!("[{task}] {text}"),
                        TaskDone { task } => println!("✔ {task}"),
                        TaskFailed { task, reason } => println!("✗ {task}: {reason}"),
                        TaskSkipped { task } => println!("↷ {task} (skipped)"),
                        WorkflowDone => println!("✔ workflow complete"),
                        WorkflowFailed { reason } => {
                            println!("✗ workflow failed: {reason}");
                            failed = true;
                        }
                    }
                }
                let _ = runner.await;
                if failed {
                    return Err(anyhow!("workflow failed"));
                }
            } else {
                let ev_tx2 = ev_tx.clone();
                let def2 = def.clone();
                let profile2 = profile.clone();
                let resume2 = Some(resume_record);
                let wf_file = String::new(); // unused — taken from resume record
                tokio::spawn(async move {
                    if let Err(e) = workflow_runner::run(
                        def2, profile2, ev_tx2.clone(), vars, resume2, wf_file,
                    )
                    .await
                    {
                        let _ = ev_tx2.send(workflow_runner::WfEvent::WorkflowFailed {
                            reason: e.to_string(),
                        });
                    }
                });
                let mut terminal = ratatui::init();
                enable_mouse();
                let r = workflow_tui::run_tui(
                    &mut terminal,
                    def,
                    ev_rx,
                    profile.clone(),
                    tenant.to_string(),
                )
                .await;
                disable_mouse();
                ratatui::restore();
                r?;
            }
            Ok(())
        }
    }
}

// ── Remote workflow management (GraphQL API) ─────────────────────────────────

fn resolve_workspace(w: Option<String>, profile: &config::Profile) -> Result<String> {
    w.or_else(|| profile.workspace_id.clone())
        .ok_or_else(|| anyhow!("no workspace — pass --workspace <UUID> or run `strobes bind` first"))
}

/// Like `resolve_workspace` but shows a workspace picker when no workspace is bound.
/// Returns `None` if the user cancels the picker.
async fn resolve_workspace_or_pick(
    workspace: Option<String>,
    profile: &config::Profile,
    client: &api::ApiClient,
) -> Result<Option<String>> {
    if let Some(w) = workspace.or_else(|| profile.workspace_id.clone()) {
        return Ok(Some(w));
    }
    let workspaces = client.list_workspaces().await?;
    if workspaces.is_empty() {
        return Err(anyhow!("no workspaces found — create one with `strobes bind`"));
    }
    let labels: Vec<String> = workspaces
        .iter()
        .map(|w| format!("{}…  {}", &w.id[..8.min(w.id.len())], w.name))
        .collect();
    match picker::select("Select workspace", &labels).await? {
        picker::Nav::Item(i) => Ok(Some(workspaces[i].id.clone())),
        _ => Ok(None),
    }
}

/// Convert a kebab-case / lower-case name to Title Case words.
fn title_case(s: &str) -> String {
    s.replace('-', " ")
        .replace('_', " ")
        .split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Serialise `Vec<PhaseDef>` from a workflow YAML into a GraphQL input literal
/// suitable for `createCustomWorkflow` / `editCustomWorkflow`.
fn phases_to_json(phases: &[workflow::PhaseDef]) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = phases
        .iter()
        .enumerate()
        .map(|(i, phase)| {
            let tasks: Vec<serde_json::Value> = phase
                .tasks
                .iter()
                .map(|task| {
                    serde_json::json!({
                        "key": task.name,
                        "title": title_case(&task.name),
                        "instructions": task.prompt,
                        "agentType": "general",
                        "taskType": "agent",
                    })
                })
                .collect();
            serde_json::json!({
                "key": phase.name,
                "name": title_case(&phase.name),
                "order": i,
                "gateType": "all_complete",
                "failurePolicy": "continue",
                "tasks": tasks,
            })
        })
        .collect();
    serde_json::Value::Array(arr)
}

async fn cmd_workflow_remote(
    profile: config::Profile,
    sub: RemoteWorkflowCmd,
    tenant: String,
) -> Result<()> {
    require_complete(&profile)?;
    let client = api::ApiClient::new(profile.clone())?;

    match sub {
        RemoteWorkflowCmd::Templates => {
            let templates = client.workflow_templates().await?;
            if templates.is_empty() {
                println!("(no templates available)");
                return Ok(());
            }
            for t in &templates {
                let req = if t.required_variables.is_empty() {
                    String::new()
                } else {
                    format!("  requires: {}", t.required_variables.join(", "))
                };
                println!(
                    "{} {}  [{}]  {} phases{}",
                    if t.icon.is_empty() { "•" } else { &t.icon },
                    t.slug,
                    t.name,
                    t.phase_count,
                    req
                );
                if !t.description.is_empty() {
                    println!("   {}", t.description);
                }
                for p in &t.phases {
                    println!("   {}: {} ({} tasks)", p.order, p.name, p.task_count);
                }
                println!();
            }
        }

        RemoteWorkflowCmd::Status { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            match client.workspace_workflow(&ws).await? {
                None => println!("(no workflow attached to workspace {}…)", &ws[..8.min(ws.len())]),
                Some(wf) => {
                    println!("workflow  {}", wf.workflow_id);
                    println!("status    {}", wf.status);
                    if let Some(slug) = &wf.template_slug {
                        println!("template  {slug}");
                    }
                    if let Some(v) = &wf.template_version {
                        println!("version   {v}");
                    }
                    if let Some(phase) = &wf.current_phase_key {
                        println!("phase     {phase}");
                    }
                    println!("tasks     {}/{}", wf.completed_tasks, wf.total_tasks);
                    if let Some(s) = &wf.started_at { println!("started   {s}"); }
                    if let Some(c) = &wf.completed_at { println!("finished  {c}"); }
                    if !wf.phases.is_empty() {
                        println!("\nphases:");
                        for p in &wf.phases {
                            let cur = if wf.current_phase_key.as_deref() == Some(&p.phase_key) {
                                " ←"
                            } else {
                                ""
                            };
                            println!("  {:>12}  {}  {}{cur}", p.status, p.phase_key, p.phase_name);
                        }
                    }
                }
            }
        }

        RemoteWorkflowCmd::Attach { workspace, template, var } => {
            // For attach, always show the workspace picker when --workspace is not
            // given explicitly — defaulting silently to the bound workspace would
            // attach to the wrong place without the user realising.
            let ws = match workspace {
                Some(w) => w,
                None => {
                    let workspaces = client.list_workspaces().await?;
                    if workspaces.is_empty() {
                        return Err(anyhow!("no workspaces found — create one with `strobes bind`"));
                    }
                    let labels: Vec<String> = workspaces.iter()
                        .map(|w| format!("{}…  {}", &w.id[..8.min(w.id.len())], w.name))
                        .collect();
                    match picker::select("Select workspace", &labels).await? {
                        picker::Nav::Item(i) => workspaces[i].id.clone(),
                        _ => return Ok(()),
                    }
                }
            };

            // Fetch templates once — needed for both the picker and required-var lookup.
            let templates = client.workflow_templates().await?;

            // Resolve template slug and its required variables.
            let (slug, required_vars) = match template {
                Some(s) => {
                    let req = templates.iter()
                        .find(|t| t.slug == s)
                        .map(|t| t.required_variables.clone())
                        .unwrap_or_default();
                    (s, req)
                }
                None => {
                    if templates.is_empty() {
                        return Err(anyhow!("no workflow templates available"));
                    }
                    let labels: Vec<String> = templates.iter().map(|t| {
                        let req_hint = if t.required_variables.is_empty() {
                            String::new()
                        } else {
                            format!("  [{}]", t.required_variables.join(", "))
                        };
                        format!(
                            "{} {}  — {}{}",
                            if t.icon.is_empty() { "•" } else { &t.icon },
                            t.slug,
                            t.name,
                            req_hint,
                        )
                    }).collect();
                    match picker::select("Select a workflow template", &labels).await? {
                        picker::Nav::Item(i) => {
                            let t = &templates[i];
                            (t.slug.clone(), t.required_variables.clone())
                        }
                        _ => return Ok(()),
                    }
                }
            };

            // Start with variables supplied via -v flags.
            let mut vars: std::collections::HashMap<String, String> = var.iter()
                .filter_map(|kv| {
                    let mut it = kv.splitn(2, '=');
                    Some((it.next()?.to_string(), it.next()?.to_string()))
                })
                .collect();

            // Prompt for any required variables not yet provided.
            let missing: Vec<&String> = required_vars.iter()
                .filter(|k| !vars.contains_key(k.as_str()))
                .collect();
            if !missing.is_empty() {
                println!("  This template requires the following variables:");
                use std::io::Write;
                for key in missing {
                    print!("  {key}: ");
                    std::io::stdout().flush()?;
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    let val = line.trim().to_string();
                    if val.is_empty() {
                        return Err(anyhow!("variable '{key}' is required and cannot be empty"));
                    }
                    vars.insert(key.clone(), val);
                }
            }

            let variables: serde_json::Value = vars.iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect::<serde_json::Map<_, _>>()
                .into();

            let wf = client.attach_workflow_template(&ws, &slug, &variables).await?;
            println!("✔ attached '{slug}' to workspace {}…", &ws[..8.min(ws.len())]);
            println!("  workflow {} [{}]", wf.workflow_id, wf.status);
        }

        RemoteWorkflowCmd::Detach { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            print!(
                "detach workflow from workspace {}…? [y/N] ",
                &ws[..8.min(ws.len())]
            );
            use std::io::Write;
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                println!("cancelled.");
                return Ok(());
            }
            client.detach_workflow(&ws).await?;
            println!("✔ workflow detached");
        }

        RemoteWorkflowCmd::Create { workspace, file, var } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            let def = workflow::load(&file)?;
            let phases_json = phases_to_json(&def.phases);
            let mut vars: serde_json::Map<String, serde_json::Value> = def
                .variables
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            for kv in &var {
                let mut it = kv.splitn(2, '=');
                if let (Some(k), Some(v)) = (it.next(), it.next()) {
                    vars.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                }
            }
            let vars_json = serde_json::Value::Object(vars);
            let wf = client
                .create_custom_workflow(&ws, &def.name, &phases_json, &vars_json)
                .await?;
            let total_tasks: usize = def.phases.iter().map(|p| p.tasks.len()).sum();
            println!("✔ workflow created: {} [{}]", wf.workflow_id, wf.status);
            println!(
                "  {} phases, {total_tasks} tasks from '{file}'",
                def.phases.len()
            );
        }

        RemoteWorkflowCmd::Edit { workspace, file } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            let def = workflow::load(&file)?;
            let phases_json = phases_to_json(&def.phases);
            let wf = client
                .edit_custom_workflow(&ws, &def.name, &phases_json)
                .await?;
            println!("✔ workflow updated: {} [{}]", wf.workflow_id, wf.status);
        }

        RemoteWorkflowCmd::Sync { workspace, file, var } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            let def = workflow::load(&file)?;
            let phases_json = phases_to_json(&def.phases);
            let mut vars: serde_json::Map<String, serde_json::Value> = def
                .variables
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            for kv in &var {
                let mut it = kv.splitn(2, '=');
                if let (Some(k), Some(v)) = (it.next(), it.next()) {
                    vars.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                }
            }
            let vars_json = serde_json::Value::Object(vars);
            match client.workspace_workflow(&ws).await? {
                None => {
                    let wf = client
                        .create_custom_workflow(&ws, &def.name, &phases_json, &vars_json)
                        .await?;
                    println!(
                        "✔ created workflow from '{file}': {} [{}]",
                        wf.workflow_id, wf.status
                    );
                }
                Some(wf) if wf.status == "running" => {
                    return Err(anyhow!(
                        "workflow {} is currently running — cancel it first before syncing",
                        wf.workflow_id
                    ));
                }
                Some(existing) => {
                    let updated = client
                        .edit_custom_workflow(&ws, &def.name, &phases_json)
                        .await?;
                    println!(
                        "✔ updated workflow {} from '{file}' [{} → {}]",
                        existing.workflow_id, existing.status, updated.status
                    );
                }
            }
        }

        RemoteWorkflowCmd::Save { workspace, name, description, icon } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            let slug = client
                .save_workflow_as_template(
                    &ws,
                    &name,
                    &description.unwrap_or_default(),
                    &icon,
                )
                .await?;
            println!("✔ saved as template '{slug}'");
            println!("  use with: strobes workflow remote attach --template {slug}");
        }

        RemoteWorkflowCmd::DeleteTemplate { slug } => {
            if !slug.starts_with("custom:") {
                return Err(anyhow!(
                    "only custom: templates can be deleted (got '{slug}')"
                ));
            }
            print!("delete template '{slug}'? [y/N] ");
            use std::io::Write;
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                println!("cancelled.");
                return Ok(());
            }
            client.delete_custom_workflow_template(&slug).await?;
            println!("✔ deleted template '{slug}'");
        }

        RemoteWorkflowCmd::Pause { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            client.pause_workflow(&ws).await?;
            println!("✔ workflow paused");
        }
        RemoteWorkflowCmd::Resume { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            client.resume_workflow(&ws).await?;
            println!("✔ workflow resumed");
        }
        RemoteWorkflowCmd::Cancel { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            client.cancel_workflow(&ws).await?;
            println!("✔ workflow cancelled");
        }
        RemoteWorkflowCmd::Restart { workspace, from_phase } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            match from_phase {
                Some(phase_key) => {
                    client.restart_workflow_from_phase(&ws, &phase_key).await?;
                    println!("✔ workflow restarted from phase '{phase_key}'");
                }
                None => {
                    client.restart_workflow(&ws).await?;
                    println!("✔ workflow restarted");
                }
            }
        }
        RemoteWorkflowCmd::Advance { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            client.advance_workflow_phase(&ws).await?;
            println!("✔ phase advanced");
        }
        RemoteWorkflowCmd::Watch { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            let mut terminal = ratatui::init();
            enable_mouse();
            let r = remote_wf_tui::run(&mut terminal, &client, ws, profile.clone(), tenant.to_string()).await;
            disable_mouse();
            ratatui::restore();
            r?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn notify_gating_and_payload() {
        // Disabled → nothing.
        assert!(notify_done_bytes(10, true, 4).is_none());
        // Too short → nothing.
        assert!(notify_done_bytes(2, false, 4).is_none());
        // Long enough → BEL + OSC 9 + OSC 777 present.
        let b = notify_done_bytes(5, false, 4).expect("should notify");
        assert_eq!(b[0], 0x07, "starts with BEL");
        let s = String::from_utf8(b).unwrap();
        assert!(s.contains("\x1b]9;"), "has OSC 9 notification");
        assert!(s.contains("\x1b]777;notify;"), "has OSC 777 notification");
        // min=0 notifies even instant replies.
        assert!(notify_done_bytes(0, false, 0).is_some());
    }

    /// Verifies the full CLI_LOCAL round-trip in Rust against a mock pulse
    /// server: server sends a `tool.local_execute`, the client runs it on the
    /// local machine and replies with `tool.local_result` carrying the output.
    #[tokio::test]
    async fn local_execute_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let ev = serde_json::json!({
                "type": "tool",
                "data": {"status": "local_execute", "toolName": "execute_command",
                         "requestId": "r1", "input": {"command": "echo RUST_ROUNDTRIP_OK"}}
            });
            ws.send(Message::Text(ev.to_string())).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(t) = msg {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    if v["type"] == "tool.local_result" {
                        return v;
                    }
                }
            }
            panic!("no tool.local_result received");
        });

        let profile = config::Profile {
            base_url: format!("http://127.0.0.1:{port}"),
            org_id: "o".into(),
            master_key: "k".into(),
            deployment: "enterprise".into(),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
        let _handle = pulse::connect(&profile, "t", tx, None).await.unwrap();

        let v = tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("server timeout")
            .expect("server task");
        assert_eq!(v["type"], "tool.local_result");
        let out = v["payload"]["output"].as_str().unwrap_or("");
        assert!(out.contains("RUST_ROUNDTRIP_OK"), "got: {out}");

        // Drain a couple app events to ensure the client surfaced the activity.
        let mut saw_local = false;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            if let pulse::AppEvent::LocalToolDone { .. } = ev {
                saw_local = true;
            }
        }
        assert!(saw_local, "client did not surface LocalToolDone");
    }

    /// Verifies the request_human_input round-trip: server sends an interrupt
    /// event, the client surfaces AppEvent::Interrupt, and respond_interrupt
    /// sends a correctly-shaped interrupt.response.
    #[tokio::test]
    async fn interrupt_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let ev = serde_json::json!({
                "type": "interrupt",
                "data": {"status": "requested", "interruptId": "i1",
                         "title": "OTP", "message": "Enter the code",
                         "formSchema": {"fields": [{"key": "otp", "label": "OTP", "type": "text"}]}}
            });
            ws.send(Message::Text(ev.to_string())).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(t) = msg {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    if v["type"] == "interrupt.response" {
                        return v;
                    }
                }
            }
            panic!("no interrupt.response received");
        });

        let profile = config::Profile {
            base_url: format!("http://127.0.0.1:{port}"),
            org_id: "o".into(),
            master_key: "k".into(),
            deployment: "enterprise".into(),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
        let handle = pulse::connect(&profile, "t", tx, None).await.unwrap();

        // Wait for the surfaced interrupt, then answer it.
        let mut answered = false;
        while let Ok(Some(ev)) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
            if let pulse::AppEvent::Interrupt { id, fields, .. } = ev {
                assert_eq!(id, "i1");
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].key, "otp");
                handle.respond_interrupt(&id, serde_json::json!({ "otp": "654321" }));
                answered = true;
                break;
            }
        }
        assert!(answered, "client never surfaced the interrupt");

        let v = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("server timeout")
            .expect("server task");
        assert_eq!(v["type"], "interrupt.response");
        assert_eq!(v["interrupt_id"], "i1");
        assert_eq!(v["response_data"]["otp"], "654321");
    }

    #[test]
    fn ws_url_and_prefix() {
        let p = config::Profile {
            base_url: "https://app.strobes.co".into(),
            org_id: "org".into(),
            master_key: "k".into(),
            deployment: "enterprise".into(),
            ..Default::default()
        };
        assert_eq!(p.api_prefix(), "/api/v1");
        assert_eq!(
            p.pulse_ws_url("T").unwrap(),
            "wss://app.strobes.co/ws/org/pulse/T/?api_key=k"
        );
    }
}
