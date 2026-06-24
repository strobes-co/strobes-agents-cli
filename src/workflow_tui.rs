use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::config::Profile;
use crate::workflow::WorkflowDef;
use crate::workflow_runner::WfEvent;

/// Render accumulated task output: event prefix lines get colors, everything
/// else is parsed as Markdown via the shared renderer.
fn render_output(text: &str) -> Vec<Line<'static>> {
    let mut result: Vec<Line<'static>> = Vec::new();
    let mut md_buf = String::new();

    let flush_md = |buf: &mut String, out: &mut Vec<Line<'static>>| {
        let trimmed = buf.trim();
        if !trimmed.is_empty() {
            out.extend(crate::markdown::render(trimmed));
        }
        buf.clear();
    };

    for line in text.split('\n') {
        match classify_event_line(line) {
            Some(styled) => {
                flush_md(&mut md_buf, &mut result);
                result.push(styled);
            }
            None => {
                md_buf.push_str(line);
                md_buf.push('\n');
            }
        }
    }
    flush_md(&mut md_buf, &mut result);
    result
}

/// If `line` is one of our prefixed event lines, return a styled `Line`.
/// Returns `None` for plain text lines (which belong to the Markdown buffer).
fn classify_event_line(line: &str) -> Option<Line<'static>> {
    let l = line.trim().to_string();
    if l.is_empty() {
        return None; // empty lines stay in the MD buffer for paragraph spacing
    }
    // ▶ tool_name(args)  — tool call start
    if l.starts_with('▶') {
        return Some(Line::from(Span::styled(
            l,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
    }
    // ◀ tool_name: result  — tool output
    if l.starts_with('◀') {
        return Some(Line::from(Span::styled(
            l,
            Style::default().fg(Color::Blue),
        )));
    }
    // ✗ tool_name: error  — tool failure
    if l.starts_with('✗') {
        return Some(Line::from(Span::styled(
            l,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    // ℹ …  — system / note
    if l.starts_with('ℹ') {
        return Some(Line::from(Span::styled(
            l,
            Style::default().fg(Color::DarkGray),
        )));
    }
    // 💭 …  — thinking
    if l.starts_with("💭") {
        return Some(Line::from(Span::styled(
            l,
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::ITALIC | Modifier::DIM),
        )));
    }
    // [task:…] or [auto-approved] …
    if l.starts_with("[task:") || l.starts_with("[auto-approved]") {
        return Some(Line::from(Span::styled(
            l,
            Style::default().fg(Color::Yellow),
        )));
    }
    None
}

/// Render the log sidebar lines with a color per prefix character.
fn render_log_line(s: &str) -> Line<'static> {
    let l = s.trim().to_string();
    let color = if l.starts_with('✔') {
        Color::Green
    } else if l.starts_with('✗') {
        Color::Red
    } else if l.starts_with('▶') {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    Line::from(Span::styled(l, Style::default().fg(color)))
}

#[derive(Debug, Clone, PartialEq)]
enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed(String),
}

struct TaskState {
    name: String,
    status: TaskStatus,
}

struct PhaseState {
    name: String,
    tasks: Vec<TaskState>,
}

struct TuiApp {
    wf_name: String,
    phases: Vec<PhaseState>,
    logs: Vec<String>,
    /// Raw accumulated text per task (tokens appended as they arrive).
    outputs: HashMap<String, String>,
    /// task name → thread id
    thread_ids: HashMap<String, String>,
    /// `None` = show the combined log pane.
    selected: Option<String>,
    /// Manual scroll offset (lines from top). Only used when `follow` is false.
    scroll: usize,
    /// When true, the output pane auto-tails to the bottom.
    follow: bool,
    workspace_id: Option<String>,
    done: bool,
    failed: Option<String>,
    profile: Profile,
    tenant: String,
}

impl TuiApp {
    fn new(def: &WorkflowDef, profile: Profile, tenant: String) -> Self {
        let phases = def
            .phases
            .iter()
            .map(|p| PhaseState {
                name: p.name.clone(),
                tasks: p
                    .tasks
                    .iter()
                    .map(|t| TaskState {
                        name: t.name.clone(),
                        status: TaskStatus::Pending,
                    })
                    .collect(),
            })
            .collect();
        TuiApp {
            wf_name: def.name.clone(),
            phases,
            logs: Vec::new(),
            outputs: HashMap::new(),
            thread_ids: HashMap::new(),
            selected: None,
            scroll: 0,
            follow: true,
            workspace_id: None,
            done: false,
            failed: None,
            profile,
            tenant,
        }
    }

    fn apply(&mut self, ev: &WfEvent) {
        match ev {
            WfEvent::WorkspaceReady { id, name } => {
                self.workspace_id = Some(id.clone());
                self.logs.push(format!("✔ workspace: {name}"));
            }
            WfEvent::SetupStarted { thread_id } => {
                self.thread_ids.insert("workspace-setup".into(), thread_id.clone());
                self.phases.insert(0, PhaseState {
                    name: "Workspace Setup".into(),
                    tasks: vec![TaskState {
                        name: "workspace-setup".into(),
                        status: TaskStatus::Running,
                    }],
                });
                let short_id = &thread_id[..8.min(thread_id.len())];
                self.logs.push(format!("▶ workspace-setup ({}…)", short_id));
                self.selected = Some("workspace-setup".into());
                self.scroll = 0;
                self.follow = true;
            }
            WfEvent::PhaseStarted { phase } => {
                self.logs.push(format!("▶ phase: {phase}"));
            }
            WfEvent::TaskStarted { task, thread_id, .. } => {
                self.thread_ids.insert(task.clone(), thread_id.clone());
                self.set_status(task, TaskStatus::Running);
                self.logs
                    .push(format!("▶ {task} ({}…)", &thread_id[..8.min(thread_id.len())]));
                if self.selected.is_none() {
                    self.selected = Some(task.clone());
                    self.scroll = 0;
                    self.follow = true;
                }
            }
            WfEvent::TaskOutput { task, text } => {
                // Append raw token text — newlines in the stream produce real line breaks.
                self.outputs.entry(task.clone()).or_default().push_str(text);
                // Re-enable auto-follow whenever new output arrives for the visible task.
                if self.selected.as_deref() == Some(task.as_str()) {
                    self.follow = true;
                }
            }
            WfEvent::TaskDone { task } => {
                self.set_status(task, TaskStatus::Done);
                self.logs.push(format!("✔ {task}"));
                if task == "workspace-setup" {
                    // Shift focus to the first real workflow task.
                    self.selected = self
                        .phases
                        .iter()
                        .find(|p| p.name != "Workspace Setup")
                        .and_then(|p| p.tasks.first())
                        .map(|t| t.name.clone());
                    self.scroll = 0;
                    self.follow = true;
                }
            }
            WfEvent::TaskFailed { task, reason } => {
                self.set_status(task, TaskStatus::Failed(reason.clone()));
                self.logs.push(format!("✗ {task}: {reason}"));
            }
            WfEvent::TaskSkipped { task } => {
                self.set_status(task, TaskStatus::Done);
                self.logs.push(format!("↷ {task} (already done)"));
            }
            WfEvent::WorkflowDone => {
                self.done = true;
                self.logs.push("✔ workflow complete — press q to exit".into());
            }
            WfEvent::WorkflowFailed { reason } => {
                self.failed = Some(reason.clone());
                self.logs.push("✗ workflow failed — press q to exit".into());
            }
            WfEvent::Log(msg) => {
                self.logs.push(msg.clone());
            }
        }
    }

    fn set_status(&mut self, name: &str, status: TaskStatus) {
        for phase in &mut self.phases {
            for task in &mut phase.tasks {
                if task.name == name {
                    task.status = status;
                    return;
                }
            }
        }
    }

    fn all_task_names(&self) -> Vec<String> {
        self.phases
            .iter()
            .flat_map(|p| p.tasks.iter().map(|t| t.name.clone()))
            .collect()
    }

    fn move_selection(&mut self, up: bool) {
        let names = self.all_task_names();
        if names.is_empty() {
            return;
        }
        let pos = self
            .selected
            .as_deref()
            .and_then(|s| names.iter().position(|n| n == s))
            .unwrap_or(0);
        let new_pos = if up {
            pos.saturating_sub(1)
        } else {
            (pos + 1).min(names.len() - 1)
        };
        self.selected = Some(names[new_pos].clone());
        self.scroll = 0;
        self.follow = true;
    }

    fn draw(&self, f: &mut Frame) {
        let area = f.area();
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

        // ── Header ──────────────────────────────────────────────────────────
        let (status_text, header_fg) = if let Some(err) = &self.failed {
            (format!("FAILED: {err}"), Color::Red)
        } else if self.done {
            ("COMPLETE".into(), Color::Green)
        } else {
            ("RUNNING".into(), Color::Yellow)
        };
        let ws = self
            .workspace_id
            .as_deref()
            .map(|id| format!("  ws:{}", &id[..8.min(id.len())]))
            .unwrap_or_default();
        let header = Paragraph::new(format!(" {} — {}{} ", self.wf_name, status_text, ws))
            .style(Style::default().fg(header_fg).add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(header, outer[0]);

        // ── Body: left tree | right output ──────────────────────────────────
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(outer[1]);

        // Left: phase / task tree
        let mut items: Vec<ListItem> = Vec::new();
        for phase in &self.phases {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("◆ {}", phase.name),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))));
            for task in &phase.tasks {
                let (icon, color) = match &task.status {
                    TaskStatus::Pending => ("○", Color::DarkGray),
                    TaskStatus::Running => ("◌", Color::Yellow),
                    TaskStatus::Done => ("✓", Color::Green),
                    TaskStatus::Failed(_) => ("✗", Color::Red),
                };
                let selected = self.selected.as_deref() == Some(&task.name);
                let base = Style::default().fg(color);
                let style = if selected { base.bg(Color::DarkGray) } else { base };
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{icon} {}", task.name), style),
                ])));
            }
        }
        let tree =
            List::new(items).block(Block::default().borders(Borders::ALL).title(" Tasks "));
        f.render_widget(tree, body[0]);

        // Right: output / log pane
        // Inner height = widget height minus top+bottom borders.
        let inner_h = body[1].height.saturating_sub(2) as usize;

        let (pane_title, lines): (String, Vec<Line<'static>>) = if let Some(tname) = &self.selected {
            let raw = self.outputs.get(tname).map(|s| s.as_str()).unwrap_or("");
            (format!(" Output: {tname} "), render_output(raw))
        } else {
            let rendered: Vec<Line<'static>> = self.logs.iter().map(|s| render_log_line(s)).collect();
            (" Log ".into(), rendered)
        };

        // Compute scroll: auto-tail when following, manual otherwise.
        let scroll = if self.follow {
            lines.len().saturating_sub(inner_h)
        } else {
            self.scroll
        };

        let output = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(pane_title))
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0));
        f.render_widget(output, body[1]);

        // ── Footer ──────────────────────────────────────────────────────────
        let scroll_hint = if self.follow { "PgUp: scroll  " } else { "PgDn/f: follow  " };
        let footer = Paragraph::new(format!(
            "  ↑↓ select task  |  Enter: open chat  |  Tab: toggle log  |  {scroll_hint}|  q: quit"
        ))
        .style(Style::default().fg(Color::DarkGray));
        f.render_widget(footer, outer[2]);
    }
}

