//! Live TUI for a cloud-orchestrated, CLI-executed workflow.
//!
//! The cloud runs the orchestration (phases, tasks, the agent loop); THIS
//! machine runs the work. The TUI polls status every 2s and opens a pulse
//! connection to every *running* task thread — each connection both renders the
//! agent's tokens/tool-calls in the details pane AND services `tool.local_execute`
//! locally (see pulse::handle_frame), so the task's shell/code/browser commands
//! run here. A periodic heartbeat tells the server the CLI is present; leaving
//! pauses the workflow so it waits for a CLI to reattach rather than stalling.
//!
//! Layout: left = phase/task tree  |  right = details + live output for selected item
//! Keys: ↑↓ navigate · Enter open thread · [p]ause · [r]esume · [s]tart · [d]etach · [q]uit

use std::collections::HashMap;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use tokio::sync::mpsc;

use crate::api::{ApiClient, Thread, WorkflowState};
use crate::config::Profile;
use crate::pulse::{self, PulseHandle, StreamItem};

/// Max characters of live output retained per task thread (keeps memory bounded
/// on a long-running workflow).
const MAX_OUTPUT_CHARS: usize = 64 * 1024;

/// Convert a StreamItem into a displayable line for the live output pane.
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

// ── Tree model ────────────────────────────────────────────────────────────────

#[derive(Clone)]
enum TreeRow {
    Phase {
        idx: usize, // index into WorkflowState::phases
    },
    Task {
        phase_idx: usize,
        thread_id: String,
        display: String, // stripped title
        status: String,
        created_at: Option<String>,
    },
}

/// Parse `"Task: Phase N[suffix]: rest"` → phase order N.
fn parse_phase_order(title: &str) -> Option<i64> {
    let rest = title.strip_prefix("Task: Phase ")?;
    let n_end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..n_end].parse::<i64>().ok()
}

/// Strip the `"Task: Phase N[suffix]: "` prefix from a thread title.
fn strip_prefix(title: &str) -> &str {
    if let Some(rest) = title.strip_prefix("Task: Phase ") {
        if let Some(pos) = rest.find(": ") {
            return &rest[pos + 2..];
        }
    }
    title
}

fn build_tree(state: &WorkflowState, threads: &[Thread]) -> Vec<TreeRow> {
    let mut tree = Vec::new();
    for (pi, phase) in state.phases.iter().enumerate() {
        tree.push(TreeRow::Phase { idx: pi });
        // threads whose title encodes this phase's order, sorted by created_at asc
        let mut phase_threads: Vec<&Thread> = threads
            .iter()
            .filter(|t| parse_phase_order(&t.title) == Some(phase.order))
            .collect();
        phase_threads.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        for t in phase_threads {
            tree.push(TreeRow::Task {
                phase_idx: pi,
                thread_id: t.id.clone(),
                display: strip_prefix(&t.title).to_string(),
                status: t.status.clone(),
                created_at: t.created_at.clone(),
            });
        }
    }
    tree
}

// ── App state ─────────────────────────────────────────────────────────────────

struct App {
    workspace_id: String,
    profile: Profile,
    state: Option<WorkflowState>,
    threads: Vec<Thread>,
    tree: Vec<TreeRow>,
    list_state: ListState,
    error: Option<String>,
    feedback: Option<String>,
    confirm_detach: bool,
    spinner: u64,
    /// Live per-thread output, keyed by thread id (accumulated from pulse).
    outputs: HashMap<String, String>,
    /// Open pulse connections, keyed by thread id. Dropping a handle stops it.
    streams: HashMap<String, PulseHandle>,
    /// Tagged stream events from every live connection funnel through here.
    stream_tx: mpsc::UnboundedSender<(String, pulse::AppEvent)>,
}

