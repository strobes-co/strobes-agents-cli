//! Live TUI for monitoring and controlling a remote workflow.
//!
//! Layout: left = phase/task tree  |  right = details for selected item
//! Keys: ↑↓ navigate · Enter open thread · [p]ause · [r]esume · [s]tart · [d]etach · [q]uit

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

use crate::api::{ApiClient, Thread, WorkflowState};
use crate::config::Profile;

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
    state: Option<WorkflowState>,
    threads: Vec<Thread>,
    tree: Vec<TreeRow>,
    list_state: ListState,
    error: Option<String>,
    feedback: Option<String>,
    confirm_detach: bool,
    spinner: u64,
}

impl App {
    fn new(workspace_id: String) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            workspace_id,
            state: None,
            threads: Vec::new(),
            tree: Vec::new(),
            list_state,
            error: None,
            feedback: None,
            confirm_detach: false,
            spinner: 0,
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
                lines.push(Line::from(vec![
                    Span::raw("  task:    "),
                    Span::styled(display.clone(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                ]));
                lines.push(Line::from(format!("  status:  {status}")));
                lines.push(Line::from(format!("  thread:  {}…", &thread_id[..8.min(thread_id.len())])));
                if let Some(ts) = created_at {
                    lines.push(Line::from(format!("  started: {}", fmt_time(ts))));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  Enter: open thread in chat",
                    Style::default().fg(Color::Cyan),
                )));
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
    let mut app = App::new(workspace_id.clone());

    // Initial load before the event loop.
    refresh(client, &workspace_id, &mut app).await;

    let mut events = EventStream::new();
    let mut poll_ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    poll_ticker.tick().await; // skip the immediate first tick
    let mut draw_ticker = tokio::time::interval(std::time::Duration::from_millis(150));

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
                            let _ = crate::run_chat(terminal, &tenant, profile.clone(), tid, None, None).await;
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
            _ = poll_ticker.tick() => {
                refresh(client, &workspace_id, &mut app).await;
            }
            _ = draw_ticker.tick() => {
                app.spinner = app.spinner.wrapping_add(1);
            }
        }
    }

    Ok(())
}