pub async fn run_tui(
    terminal: &mut ratatui::DefaultTerminal,
    def: WorkflowDef,
    ev_rx: mpsc::UnboundedReceiver<WfEvent>,
    profile: Profile,
    tenant: String,
) -> anyhow::Result<()> {
    tui_loop(terminal, def, ev_rx, profile, tenant).await
}

async fn tui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    def: WorkflowDef,
    mut ev_rx: mpsc::UnboundedReceiver<WfEvent>,
    profile: Profile,
    tenant: String,
) -> anyhow::Result<()> {
    let mut app = TuiApp::new(&def, profile, tenant);
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));
    let mut open_thread: Option<String> = None;

    loop {
        terminal.draw(|f| app.draw(f))?;

        tokio::select! {
            maybe = events.next() => {
                match maybe {
                    Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                        match k.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('c') if ctrl => break,
                            // Re-enable follow mode.
                            KeyCode::Char('f') => {
                                app.follow = true;
                                app.scroll = 0;
                            }
                            KeyCode::Enter => {
                                if let Some(task) = &app.selected {
                                    if let Some(tid) = app.thread_ids.get(task).cloned() {
                                        open_thread = Some(tid);
                                    }
                                }
                            }
                            KeyCode::Up => app.move_selection(true),
                            KeyCode::Down => app.move_selection(false),
                            KeyCode::Tab => {
                                if app.selected.is_some() {
                                    app.selected = None;
                                    app.scroll = 0;
                                    app.follow = true;
                                } else {
                                    app.selected = app
                                        .phases
                                        .iter()
                                        .flat_map(|p| p.tasks.iter())
                                        .next()
                                        .map(|t| t.name.clone());
                                    app.scroll = 0;
                                    app.follow = true;
                                }
                            }
                            KeyCode::PageUp => {
                                app.follow = false;
                                app.scroll = app.scroll.saturating_sub(10);
                            }
                            KeyCode::PageDown => {
                                app.scroll = app.scroll.saturating_add(10);
                            }
                            _ => {}
                        }
                    }
                    None => break,
                    _ => {}
                }
            }
            maybe = ev_rx.recv() => {
                if let Some(ev) = maybe {
                    app.apply(&ev);
                }
            }
            _ = ticker.tick() => {}
        }

        if let Some(tid) = open_thread.take() {
            let profile = app.profile.clone();
            let tenant = app.tenant.clone();
            let _ = crate::run_chat(terminal, &tenant, profile, tid, None, None).await;
            terminal.clear()?;
        }
    }

    Ok(())
}