impl App {
    fn new(
        workspace_id: String,
        profile: Profile,
        stream_tx: mpsc::UnboundedSender<(String, pulse::AppEvent)>,
    ) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            workspace_id,
            profile,
            state: None,
            threads: Vec::new(),
            tree: Vec::new(),
            list_state,
            error: None,
            feedback: None,
            confirm_detach: false,
            spinner: 0,
            outputs: HashMap::new(),
            streams: HashMap::new(),
            stream_tx,
        }
    }

    /// Append streamed text to a thread's live buffer, trimming from the front
    /// once it exceeds the retention cap.
    fn push_output(&mut self, thread_id: String, text: &str) {
        let buf = self.outputs.entry(thread_id).or_default();
        buf.push_str(text);
        if buf.len() > MAX_OUTPUT_CHARS {
            let cut = buf.len() - MAX_OUTPUT_CHARS;
            // Trim on a char boundary.
            let mut idx = cut;
            while idx < buf.len() && !buf.is_char_boundary(idx) {
                idx += 1;
            }
            *buf = buf[idx..].to_string();
        }
    }

    /// Open pulse streams for every running task thread not already streaming,
    /// and drop connections for tasks that are no longer running. Called after
    /// each status poll.
    async fn sync_streams(&mut self) {
        use std::collections::HashSet;

        // Threads that should currently be streaming = running task threads.
        let mut running: HashSet<String> = HashSet::new();
        for row in &self.tree {
            if let TreeRow::Task { thread_id, status, .. } = row {
                if status == "running" {
                    running.insert(thread_id.clone());
                }
            }
        }

        // Drop finished/vanished connections (keeps their captured output).
        let stale: Vec<String> = self
            .streams
            .keys()
            .filter(|tid| !running.contains(*tid))
            .cloned()
            .collect();
        for tid in stale {
            self.streams.remove(&tid); // Drop stops the pulse supervisor.
        }

        // Open new connections for newly-running threads.
        let to_open: Vec<String> = running
            .into_iter()
            .filter(|tid| !self.streams.contains_key(tid))
            .collect();
        for tid in to_open {
            let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
            let out_tx = self.stream_tx.clone();
            let tid_for_task = tid.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    if out_tx.send((tid_for_task.clone(), ev)).is_err() {
                        break;
                    }
                }
            });
            match pulse::connect(&self.profile, &tid, tx, None, None).await {
                Ok(handle) => {
                    self.streams.insert(tid.clone(), handle);
                    self.outputs.entry(tid).or_default();
                }
                Err(_) => {
                    // Best-effort: a failed connect just means no live view for
                    // this task; the 2s status poll still tracks its state.
                }
            }
        }
    }

    fn rebuild_tree(&mut self) {
        if let Some(s) = &self.state {
            let cur = self.list_state.selected().unwrap_or(0);
            self.tree = build_tree(s, &self.threads);
            self.list_state.select(Some(cur.min(self.tree.len().saturating_sub(1))));
        }
    }

    fn move_cursor(&mut self, up: bool) {
        let len = self.tree.len();
        if len == 0 {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0);
        let next = if up { cur.saturating_sub(1) } else { (cur + 1).min(len - 1) };
        self.list_state.select(Some(next));
    }

    fn selected_thread_id(&self) -> Option<String> {
        match self.tree.get(self.list_state.selected().unwrap_or(0)) {
            Some(TreeRow::Task { thread_id, .. }) => Some(thread_id.clone()),
            _ => None,
        }
    }

    fn status_style(&self) -> (&'static str, Color) {
        match self.state.as_ref().map(|s| s.status.as_str()) {
            None => ("LOADING", Color::DarkGray),
            Some("running") => ("RUNNING", Color::Green),
            Some("paused") => ("PAUSED", Color::Yellow),
            Some("completed") => ("COMPLETE", Color::Cyan),
            Some("failed") => ("FAILED", Color::Red),
            Some("cancelled") => ("CANCELLED", Color::DarkGray),
            Some("pending") => ("PENDING", Color::Yellow),
            _ => ("UNKNOWN", Color::White),
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        self.draw_header(f, chunks[0]);
        self.draw_body(f, chunks[1]);
        self.draw_footer(f, chunks[2]);
    }

    fn draw_header(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let (status_str, color) = self.status_style();
        let slug = self.state.as_ref()
            .and_then(|s| s.template_slug.as_deref())
            .unwrap_or("—");
        let ws = &self.workspace_id[..8.min(self.workspace_id.len())];
        let tasks = self.state.as_ref()
            .map(|s| format!("   {}/{} tasks", s.completed_tasks, s.total_tasks))
            .unwrap_or_default();
        f.render_widget(
            Paragraph::new(format!("  {status_str}  {slug}  ws:{ws}…{tasks}"))
                .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
                .block(Block::default().borders(Borders::ALL)),
            area,
        );
    }

    fn draw_body(&mut self, f: &mut Frame, area: ratatui::layout::Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(area);

        // ── Left: phase + task tree ───────────────────────────────────────────
        let cur = self.list_state.selected().unwrap_or(0);
        let items: Vec<ListItem> = if self.tree.is_empty() {
            vec![ListItem::new("  loading…")]
        } else {
            self.tree.iter().enumerate().map(|(i, row)| {
                let selected = i == cur;
                match row {
                    TreeRow::Phase { idx } => {
                        let phase = self.state.as_ref()
                            .and_then(|s| s.phases.get(*idx));
                        let (icon, color) = phase_icon(phase.map(|p| p.status.as_str()).unwrap_or(""));
                        let name = phase.map(|p| p.phase_name.as_str()).unwrap_or("?");
                        let current = self.state.as_ref()
                            .and_then(|s| s.current_phase_key.as_deref())
                            == phase.map(|p| p.phase_key.as_str());
                        let cur_mark = if current { " ◀" } else { "" };
                        let style = if selected {
                            Style::default().fg(color).add_modifier(Modifier::BOLD | Modifier::REVERSED)
                        } else if current {
                            Style::default().fg(color).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(color)
                        };
                        ListItem::new(Line::from(Span::styled(
                            format!("{icon} {name}{cur_mark}"),
                            style,
                        )))
                    }
                    TreeRow::Task { status, display, .. } => {
                        let (icon, color) = task_icon(status);
                        let label = trunc_str(display, cols[0].width.saturating_sub(8) as usize);
                        let style = if selected {
                            Style::default().fg(color).add_modifier(Modifier::REVERSED)
                        } else {
                            Style::default().fg(color)
                        };
                        ListItem::new(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(format!("╰ {icon} {label}"), style),
                        ]))
                    }
                }
            }).collect()
        };

        f.render_stateful_widget(
            List::new(items).block(Block::default().borders(Borders::ALL).title(" Phases & Tasks ")),
            cols[0],
            &mut self.list_state,
        );

        // ── Right: details for selected item ─────────────────────────────────
        let mut lines: Vec<Line<'static>> = Vec::new();

        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("  ✗ {err}"),
                Style::default().fg(Color::Red),
            )));
            lines.push(Line::from(""));
        }
        if let Some(fb) = &self.feedback {
            lines.push(Line::from(Span::styled(
                format!("  ✔ {fb}"),
                Style::default().fg(Color::Green),
            )));
            lines.push(Line::from(""));
        }

        match self.tree.get(cur) {
            Some(TreeRow::Phase { idx }) => {
                let phase = self.state.as_ref().and_then(|s| s.phases.get(*idx));
                if let Some(p) = phase {
                    let (_, color) = phase_icon(&p.status);
                    lines.push(Line::from(vec![
                        Span::raw("  phase:    "),
                        Span::styled(p.phase_name.clone(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    ]));
                    lines.push(Line::from(format!("  key:      {}", p.phase_key)));
                    lines.push(Line::from(format!("  status:   {}", p.status)));
                    if let Some(s) = &p.started_at {
                        lines.push(Line::from(format!("  started:  {}", fmt_time(s))));
                    }
                    if let Some(s) = &p.completed_at {
                        lines.push(Line::from(format!("  finished: {}", fmt_time(s))));
                    }
                    // task count for this phase
                    let tc = self.tree.iter().filter(|r| matches!(r, TreeRow::Task { phase_idx, .. } if *phase_idx == *idx)).count();
                    if tc > 0 {
                        lines.push(Line::from(format!("  tasks:    {tc} thread(s)")));
                    }
                }
            }
            Some(TreeRow::Task { thread_id, display, status, created_at, .. }) => {
                let (_, color) = task_icon(status);
                let streaming = self.streams.contains_key(thread_id);
                let mut title_spans = vec![
                    Span::raw("  task:    "),
                    Span::styled(display.clone(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                ];
                if streaming {
                    title_spans.push(Span::styled(
                        "  ● live",
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    ));
                }
                lines.push(Line::from(title_spans));
                lines.push(Line::from(format!("  status:  {status}")));
                lines.push(Line::from(format!("  thread:  {}…", &thread_id[..8.min(thread_id.len())])));
                if let Some(ts) = created_at {
                    lines.push(Line::from(format!("  started: {}", fmt_time(ts))));
                }
                lines.push(Line::from(Span::styled(
                    "  Enter: open thread in chat",
                    Style::default().fg(Color::Cyan),
                )));

                // Live output streamed from the cloud for this task thread.
                let out = self.outputs.get(thread_id).map(|s| s.as_str()).unwrap_or("");
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  ── live output ──",
                    Style::default().fg(Color::DarkGray),
                )));
                if out.trim().is_empty() {
                    let placeholder = if streaming {
                        "  (waiting for the agent to emit output…)"
                    } else {
                        "  (no live output — task not running; press Enter for full transcript)"
                    };
                    lines.push(Line::from(Span::styled(
                        placeholder,
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    // Show the tail so the newest output stays in view.
                    let tail: Vec<&str> = out.lines().rev().take(200).collect();
                    for l in tail.into_iter().rev() {
                        lines.push(Line::from(format!("  {l}")));
                    }
                }
            }
            None => {
                if self.state.is_none() && self.error.is_none() {
                    lines.push(Line::from(Span::styled(
                        "  Fetching workflow status…",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }

        // Progress bar at the bottom of the details pane.
        if let Some(s) = &self.state {
            if s.total_tasks > 0 {
                lines.push(Line::from(""));
                let pct = (s.completed_tasks * 100 / s.total_tasks.max(1)) as usize;
                let bar_w = (cols[1].width as usize).saturating_sub(16).clamp(6, 36);
                let filled = bar_w * pct / 100;
                let bar = format!(
                    "  [{}{}]  {pct}%  ({}/{})",
                    "█".repeat(filled),
                    "░".repeat(bar_w.saturating_sub(filled)),
                    s.completed_tasks,
                    s.total_tasks,
                );
                lines.push(Line::from(Span::styled(bar, Style::default().fg(Color::Cyan))));
            }
        }

        f.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(" Details "))
                .wrap(Wrap { trim: false }),
            cols[1],
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let text = if self.confirm_detach {
            "  Detach cancels and removes the workflow.  [y] confirm  [any] cancel".to_string()
        } else {
            let wf_controls = match self.state.as_ref().map(|s| s.status.as_str()) {
                Some("running") => "[p] pause",
                Some("paused") => "[r] resume",
                Some("completed") | Some("failed") | Some("cancelled") => "[s] restart",
                _ => "",
            };
            let spin = ["◐", "◓", "◑", "◒"][(self.spinner / 2) as usize % 4];
            let has_task = matches!(self.tree.get(self.list_state.selected().unwrap_or(0)), Some(TreeRow::Task { .. }));
            let enter_hint = if has_task { "  Enter: open chat  |" } else { "" };
            format!("  ↑↓ navigate{enter_hint}  {wf_controls}  [d] detach  [q] quit  |  {spin} 2s")
        };
        f.render_widget(
            Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn phase_icon(status: &str) -> (&'static str, Color) {
    match status {
        "completed" => ("✓", Color::Green),
        "running" => ("◆", Color::Yellow),
        "failed" => ("✗", Color::Red),
        "skipped" => ("↷", Color::DarkGray),
        "paused" => ("⏸", Color::Yellow),
        _ => ("○", Color::DarkGray),
    }
}

fn task_icon(status: &str) -> (&'static str, Color) {
    match status {
        "completed" => ("✓", Color::Green),
        "running" => ("◌", Color::Yellow),
        "failed" => ("✗", Color::Red),
        _ => ("○", Color::DarkGray),
    }
}

fn fmt_time(s: &str) -> String {
    let date = s.split('T').next().unwrap_or(s);
    let time = s.split('T').nth(1)
        .and_then(|t| t.split(['.', '+']).next())
        .unwrap_or("");
    if time.is_empty() { date.to_string() } else { format!("{date} {time}") }
}

fn trunc_str(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…", &s[..max.saturating_sub(1)]) }
}

// ── Polling ───────────────────────────────────────────────────────────────────

async fn refresh(client: &ApiClient, workspace_id: &str, app: &mut App) {
    match client.workspace_workflow(workspace_id).await {
        Ok(s) => {
            app.state = s;
            app.error = None;
        }
        Err(e) => app.error = Some(e.to_string()),
    }
    // Also fetch threads so task rows stay current.
    if let Ok(threads) = client.list_threads(Some(workspace_id)).await {
        // Filter to threads created at or after the workflow started.
        let since = app.state.as_ref().and_then(|s| s.started_at.as_deref());
        app.threads = threads.into_iter().filter(|t| {
            match (since, t.created_at.as_deref()) {
                (Some(wf), Some(tc)) => tc >= wf,
                _ => true,
            }
        }).collect();
    }
    app.rebuild_tree();
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    client: &ApiClient,
    workspace_id: String,
    profile: Profile,
    tenant: String,
) -> Result<()> {
    // Live task-output events from every per-thread pulse connection arrive here,
    // tagged with their thread id.
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<(String, pulse::AppEvent)>();
    let mut app = App::new(workspace_id.clone(), profile.clone(), stream_tx);

    // Initial load before the event loop, then open live streams for anything
    // already running.
    refresh(client, &workspace_id, &mut app).await;
    app.sync_streams().await;

    let mut events = EventStream::new();
    let mut poll_ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    poll_ticker.tick().await; // skip the immediate first tick
    let mut draw_ticker = tokio::time::interval(std::time::Duration::from_millis(150));
    // Presence heartbeat: this CLI is the execution surface for a CLI-local
    // workflow, so it must tell the server it is still here. If these stop,
    // the server pauses the workflow (nothing left to service local tools).
    let mut hb_ticker = tokio::time::interval(std::time::Duration::from_secs(10));
    let _ = client.heartbeat_workflow(&workspace_id).await;

    loop {
        terminal.draw(|f| app.draw(f))?;

        tokio::select! {
            maybe = events.next() => {
                let Some(Ok(Event::Key(k))) = maybe else {
                    if maybe.is_none() { break; }
                    continue;
                };
                if k.kind != KeyEventKind::Press { continue; }
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

                if app.confirm_detach {
                    app.confirm_detach = false;
                    if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        app.feedback = None;
                        app.error = None;
                        match client.detach_workflow(&workspace_id).await {
                            Ok(()) => { app.state = None; app.tree.clear(); app.feedback = Some("detached".to_string()); }
                            Err(e) => app.error = Some(e.to_string()),
                        }
                    }
                    continue;
                }

                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if ctrl => break,
                    KeyCode::Up => app.move_cursor(true),
                    KeyCode::Down => app.move_cursor(false),
                    KeyCode::Enter => {
                        if let Some(tid) = app.selected_thread_id() {
                            let _ = crate::run_chat(terminal, &tenant, profile.clone(), tid, None, None, None).await;
                            terminal.clear()?;
                        }
                    }
                    KeyCode::Char('p') => {
                        app.feedback = None; app.error = None;
                        match client.pause_workflow(&workspace_id).await {
                            Ok(()) => app.feedback = Some("paused".to_string()),
                            Err(e) => app.error = Some(e.to_string()),
                        }
                        refresh(client, &workspace_id, &mut app).await;
                    }
                    KeyCode::Char('r') => {
                        app.feedback = None; app.error = None;
                        match client.resume_workflow(&workspace_id).await {
                            Ok(()) => app.feedback = Some("resumed".to_string()),
                            Err(e) => app.error = Some(e.to_string()),
                        }
                        refresh(client, &workspace_id, &mut app).await;
                    }
                    KeyCode::Char('s') => {
                        app.feedback = None; app.error = None;
                        match client.restart_workflow(&workspace_id).await {
                            Ok(()) => app.feedback = Some("restarted".to_string()),
                            Err(e) => app.error = Some(e.to_string()),
                        }
                        refresh(client, &workspace_id, &mut app).await;
                    }
                    KeyCode::Char('d') => app.confirm_detach = true,
                    _ => {}
                }
            }
            maybe_stream = stream_rx.recv() => {
                if let Some((tid, ev)) = maybe_stream {
                    if let pulse::AppEvent::Stream(item) = ev {
                        if let Some(text) = format_stream_item(&item) {
                            app.push_output(tid, &text);
                        }
                    }
                }
            }
            _ = poll_ticker.tick() => {
                refresh(client, &workspace_id, &mut app).await;
                app.sync_streams().await;
            }
            _ = hb_ticker.tick() => {
                let _ = client.heartbeat_workflow(&workspace_id).await;
            }
            _ = draw_ticker.tick() => {
                app.spinner = app.spinner.wrapping_add(1);
            }
        }
    }

    // Leaving the TUI stops all local-tool servicing, so a still-running
    // CLI-local workflow would have nothing to execute its commands. Pause it
    // on the way out (best-effort) so it waits for a CLI to reattach rather
    // than stalling on tool calls that never get answered. A detached workflow
    // (state cleared) needs no pause.
    let still_running = app
        .state
        .as_ref()
        .map(|s| matches!(s.status.as_str(), "running" | "pending"))
        .unwrap_or(false);
    if still_running {
        let _ = client.pause_workflow_on_exit(&workspace_id).await;
    }

    Ok(())
}
