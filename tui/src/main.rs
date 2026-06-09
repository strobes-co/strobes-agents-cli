//! `strobes-tui` — a Ratatui terminal client for Strobes Agents AI.
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
#[command(name = "strobes-tui", about = "Ratatui client for Strobes Agents AI")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Config profile name
    #[arg(long, global = true)]
    profile: Option<String>,
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
    if let Some(p) = &cli.profile {
        cfg.current_profile = p.clone();
    }
    let profile = cfg.current();

    match cli.cmd.unwrap_or(Cmd::Chat { thread: None, workspace: None, model: None, new: false }) {
        Cmd::Status => cmd_status(&profile).await,
        Cmd::Workspaces => cmd_workspaces(&profile).await,
        Cmd::Threads => cmd_threads(&profile).await,
        Cmd::Bind { workspace, new, name, download, dir } => {
            cmd_bind(&mut cfg, profile, workspace, new, name, download, dir).await
        }
        Cmd::Pull { workspace, dir } => cmd_pull(&mut cfg, &profile, workspace, dir).await,
        Cmd::Chat { thread, workspace, model, new } => {
            let mut profile = profile;
            if let Some(w) = workspace {
                profile.workspace_id = Some(w);
            }
            // Resolve a thread: explicit > picker/create (when --new or none bound).
            let explicit = if new { None } else { thread.or(profile.thread_id.clone()) };
            let thread_id = match explicit {
                Some(t) => t,
                None => resolve_thread_interactive(&profile).await?,
            };
            // Persist the binding to the stored profile.
            {
                let name = cfg.current_profile.clone();
                let p = cfg.profile_mut(&name);
                p.thread_id = Some(thread_id.clone());
                if profile.workspace_id.is_some() {
                    p.workspace_id = profile.workspace_id.clone();
                }
                let _ = cfg.save();
            }
            run_chat(profile, thread_id, model).await
        }
        Cmd::Probe { thread, send, secs, model } => cmd_probe(&profile, &thread, send, secs, model).await,
    }
}

/// Show a thread picker (with a "new thread" option) and return a thread id,
/// creating one via REST if requested.
async fn resolve_thread_interactive(profile: &config::Profile) -> Result<String> {
    require_complete(profile)?;
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

    let choice = picker::select("Select a thread", &labels).await?;
    match choice {
        None => Err(anyhow!("cancelled")),
        Some(0) => {
            let id = client
                .create_thread("CLI session", profile.workspace_id.as_deref())
                .await?;
            Ok(id)
        }
        Some(i) => Ok(threads[i - 1].id.clone()),
    }
}

