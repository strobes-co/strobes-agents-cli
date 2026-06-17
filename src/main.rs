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

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use std::io::stdout;
use tokio::sync::mpsc;

use app::App;
use config::Config;

#[derive(Parser)]
#[command(name = "strobes", about = "Ratatui client for Strobes Agents AI")]
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
        Cmd::Tenants => cmd_tenants(&cfg),
        Cmd::Credits { workspace, thread } => cmd_credits(&profile, workspace, thread).await,
        Cmd::Status => cmd_status(&profile).await,
        Cmd::Workspaces => cmd_workspaces(&profile).await,
        Cmd::Threads => cmd_threads(&profile).await,
        Cmd::Bind { workspace, new, name, download, dir } => {
            cmd_bind(&mut cfg, &tenant, profile, workspace, new, name, download, dir).await
        }
        Cmd::Pull { workspace, dir } => cmd_pull(&mut cfg, &profile, workspace, dir).await,
        Cmd::Chat { thread, workspace, model, new } => {
            // Enter the alternate screen ONCE for the whole interactive flow
            // (pickers + chat) and restore ONCE, so switching between workspace,
            // thread, and chat never flashes the normal terminal.
            let mut terminal = ratatui::init();
            let _ = execute!(stdout(), EnableMouseCapture);
            let r = chat_flow(&mut terminal, &mut cfg, &tenant, profile, thread, workspace, new, model).await;
            let _ = execute!(stdout(), DisableMouseCapture);
            ratatui::restore();
            r
        }
        Cmd::Probe { thread, send, secs, model } => cmd_probe(&profile, &thread, send, secs, model).await,
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
                    match resolve_workspace_interactive(terminal, tenant, &profile).await? {
                        WsChoice::Pick(w) => profile.workspace_id = w,
                        WsChoice::Created { id, setup_thread, prompt } => {
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
) -> Result<WsChoice> {
    require_complete(profile)?;
    let auth = auth_line(profile, tenant);
    let client = api::ApiClient::new(profile.clone())?;
    let workspaces = client.list_workspaces().await.unwrap_or_default();
    let cur = profile.workspace_id.as_deref();
    // Item 0 = create new, item 1 = no workspace, then existing workspaces.
    let mut labels = vec![
        "➕  New workspace".to_string(),
        "↪  No workspace (all threads)".to_string(),
    ];
    for w in &workspaces {
        let name = if w.name.is_empty() { "(unnamed)" } else { &w.name };
        let mark = if Some(w.id.as_str()) == cur { "  ✓" } else { "" };
        labels.push(format!("{name}   [{}]{mark}", w.status));
    }
    loop {
        match picker::select_with(terminal, "Select a workspace", &labels, &auth).await? {
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
        picker::Nav::Back => Ok(ThreadChoice::Back),
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
            picker::Nav::Back | picker::Nav::Quit => return Ok(()),
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
            Err(e) => { let _ = tx.send(pulse::AppEvent::Error(format!("workspace download failed: {e}"))); }
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
        match api::ApiClient::new(p.clone())?.ping().await {
            Ok(_) => println!("\n✔ connection OK"),
            Err(e) => println!("\n✗ connection failed: {e}"),
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
    SwitchThread(String),
    BindWorkspace(String),
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

async fn run_chat(
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
                                KeyCode::Up | KeyCode::Char('k') => app.overlay_move(true),
                                KeyCode::Down | KeyCode::Char('j') => app.overlay_move(false),
                                KeyCode::PageUp => app.overlay_page(true, viewport_h),
                                KeyCode::PageDown => app.overlay_page(false, viewport_h),
                                KeyCode::Enter => {
                                    match app.overlay_enter() {
                                        Some((app::OverlayKind::Workspaces, id)) => defer = Some(Defer::BindWorkspace(id)),
                                        Some((app::OverlayKind::Threads, id)) => defer = Some(Defer::SwitchThread(id)),
                                        _ => {}
                                    }
                                }
                                KeyCode::Char('c') if ctrl => { if app.running { handle.cancel(); } }
                                _ => {}
                            }
                        } else if app.awaiting_input() {
                            match k.code {
                                KeyCode::Esc => quit = true,
                                KeyCode::Char(c) => app.input.push(c),
                                KeyCode::Backspace => { app.input.pop(); }
                                KeyCode::Enter => {
                                    let raw = std::mem::take(&mut app.input);
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
                                // Esc steps back into navigation (threads →
                                // workspaces) instead of quitting; ^C quits.
                                KeyCode::Esc => defer = Some(Defer::Threads),
                                KeyCode::Char('c') if ctrl => { if app.running { handle.cancel(); } else { quit = true; } }
                                KeyCode::Char('t') if ctrl => app.show_thinking = !app.show_thinking,
                                KeyCode::Char('r') if ctrl => app.markdown = !app.markdown,
                                KeyCode::Char('w') if ctrl => defer = Some(Defer::Workspaces),
                                KeyCode::Char('o') if ctrl => defer = Some(Defer::Threads),
                                KeyCode::Char('f') if ctrl => defer = Some(Defer::Findings),
                                KeyCode::Char('a') if ctrl => defer = Some(Defer::Approvals),
                                KeyCode::Char(c) => app.input.push(c),
                                KeyCode::Backspace => { app.input.pop(); }
                                KeyCode::Enter => {
                                    let text = app.input.trim().to_string();
                                    if !text.is_empty() {
                                        app.echo_user(&text);
                                        handle.send_user_message(&text);
                                        app.input.clear();
                                        app.running = true;
                                        app.status = "sending…".into();
                                    }
                                }
                                KeyCode::PageUp => app.page(true, viewport_h),
                                KeyCode::PageDown => app.page(false, viewport_h),
                                KeyCode::Up => app.scroll_line(true),
                                KeyCode::Down => app.scroll_line(false),
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Event::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => if app.overlay_active() { app.overlay_move(true) } else { app.scroll_lines(true, 3) },
                        MouseEventKind::ScrollDown => if app.overlay_active() { app.overlay_move(false) } else { app.scroll_lines(false, 3) },
                        _ => {}
                    },
                    Some(Err(_)) | None => quit = true,
                    _ => {}
                }
            }
            maybe_app = rx.recv() => {
                match maybe_app {
                    Some(ev) => app.on_app_event(ev),
                    None => quit = true,
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
        let label = format!("{name}{bound}  · {} · {tcount} · ◈ {cr:.2} cr", w.status);
        let detail = vec![name, format!("status: {}", w.status), format!("threads: {tcount}"),
            format!("credits: {cr:.3}"),
            format!("id: {}", w.id),
            String::new(), "Press Enter to bind this workspace (enables ^F / ^A).".into()];
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
    let ws = client.list_workspaces().await?;
    let (counts, credits) =
        tokio::join!(workspace_thread_counts(client, &ws), workspace_credits_map(client));
    Ok(workspace_items(ws, &counts, &credits, profile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

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