async fn cmd_bind(
    cfg: &mut Config,
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
            Some(i) => workspaces[i].id.clone(),
            None => return Err(anyhow!("cancelled")),
        }
    };

    let pname = cfg.current_profile.clone();
    cfg.profile_mut(&pname).workspace_id = Some(ws_id.clone());
    cfg.save()?;
    println!("✔ bound workspace {ws_id}");

    if download {
        download_workspace(cfg, &profile, &ws_id, dir).await?;
    } else {
        println!("(run `strobes-tui pull` to download its files locally)");
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

async fn cmd_status(p: &config::Profile) -> Result<()> {
    println!("base_url    {}", if p.base_url.is_empty() { "(unset)" } else { &p.base_url });
    println!("org_id      {}", if p.org_id.is_empty() { "(unset)" } else { &p.org_id });
    println!("master_key  {}", config::redact(&p.master_key));
    println!("deployment  {}", p.deployment);
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

async fn cmd_workspaces(p: &config::Profile) -> Result<()> {
    require_complete(p)?;
    let rows = api::ApiClient::new(p.clone())?.list_workspaces().await?;
    if rows.is_empty() {
        println!("(no workspaces)");
    }
    for w in rows {
        let bound = if Some(&w.id) == p.workspace_id.as_ref() { " ●" } else { "" };
        println!("{}  {}{}  [{}]", w.id, w.name, bound, w.status);
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
        return Err(anyhow!("profile incomplete — run `strobes-ai login` first"));
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

async fn fetch_title(client: &api::ApiClient, thread_id: &str, app: &mut App) {
    if let Ok(threads) = client.list_threads(None).await {
        if let Some(t) = threads.into_iter().find(|t| t.id == thread_id) {
            if !t.title.is_empty() {
                app.set_title(t.title);
            }
        }
    }
}

async fn load_history(client: &api::ApiClient, thread_id: &str, app: &mut App) {
    match client.get_thread_events(thread_id, 0, 2000).await {
        Ok(events) if !events.is_empty() => app.seed_history_events(events),
        _ => {
            if let Ok(hist) = client.get_thread_history(thread_id, 100).await {
                app.seed_history(hist.messages);
            }
        }
    }
    if let Ok(hist) = client.get_thread_history(thread_id, 1).await {
        if let Some(run) = hist.active_run {
            app.note_active_run(&run.status);
        }
    }
}

async fn run_chat(profile: config::Profile, thread_id: String, model: Option<i64>) -> Result<()> {
    require_complete(&profile)?;

    let mut profile = profile;
    let client = api::ApiClient::new(profile.clone())?;
    let mut app = App::new(thread_id.clone(), profile.base_url.clone());
    app.has_workspace = profile.workspace_id.is_some();

    load_history(&client, &thread_id, &mut app).await;
    fetch_title(&client, &thread_id, &mut app).await;
    if let Ok(cmds) = client.list_slash_commands().await {
        app.set_slash_commands(cmds);
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let mut app_tx = tx.clone(); // for background tasks (workspace sync) to post UI events
    let mut handle = pulse::connect(&profile, &thread_id, tx, model).await?;

    if let Some(ws) = profile.workspace_id.clone() {
        spawn_workspace_sync(profile.clone(), ws, app_tx.clone());
    }

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);
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
                                KeyCode::Esc => { app.overlay_esc(); }
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
                                KeyCode::Esc => quit = true,
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
            Some(Defer::Workspaces) => match client.list_workspaces().await {
                Ok(ws) => app.open_overlay(app::OverlayKind::Workspaces, "Workspaces".into(), workspace_items(ws, &profile)),
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
                None => match client.list_workspaces().await {
                    Ok(ws) => {
                        app.notice("pick a workspace, then press ^F for its findings");
                        app.open_overlay(app::OverlayKind::Workspaces, "Workspaces".into(), workspace_items(ws, &profile));
                    }
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
            },
            Some(Defer::Approvals) => match profile.workspace_id.clone() {
                Some(ws) => match client.list_workspace_approvals(&ws).await {
                    Ok(aps) => app.open_overlay(app::OverlayKind::Approvals, "Approvals".into(), approval_items(aps)),
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
                None => match client.list_workspaces().await {
                    Ok(ws) => {
                        app.notice("pick a workspace, then press ^A for its approvals");
                        app.open_overlay(app::OverlayKind::Workspaces, "Workspaces".into(), workspace_items(ws, &profile));
                    }
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
            },
            Some(Defer::BindWorkspace(id)) => {
                profile.workspace_id = Some(id.clone());
                handle.set_workspace(Some(id.clone()));
                app.has_workspace = true;
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
                            load_history(&client, &new_thread, &mut app).await;
                            fetch_title(&client, &new_thread, &mut app).await;
                        }
                        Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("switch failed: {e}"))),
                    }
                }
            }
            None => {}
        }
    };

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
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

fn workspace_items(ws: Vec<api::Workspace>, profile: &config::Profile) -> Vec<app::OverlayItem> {
    ws.into_iter()
        .map(|w| {
            let bound = if Some(&w.id) == profile.workspace_id.as_ref() { " ●" } else { "" };
            let name = if w.name.is_empty() { "(unnamed)".into() } else { w.name.clone() };
            let label = format!("{name}{bound}  · {}", w.status);
            let detail = vec![name, format!("status: {}", w.status), format!("id: {}", w.id),
                String::new(), "Press Enter to bind this workspace (enables ^F / ^A).".into()];
            app::OverlayItem { label, detail, action: Some(w.id) }
        })
        .collect()
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
