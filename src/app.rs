//! Ratatui chat application: transcript pane, streaming text rendered as
//! Markdown, tool cards, input box, and status bar.
//!
//! The transcript is a list of `Block`s. Assistant messages are stored as raw
//! Markdown strings and re-rendered with `tui-markdown` every frame, so partial
//! streamed Markdown formats live. Tool cards / tasks / system notes are stored
//! as pre-styled lines.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as TuiBlock, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
    Frame,
};

use serde_json::{Map, Value};

use crate::pulse::{AppEvent, Field, StreamItem};

/// Status of a tracked agent task.
#[derive(Clone, Copy, PartialEq)]
pub enum TaskStatus {
    Created,
    Running,
    Completed,
    Failed,
}

impl TaskStatus {
    fn icon(self) -> &'static str {
        match self {
            TaskStatus::Created   => "○",
            TaskStatus::Running   => "⟳",
            TaskStatus::Completed => "✓",
            TaskStatus::Failed    => "✗",
        }
    }
    fn color(self) -> Color {
        match self {
            TaskStatus::Created   => Color::DarkGray,
            TaskStatus::Running   => Color::Cyan,
            TaskStatus::Completed => Color::Green,
            TaskStatus::Failed    => Color::Red,
        }
    }
}

/// A single agent task tracked in the tasks panel.
#[derive(Clone)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub status: TaskStatus,
    pub agent: Option<String>,
}

/// Execution state of a tool call, driving its dot colour/animation.
#[derive(Clone, Copy, PartialEq)]
enum ToolStatus {
    Running,
    Done,
    Failed,
}

/// One transcript entry.
enum Block {
    User(String),
    Assistant { agent: String, md: String },
    Thinking(String),
    Plain(Line<'static>),
    Rule(String),
    /// A tool call line ("⏺ name(args)") with a live status: the dot blinks
    /// while Running, then turns green (Done) or red (Failed).
    Tool { name: String, detail: String, status: ToolStatus },
}

struct Stream {
    thinking: bool,
    agent: String,
    buf: String,
}

/// State of an in-progress `request_human_input` interrupt the user is answering.
struct Pending {
    id: String,
    fields: Vec<Field>,
    idx: usize,
    answers: Map<String, Value>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Findings,
    Approvals,
    Threads,
    Workspaces,
    Files,
    Models,
}

/// One row in an overlay browser. `detail` lines are shown when the row is
/// opened; `action` (an id) is returned to the caller for select-to-act
/// overlays (threads/workspaces).
pub struct OverlayItem {
    pub label: String,
    pub detail: Vec<String>,
    pub action: Option<String>,
}

struct Overlay {
    kind: OverlayKind,
    title: String,
    items: Vec<OverlayItem>,
    /// Position of the cursor within the *filtered* (visible) list.
    sel: usize,
    detail_open: bool,
    dscroll: u16,
    /// Type-to-search filter (case-insensitive substring over item labels).
    filter: String,
}

impl Overlay {
    /// Indices into `items` that match the current filter (all when empty).
    fn visible(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.items.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.label.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// The real `items` index under the cursor (maps sel → filtered item).
    fn current(&self) -> Option<usize> {
        self.visible().get(self.sel).copied()
    }
}

pub struct App {
    blocks: Vec<Block>,
    stream: Option<Stream>,
    pub input: String,
    /// Byte offset of the edit cursor within `input` (always on a char boundary).
    input_cursor: usize,
    pub status: String,
    pub connected: bool,
    pub running: bool,
    pub show_thinking: bool,
    pub markdown: bool,
    pub scroll: u16,
    pub max_scroll: u16,
    pub follow: bool,
    /// The user scrolled up to read; suppress auto-scroll (even while output
    /// streams) until they return to the bottom.
    pub pinned: bool,
    pub thread_id: String,
    pub title: String,
    pub base: String,
    pending: Option<Pending>,
    last_task: Option<String>,
    last_tool: Option<String>,
    overlay: Option<Overlay>,
    pub has_workspace: bool,
    // Shown top-right in the transcript: where work is running.
    workspace_name: Option<String>,
    task_label: Option<String>,
    // "authenticated …" subtitle (cached for the overlay banner).
    auth_line: String,
    // Animation frame for the working spinner.
    spinner: usize,
    // When the current turn started (for the elapsed timer).
    run_started: Option<std::time::Instant>,
    // Elapsed of the last completed turn (shown briefly when idle).
    last_elapsed: Option<std::time::Duration>,
    slash_commands: Vec<crate::api::SlashCmd>,
    slash_sel: usize,
    run_credits: f64,
    run_tokens: i64,
    session_credits: f64,
    session_tokens: i64,
    // Lifetime usage for THIS thread (from cli/credits at open, kept current as
    // each run completes).
    thread_credits: f64,
    thread_tokens: i64,
    /// The AI model currently selected for this chat session (None = org default).
    current_model: Option<i64>,
    /// Live task list populated by "task" stream events; keyed insertion-order.
    tasks: Vec<Task>,
    /// When true, mouse capture is off so the terminal can do native text selection.
    pub select_mode: bool,
}

/// ASCII banner shown at the top of a fresh chat transcript and the pickers.
/// The subtitle carries the build version (via `concat!` + `env!`).
pub const BANNER: &str = concat!(
    " ███████╗████████╗██████╗  ██████╗ ██████╗ ███████╗███████╗\n",
    " ██╔════╝╚══██╔══╝██╔══██╗██╔═══██╗██╔══██╗██╔════╝██╔════╝\n",
    " ███████╗   ██║   ██████╔╝██║   ██║██████╔╝█████╗  ███████╗\n",
    " ╚════██║   ██║   ██╔══██╗██║   ██║██╔══██╗██╔══╝  ╚════██║\n",
    " ███████║   ██║   ██║  ██║╚██████╔╝██████╔╝███████╗███████║\n",
    " ╚══════╝   ╚═╝   ╚═╝  ╚═╝ ╚═════╝ ╚═════╝ ╚══════╝╚══════╝\n",
    "            A G E N T S   A I  ·  v",
    env!("CARGO_PKG_VERSION"),
    "  ·  terminal client",
);

impl App {
    pub fn new(thread_id: String, base: String, tenant: String, org_id: String) -> Self {
        let mut app = Self {
            blocks: Vec::new(),
            stream: None,
            input: String::new(),
            input_cursor: 0,
            status: "connecting…".into(),
            connected: false,
            running: false,
            show_thinking: false,
            markdown: true,
            scroll: 0,
            max_scroll: 0,
            follow: true,
            pinned: false,
            thread_id,
            title: String::new(),
            base,
            pending: None,
            last_task: None,
            last_tool: None,
            overlay: None,
            has_workspace: false,
            workspace_name: None,
            task_label: None,
            auth_line: String::new(),
            spinner: 0,
            run_started: None,
            last_elapsed: None,
            slash_commands: Vec::new(),
            slash_sel: 0,
            run_credits: 0.0,
            run_tokens: 0,
            session_credits: 0.0,
            session_tokens: 0,
            thread_credits: 0.0,
            thread_tokens: 0,
            current_model: None,
            tasks: Vec::new(),
            select_mode: false,
        };
        // Banner (cyan) + the authenticated tenant on top, indented to match
        // the picker pages.
        const LEFT: &str = "   ";
        for line in BANNER.lines() {
            app.blocks.push(Block::Plain(Line::from(Span::styled(
                format!("{LEFT}{line}"),
                Style::default().fg(Color::Cyan),
            ))));
        }
        let org = if org_id.len() > 8 { &org_id[..8] } else { &org_id };
        app.auth_line = format!("✔ authenticated · tenant: {tenant} · {} · org {org}", app.base);
        app.blocks.push(Block::Plain(Line::from(Span::styled(
            format!("{LEFT}{}", app.auth_line),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ))));
        app.sys("Enter: send · Ctrl-C: cancel/quit · Esc: quit · PgUp/PgDn: scroll · Ctrl-T: thinking · Ctrl-R: markdown");
        app
    }

    // ---- transcript mutation -------------------------------------------

    fn flush_stream(&mut self) {
        if let Some(s) = self.stream.take() {
            if s.thinking {
                // Drop empty / duplicate consecutive thinking blocks.
                if s.buf.trim().is_empty() {
                    return;
                }
                if let Some(Block::Thinking(prev)) = self.blocks.last() {
                    if prev.trim() == s.buf.trim() {
                        return;
                    }
                }
                self.blocks.push(Block::Thinking(s.buf));
            } else {
                self.push_assistant(s.agent, s.buf);
            }
        }
    }

    /// Mark the most recent still-running tool as finished (green on success,
    /// red on failure), stopping its blink.
    fn finish_last_tool(&mut self, ok: bool) {
        for b in self.blocks.iter_mut().rev() {
            if let Block::Tool { status, .. } = b {
                if *status == ToolStatus::Running {
                    *status = if ok { ToolStatus::Done } else { ToolStatus::Failed };
                    return;
                }
            }
        }
    }

    /// Settle any tools still marked Running (e.g. when a run ends without an
    /// explicit per-tool result) so nothing is left blinking.
    pub fn settle_running_tools(&mut self) {
        for b in self.blocks.iter_mut() {
            if let Block::Tool { status, .. } = b {
                if *status == ToolStatus::Running {
                    *status = ToolStatus::Done;
                }
            }
        }
    }

    /// Append an assistant message, merging into the previous assistant block
    /// from the same agent so a multi-segment reply renders under ONE header.
    fn push_assistant(&mut self, agent: String, md: String) {
        if md.trim().is_empty() {
            return;
        }
        if let Some(Block::Assistant { agent: prev_agent, md: prev_md }) = self.blocks.last_mut() {
            if *prev_agent == agent {
                prev_md.push_str("\n\n");
                prev_md.push_str(&md);
                self.follow = true;
                return;
            }
        }
        self.blocks.push(Block::Assistant { agent, md });
        self.last_task = None;
        self.last_tool = None;
        self.follow = true;
    }

    fn push_thinking(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        if let Some(Block::Thinking(prev)) = self.blocks.last() {
            if prev.trim() == text.trim() {
                return;
            }
        }
        self.flush_stream();
        self.blocks.push(Block::Thinking(text));
        self.last_task = None;
    }

    fn push_task(&mut self, icon: &str, color: Color, title: &str) {
        if title.trim().is_empty() {
            return;
        }
        let key = format!("{icon} {title}");
        if self.last_task.as_deref() == Some(&key) {
            return;
        }
        self.flush_stream();
        self.blocks.push(Block::Plain(Line::from(Span::styled(
            format!("  {key}"),
            Style::default().fg(color),
        ))));
        self.last_task = Some(key);
        self.follow = true;
    }

    fn sys(&mut self, text: &str) {
        self.flush_stream();
        self.blocks.push(Block::Plain(Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(Color::DarkGray),
        ))));
    }

    fn push(&mut self, line: Line<'static>) {
        self.flush_stream();
        self.blocks.push(Block::Plain(line));
        self.last_task = None;
        self.last_tool = None;
        self.follow = true;
    }

    fn feed(&mut self, agent: Option<String>, text: String, thinking: bool) {
        match &mut self.stream {
            Some(s) if s.thinking == thinking => s.buf.push_str(&text),
            _ => {
                self.flush_stream();
                self.stream = Some(Stream {
                    thinking,
                    agent: agent.unwrap_or_else(|| "agent".into()),
                    buf: text,
                });
            }
        }
        self.follow = true;
    }

    pub fn on_app_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Connected => {
                self.connected = true;
                self.status = "connected".into();
            }
            AppEvent::Disconnected(why) => {
                self.connected = false;
                self.running = false;
                self.settle_running_tools();
                self.status = format!("disconnected — {why}");
            }
            AppEvent::RunStarted => {
                self.running = true;
                self.status = "running…".into();
                self.run_credits = 0.0;
                self.run_tokens = 0;
                self.tasks.clear();
                if self.run_started.is_none() {
                    self.run_started = Some(std::time::Instant::now());
                }
            }
            AppEvent::Credits { credits, tokens, final_run } => {
                if final_run {
                    // Authoritative run total → fold into the session and thread
                    // lifetime, and show it as the run's figure.
                    self.session_credits += credits;
                    self.session_tokens += tokens;
                    self.thread_credits += credits;
                    self.thread_tokens += tokens;
                    self.run_credits = credits;
                    self.run_tokens = tokens;
                } else {
                    // Live per-call delta.
                    self.run_credits += credits;
                    self.run_tokens += tokens;
                }
            }
            AppEvent::RunFinished(label) => {
                self.running = false;
                self.settle_running_tools();
                self.last_elapsed = self.run_started.take().map(|t| t.elapsed());
                let took = self.last_elapsed.map(|d| format!(" · {}", fmt_elapsed(d))).unwrap_or_default();
                self.status = format!("idle ({label}){took}");
                self.rule(&label);
            }
            AppEvent::Notice(n) => self.notice(&n),
            AppEvent::Error(e) => self.push(Line::from(Span::styled(
                format!("error: {e}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ))),
            AppEvent::LocalToolDone { name, ms: _, exit, err } => {
                // Success is shown by the tool's ⎿ result line; only surface
                // local failures / non-zero exits here.
                if let Some(e) = err {
                    self.push(Line::from(Span::styled(
                        format!("  ⎿ ✗ {name}: {e}"), Style::default().fg(Color::Red))));
                } else if !matches!(exit, Some(0) | None) {
                    self.push(Line::from(Span::styled(
                        format!("  ⎿ {name} exit {}", exit.unwrap()), Style::default().fg(Color::Red))));
                }
            }
            AppEvent::Interrupt { id, title, message, fields } => {
                self.running = false;
                self.status = "input needed ⏸".into();
                self.push(Line::from(Span::styled(
                    format!("⏸ {title}"),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )));
                if !message.is_empty() {
                    self.push(Line::from(Span::styled(
                        format!("   {message}"),
                        Style::default().fg(Color::Cyan),
                    )));
                }
                self.pending = Some(Pending { id, fields, idx: 0, answers: Map::new() });
                self.follow = true;
            }
            AppEvent::Stream(item) => self.on_stream(item),
        }
    }

    /// True while the agent is blocked awaiting `request_human_input`.
    pub fn awaiting_input(&self) -> bool {
        self.pending.is_some()
    }

    /// Label of the field currently being collected (for the input box title).
    pub fn pending_label(&self) -> Option<String> {
        self.pending.as_ref().and_then(|p| {
            p.fields.get(p.idx).map(|f| {
                let req = if p.fields.len() > 1 {
                    format!(" [{}/{}]", p.idx + 1, p.fields.len())
                } else {
                    String::new()
                };
                format!("{}{}", f.label, req)
            })
        })
    }

    /// Feed the typed value as the answer to the current interrupt field.
    /// Returns `Some((interrupt_id, response_data))` once all fields are filled.
    pub fn submit_interrupt_value(&mut self, text: &str) -> Option<(String, Value)> {
        // Read the current field (short immutable borrow), compute the value.
        let (key, label, value) = {
            let p = self.pending.as_ref()?;
            let field = p.fields.get(p.idx)?;
            let value: Value = match field.ftype.as_str() {
                "number" => text
                    .trim()
                    .parse::<f64>()
                    .map(Value::from)
                    .unwrap_or_else(|_| Value::from(text)),
                "checkbox" => Value::from(matches!(
                    text.trim().to_lowercase().as_str(),
                    "y" | "yes" | "true" | "1" | "on"
                )),
                _ => Value::from(text),
            };
            (field.key.clone(), field.label.clone(), value)
        };

        // Echo the answer (mutable borrow — no `pending` borrow held here).
        self.push(Line::from(Span::styled(
            format!("   {label} = {text}"),
            Style::default().fg(Color::Magenta),
        )));

        // Record it and advance.
        let p = self.pending.as_mut()?;
        p.answers.insert(key, value);
        p.idx += 1;
        if p.idx >= p.fields.len() {
            let done = self.pending.take().unwrap();
            self.running = true;
            self.status = "resuming…".into();
            Some((done.id, Value::Object(done.answers)))
        } else {
            None
        }
    }

    fn on_stream(&mut self, item: StreamItem) {
        match item.kind.as_str() {
            "token" => {
                if let Some(t) = item.text {
                    self.feed(item.agent, t, false);
                }
            }
            "thinking" => {
                if let Some(t) = item.text {
                    self.feed(item.agent, t, true);
                }
            }
            "tool_start" => {
                // One compact line per tool call: "⏺ name(args)". A tool often
                // emits an empty `start` then a detailed `local_execute` for the
                // same call — collapse them by replacing the previous line.
                let name = item.tool_name.unwrap_or_default();
                let detail = item.detail.unwrap_or_default();
                if self.last_tool.as_deref() == Some(&name) {
                    // Same call: enrich the earlier (often empty) start in place.
                    if let Some(Block::Tool { detail: d, .. }) = self.blocks.last_mut() {
                        if !detail.is_empty() {
                            *d = detail;
                        }
                    } else {
                        self.blocks.push(Block::Tool { name: name.clone(), detail, status: ToolStatus::Running });
                    }
                } else {
                    self.flush_stream();
                    self.blocks.push(Block::Tool { name: name.clone(), detail, status: ToolStatus::Running });
                    self.last_task = None;
                    self.last_tool = Some(name);
                }
                self.follow = true;
            }
            "tool_output" => {
                // Result rendered as a dim child line under the tool call.
                let ms = item.status.map(|s| format!("  ·{s}")).unwrap_or_default();
                let d = item.detail.unwrap_or_default();
                let body = if d.is_empty() { "(ok)".to_string() } else { truncate(&d, 220) };
                self.flush_stream();
                self.finish_last_tool(true); // dot → green
                self.blocks.push(Block::Plain(Line::from(Span::styled(
                    format!("  ⎿ {body}{ms}"),
                    Style::default().fg(Color::DarkGray),
                ))));
                self.last_tool = None;
                self.follow = true;
            }
            "tool_failed" => {
                self.flush_stream();
                self.finish_last_tool(false); // dot → red
                self.blocks.push(Block::Plain(Line::from(Span::styled(
                    format!("  ⎿ ✗ {}", truncate(&item.detail.unwrap_or_default(), 220)),
                    Style::default().fg(Color::Red),
                ))));
                self.last_tool = None;
                self.follow = true;
            }
            "task" => {
                let title = item.text.unwrap_or_default();
                let task_status = match item.status.as_deref() {
                    Some("started")   => TaskStatus::Running,
                    Some("completed") => TaskStatus::Completed,
                    Some("failed")    => TaskStatus::Failed,
                    _                 => TaskStatus::Created,
                };
                if let Some(id) = &item.task_id {
                    if let Some(existing) = self.tasks.iter_mut().find(|t| &t.id == id) {
                        existing.status = task_status;
                        if !title.trim().is_empty() {
                            existing.title = title.clone();
                        }
                    } else if !title.trim().is_empty() {
                        self.tasks.push(Task {
                            id: id.clone(),
                            title: title.clone(),
                            status: task_status,
                            agent: item.agent.clone(),
                        });
                    }
                }
                // Keep a subtle inline marker for task.created events only.
                // Skip for plan.updated batch tasks (status="pending") — those only go to the panel.
                if !matches!(item.status.as_deref(), Some("pending"))
                    && task_status == TaskStatus::Created && !title.trim().is_empty()
                    && self.last_task.as_deref() != Some(&title)
                {
                    self.flush_stream();
                    self.blocks.push(Block::Plain(Line::from(Span::styled(
                        format!("◇ {title}"),
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM),
                    ))));
                    self.task_label = Some(title.clone());
                    self.last_task = Some(title);
                    self.last_tool = None;
                    self.follow = true;
                }
            }
            "approval" => self.push(Line::from(Span::styled(
                format!("  ⚠ approval ({}) {} — auto-approved",
                    item.detail.unwrap_or_default(), item.text.unwrap_or_default()),
                Style::default().fg(Color::Yellow),
            ))),
            "system" => {
                // All system subtypes (run.started/created, credit.update,
                // message.segment.completed, seq.reserved, queue events) are
                // noise in the transcript — the agent header + content and the
                // run-completed rule are enough. Drop them.
            }
            "note" => {
                // Reserved for genuinely useful notes (artifacts, queued).
                if let Some(t) = item.text {
                    let t = t.trim();
                    let useful = t.starts_with("artifact")
                        || t.starts_with("queued")
                        || t.contains("interrupt");
                    if useful {
                        self.sys(&format!("· {t}"));
                    }
                }
            }
            _ => {}
        }
    }

    // ---- line building (owned, re-rendered each frame) -----------------

    fn build_lines(&self) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        // Track the agent currently "speaking" so we only print the agent
        // header when it changes (not on every segment or after every tool).
        let mut cur_agent: Option<String> = None;
        for b in &self.blocks {
            match b {
                Block::User(text) => {
                    cur_agent = None; // next agent reply re-prints its header
                    blank(&mut out);
                    out.push(Line::from(vec![
                        Span::styled("› ".to_string(), Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                        Span::styled(text.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    ]));
                    blank(&mut out);
                }
                Block::Assistant { agent, md } => {
                    if cur_agent.as_deref() != Some(agent.as_str()) {
                        blank(&mut out);
                        out.push(agent_header(agent));
                        cur_agent = Some(agent.clone());
                    }
                    self.render_md(md, &mut out);
                }
                Block::Thinking(s) => self.render_thinking(s, &mut out),
                Block::Plain(l) => out.push(l.clone()),
                Block::Tool { name, detail, status } => {
                    // Dot: blinks while running, green when done, red on failure.
                    let dot_style = match status {
                        ToolStatus::Running => {
                            // ~3 ticks per state → a calm ≈0.7s blink cycle.
                            let on = (self.spinner / 3) % 2 == 0;
                            Style::default()
                                .fg(if on { Color::Cyan } else { Color::DarkGray })
                                .add_modifier(Modifier::BOLD)
                        }
                        ToolStatus::Done => {
                            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                        }
                        ToolStatus::Failed => {
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                        }
                    };
                    let mut spans = vec![
                        Span::styled("⏺ ".to_string(), dot_style),
                        Span::styled(
                            name.clone(),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if !detail.is_empty() {
                        spans.push(Span::styled(
                            format!("({detail})"),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    out.push(Line::from(spans));
                }
                Block::Rule(label) => {
                    cur_agent = None;
                    blank(&mut out);
                    out.push(rule_line(label));
                }
            }
        }
        if let Some(s) = &self.stream {
            if s.thinking {
                if self.show_thinking {
                    self.render_thinking(&s.buf, &mut out);
                } else {
                    // Collapsed: animated dot + elapsed time + live token count.
                    let on = (self.spinner / 3) % 2 == 0;
                    let dot_style = Style::default()
                        .fg(if on { Color::Magenta } else { Color::DarkGray })
                        .add_modifier(Modifier::BOLD);
                    let text_style = Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC);
                    let el = self
                        .run_started
                        .map(|t| format!("  {}", fmt_elapsed(t.elapsed())))
                        .unwrap_or_default();
                    let tok = if self.run_tokens > 0 {
                        format!("  {}", fmt_tokens(self.run_tokens))
                    } else {
                        String::new()
                    };
                    out.push(Line::from(vec![
                        Span::styled("✻ ", dot_style),
                        Span::styled(format!("thinking…{el}{tok}"), text_style),
                    ]));
                }
            } else {
                if cur_agent.as_deref() != Some(s.agent.as_str()) {
                    blank(&mut out);
                    out.push(agent_header(&s.agent));
                }
                self.render_md(&s.buf, &mut out);
            }
        }
        #[cfg(windows)]
        out.iter_mut().for_each(win_safe_line);
        out
    }

    fn render_md(&self, md: &str, out: &mut Vec<Line<'static>>) {
        if self.markdown {
            out.extend(md_to_owned(md));
        } else {
            for l in md.split('\n') {
                out.push(Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(Color::White),
                )));
            }
        }
    }

    fn render_thinking(&self, s: &str, out: &mut Vec<Line<'static>>) {
        if !self.show_thinking {
            return;
        }
        let header = Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD);
        let body = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD | Modifier::ITALIC);
        out.push(Line::from(Span::styled("✻ thinking", header)));
        for l in s.split('\n') {
            out.push(Line::from(Span::styled(format!("  {l}"), body)));
        }
    }

    // ---- rendering ------------------------------------------------------

    pub fn draw(&mut self, f: &mut Frame) {
        // Input box grows with the wrapped message (borders + content), capped
        // so the transcript, hint and status rows always remain on screen.
        let tw = Self::input_text_width(f.area().width);
        let cap = f.area().height.saturating_sub(3).max(3);
        let input_h = (self.input_visual_rows(tw) + 2).clamp(3, cap);
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(input_h),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());
        if self.overlay.is_some() {
            // Close the chat view: show only the picker, anchored to the bottom
            // of the screen (just above the input bar).
            self.draw_overlay(f, chunks[0]);
        } else {
            // Show tasks side panel when there are tasks and the screen is wide enough.
            let body_area = chunks[0];
            if !self.tasks.is_empty() && body_area.width > 60 {
                let panel_w = (body_area.width / 4).clamp(22, 36);
                let hchunks = Layout::horizontal([
                    Constraint::Min(1),
                    Constraint::Length(panel_w),
                ]).split(body_area);
                self.draw_transcript(f, hchunks[0]);
                self.draw_tasks_panel(f, hchunks[1]);
            } else {
                self.draw_transcript(f, body_area);
            }
            if self.slash_open() {
                self.draw_slash_popup(f, chunks[0]);
            }
        }
        self.draw_input(f, chunks[1]);
        self.draw_hint(f, chunks[2]);
        self.draw_status(f, chunks[3]);
    }

    fn draw_overlay(&self, f: &mut Frame, area: Rect) {
        let o = match &self.overlay {
            Some(o) => o,
            None => return,
        };
        if o.detail_open {
            let lines: Vec<Line<'static>> = o
                .current()
                .and_then(|i| o.items.get(i))
                .map(|it| it.detail.iter().map(|l| Line::from(l.clone())).collect())
                .unwrap_or_default();
            let block = TuiBlock::default()
                .borders(Borders::ALL)
                .title(format!(" {} — detail (Esc back) ", o.title))
                .border_style(Style::default().fg(Color::Cyan));
            f.render_widget(
                Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((o.dscroll, 0)),
                area,
            );
        } else {
            // Only the items matching the type-to-search filter are shown.
            let visible = o.visible();
            let items: Vec<ListItem> = visible
                .iter()
                .map(|&i| ListItem::new(Line::from(o.items[i].label.clone())))
                .collect();
            let mut state = ListState::default();
            if !visible.is_empty() {
                state.select(Some(o.sel.min(visible.len() - 1)));
            }
            let count = visible.len();
            // Anchor the picker to the bottom of the area (just above the input),
            // sized to its contents but capped to the available height.
            let needed = (count as u16).saturating_add(2);
            let h = needed.clamp(3, area.height.max(3));
            let oarea = Rect {
                x: area.x,
                y: area.y + area.height.saturating_sub(h),
                width: area.width,
                height: h,
            };
            let esc_hint = match o.kind {
                OverlayKind::Threads => "Esc → workspaces",
                OverlayKind::Workspaces => "Esc quit",
                OverlayKind::Models => "Esc close · ^P toggle",
                _ => "Esc close",
            };
            // Show the live search filter (the pickers are type-to-search).
            let search = if o.filter.is_empty() {
                if self.overlay_searchable() { " · type to search".to_string() } else { String::new() }
            } else {
                format!(" · search: {}", o.filter)
            };
            let list = List::new(items)
                .block(
                    TuiBlock::default()
                        .borders(Borders::ALL)
                        .title(format!(" {} ({count}) — Enter select · {esc_hint} · ^C quit{search} ", o.title))
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
                .highlight_symbol("➤ ");
            // ASCII banner (+ authenticated line) above the bottom-anchored picker.
            let top_h = area.height.saturating_sub(h);
            if top_h >= 3 {
                // Left indent + a blank top line so the art isn't in the corner.
                const LEFT: &str = "   ";
                let mut banner: Vec<Line<'static>> = vec![Line::default()];
                banner.extend(BANNER.lines().map(|l| {
                    Line::from(Span::styled(format!("{LEFT}{l}"), Style::default().fg(Color::Cyan)))
                }));
                if !self.auth_line.is_empty() {
                    banner.push(Line::from(Span::styled(
                        format!("{LEFT}{}", self.auth_line),
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    )));
                }
                // On the thread picker, show which workspace these threads
                // belong to, so the user knows where they are.
                if o.kind == OverlayKind::Threads {
                    let ws = self.workspace_name.as_deref().unwrap_or("(no workspace — all threads)");
                    banner.push(Line::from(Span::styled(
                        format!("{LEFT}⊞ workspace: {ws}"),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )));
                }
                // On the model picker, show the currently active model.
                if o.kind == OverlayKind::Models {
                    let name = crate::api::model_name(self.current_model);
                    banner.push(Line::from(Span::styled(
                        format!("{LEFT}⚙ current model: {name}"),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )));
                }
                f.render_widget(
                    Paragraph::new(banner),
                    Rect { x: area.x, y: area.y, width: area.width, height: top_h },
                );
            }
            let mut s = state;
            f.render_widget(Clear, oarea);
            f.render_stateful_widget(list, oarea, &mut s);
        }
    }

    fn draw_tasks_panel(&self, f: &mut Frame, area: Rect) {
        let inner_w = area.width.saturating_sub(2) as usize;
        let items: Vec<ListItem> = self.tasks.iter().map(|task| {
            let icon = task.status.icon();
            let col  = task.status.color();
            let title = truncate(&task.title, inner_w.saturating_sub(3));
            ListItem::new(Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(col).add_modifier(Modifier::BOLD)),
                Span::styled(title.to_string(), Style::default().fg(col)),
            ]))
        }).collect();

        let panel = List::new(items)
            .block(TuiBlock::default()
                .title(Span::styled(" Tasks ", Style::default().fg(Color::DarkGray)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(panel, area);
    }

    fn draw_hint(&self, f: &mut Frame, area: Rect) {
        let text = if self.overlay.is_some() {
            let back = match self.overlay_kind() {
                Some(OverlayKind::Threads) => "Esc → workspaces",
                Some(OverlayKind::Workspaces) => "Esc → quit",
                _ => "Esc close",
            };
            let search = if self.overlay_searchable() { "type to search · " } else { "" };
            format!("  {search}↑/↓ move · Enter select · {back} · ^C quit")
        } else if self.pending.is_some() {
            "  type your answer · Enter submit".to_string()
        } else {
            let star = if self.has_workspace { "" } else { "*" };
            // ^F/^A are always offered; without a bound workspace they open the
            // workspace picker first (marked with *).
            if self.select_mode {
                "  SELECT MODE — drag to select text, Cmd-C to copy · ^S exit select mode".to_string()
            } else {
                format!("  ^W workspaces · ^O threads · ^P model · ^F findings{star} · ^A approvals{star} · ^L files · ^E open · ^Y copy · ^T thinking · ^R md · ^S select · Esc back · ^C quit")
            }
        };
        f.render_widget(
            Paragraph::new(win_safe(&text).into_owned()).style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }

    fn draw_transcript(&mut self, f: &mut Frame, area: Rect) {
        // 1-char left/right indent so messages don't run to the terminal edge.
        const PAD_X: u16 = 1;
        const PAD_Y: u16 = 1;
        let lines = self.build_lines();
        // Only top + bottom bars (no left/right borders) for a cleaner, wider
        // transcript; horizontal padding keeps text off the edges.
        let mut block = TuiBlock::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .title(" transcript ")
            .border_style(Style::default().fg(Color::Cyan))
            .padding(Padding::new(PAD_X, PAD_X, PAD_Y, PAD_Y));
        // Top-right: where work runs — workspace · current task.
        let mut loc: Vec<String> = Vec::new();
        if let Some(ws) = &self.workspace_name {
            loc.push(format!("⊞ {}", truncate(ws, 28)));
        }
        if let Some(t) = &self.task_label {
            loc.push(format!("▷ {}", truncate(t, 32)));
        }
        if !loc.is_empty() {
            block = block.title_top(
                Line::from(Span::styled(
                    format!(" {} ", loc.join("  ·  ")),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ))
                .right_aligned(),
            );
        }
        // No side borders → only the horizontal padding eats into the width.
        let inner_w = area.width.saturating_sub(2 * PAD_X);
        let inner_h = area.height.saturating_sub(2 + 2 * PAD_Y);

        let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
        let total = para.line_count(inner_w) as u16;
        self.max_scroll = total.saturating_sub(inner_h);
        // Auto-scroll to the bottom only when following AND the user hasn't
        // pinned the view by scrolling up (so streaming output never yanks them
        // back down mid-read).
        if self.follow && !self.pinned {
            self.scroll = self.max_scroll;
        } else if self.scroll > self.max_scroll {
            self.scroll = self.max_scroll;
        }
        f.render_widget(para.scroll((self.scroll, 0)), area);
    }

    // ---- multiline input editor ----------------------------------------

    /// Insert a char at the cursor and step past it.
    pub fn input_insert_char(&mut self, c: char) {
        self.input.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
    }

    /// Insert a newline (Alt/Shift+Enter) for composing a multiline message.
    pub fn input_newline(&mut self) {
        self.input.insert(self.input_cursor, '\n');
        self.input_cursor += 1;
    }

    /// Delete the char before the cursor.
    pub fn input_backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let prev = self.input[..self.input_cursor].chars().next_back().unwrap();
        self.input_cursor -= prev.len_utf8();
        self.input.remove(self.input_cursor);
    }

    /// Delete the char at the cursor (Del key).
    pub fn input_delete(&mut self) {
        if self.input_cursor < self.input.len() {
            self.input.remove(self.input_cursor);
        }
    }

    pub fn input_left(&mut self) {
        if self.input_cursor > 0 {
            let prev = self.input[..self.input_cursor].chars().next_back().unwrap();
            self.input_cursor -= prev.len_utf8();
        }
    }

    pub fn input_right(&mut self) {
        if self.input_cursor < self.input.len() {
            let next = self.input[self.input_cursor..].chars().next().unwrap();
            self.input_cursor += next.len_utf8();
        }
    }

    fn line_start(&self, pos: usize) -> usize {
        self.input[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self, pos: usize) -> usize {
        self.input[pos..].find('\n').map(|i| pos + i).unwrap_or(self.input.len())
    }

    pub fn input_home(&mut self) {
        self.input_cursor = self.line_start(self.input_cursor);
    }

    pub fn input_end(&mut self) {
        self.input_cursor = self.line_end(self.input_cursor);
    }

    /// Byte index `col` chars into the line `[start, end]`, clamped to `end`.
    fn col_to_byte(&self, start: usize, end: usize, col: usize) -> usize {
        match self.input[start..end].char_indices().nth(col) {
            Some((b, _)) => start + b,
            None => end,
        }
    }

    /// Move the cursor up one line (keeping its column). Returns false when
    /// already on the first line, so the caller can scroll the transcript.
    pub fn input_up(&mut self) -> bool {
        let ls = self.line_start(self.input_cursor);
        if ls == 0 {
            return false;
        }
        let col = self.input[ls..self.input_cursor].chars().count();
        let prev_start = self.line_start(ls - 1);
        self.input_cursor = self.col_to_byte(prev_start, ls - 1, col);
        true
    }

    /// Move the cursor down one line. Returns false when on the last line.
    pub fn input_down(&mut self) -> bool {
        let le = self.line_end(self.input_cursor);
        if le == self.input.len() {
            return false;
        }
        let ls = self.line_start(self.input_cursor);
        let col = self.input[ls..self.input_cursor].chars().count();
        let next_start = le + 1;
        let next_end = self.line_end(next_start);
        self.input_cursor = self.col_to_byte(next_start, next_end, col);
        true
    }

    /// Clear the input and reset the cursor (used on send and thread switch).
    pub fn input_clear(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
    }

    /// Text width available per visual row: terminal width minus the input
    /// box borders (2), horizontal padding (2) and the 2-col prompt gutter.
    fn input_text_width(area_width: u16) -> usize {
        (area_width as usize).saturating_sub(6).max(1)
    }

    /// Hard-wrap the input into visual rows of at most `tw` chars (also breaking
    /// on '\n'). Returns each row's byte span plus the cursor's (row, col) so
    /// the box can size itself and place the cursor exactly — long lines wrap
    /// instead of being clipped.
    fn input_wrap(&self, tw: usize) -> (Vec<(usize, usize)>, u16, u16) {
        let tw = tw.max(1);
        let mut rows: Vec<(usize, usize)> = Vec::new();
        let mut row_start = 0usize;
        let mut chars_in_row = 0usize;
        let mut byte = 0usize;
        for ch in self.input.chars() {
            if ch == '\n' {
                rows.push((row_start, byte));
                byte += 1;
                row_start = byte;
                chars_in_row = 0;
            } else {
                if chars_in_row == tw {
                    rows.push((row_start, byte));
                    row_start = byte;
                    chars_in_row = 0;
                }
                byte += ch.len_utf8();
                chars_in_row += 1;
            }
        }
        rows.push((row_start, byte));
        // Cursor sits on the last row whose start is at or before it (so a
        // position exactly on a wrap boundary lands at col 0 of the next row).
        let cur = self.input_cursor.min(self.input.len());
        let r = rows.iter().rposition(|&(s, _)| s <= cur).unwrap_or(0);
        let col = self.input[rows[r].0..cur].chars().count() as u16;
        (rows, r as u16, col)
    }

    /// Number of visual rows the message occupies at the given text width.
    fn input_visual_rows(&self, tw: usize) -> u16 {
        self.input_wrap(tw).0.len() as u16
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let (title, color) = if let Some(label) = self.pending_label() {
            (format!(" answer: {label} (Enter to submit) "), Color::Cyan)
        } else if self.running {
            (" message (Enter send · ^J newline · ^C cancels) ".to_string(), Color::Yellow)
        } else {
            (" message (Enter send · ^J newline) ".to_string(), Color::Green)
        };
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(color))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);

        // Pre-wrapped visual rows: the first carries the "› " prompt, the rest a
        // 2-space hang so wrapped/continuation text stays aligned under it.
        let tw = Self::input_text_width(area.width);
        let (rows, crow, ccol) = self.input_wrap(tw);
        let lines: Vec<Line<'static>> = rows
            .iter()
            .enumerate()
            .map(|(i, &(s, e))| {
                let prefix = if i == 0 { "› " } else { "  " };
                Line::from(win_safe(&format!("{prefix}{}", &self.input[s..e])).into_owned())
            })
            .collect();

        // Scroll so the cursor's row stays visible when the message is taller
        // than the (capped) box.
        let vis = inner.height.max(1);
        let scroll = (crow + 1).saturating_sub(vis);
        f.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);

        // Place the real terminal cursor (prompt/hang prefix is 2 cols wide).
        let cx = (inner.x + 2 + ccol).min(inner.x + inner.width.saturating_sub(1));
        let cy = inner.y + crow.saturating_sub(scroll);
        f.set_cursor_position(ratatui::layout::Position { x: cx, y: cy });
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let dot = if self.connected { "●" } else { "○" };
        let chat = if self.title.is_empty() {
            format!("thread {}", short(&self.thread_id))
        } else {
            truncate(&self.title, 48)
        };
        // Live credit utilization: the current run's usage while it streams,
        // plus the running session total. Pinned to the right so it is never
        // clipped by a long thread title.
        let mut credit_parts: Vec<String> = Vec::new();
        if self.running && self.run_credits > 0.0 {
            credit_parts.push(format!("◈ {:.3} cr · {}", self.run_credits, fmt_tokens(self.run_tokens)));
        }
        // Lifetime usage for this chat (⛁), kept live as runs complete.
        if self.thread_credits > 0.0 {
            credit_parts.push(format!("⛁ {:.3} cr · {}", self.thread_credits, fmt_tokens(self.thread_tokens)));
        }
        let credits = credit_parts.join("  ");

        // Working spinner while a turn is running (covers tool/http waits).
        const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let status = if self.running {
            let el = self
                .run_started
                .map(|t| format!("  {}", fmt_elapsed(t.elapsed())))
                .unwrap_or_default();
            format!("{} {}{el}", SPIN[self.spinner % SPIN.len()], self.status)
        } else {
            self.status.clone()
        };
        let model_label = crate::api::model_name(self.current_model);
        let select_indicator = if self.select_mode { "  ⊙ SELECT" } else { "" };
        let left = format!(
            " {dot} {chat}  ·  {status}  ·  md:{}  think:{}  ⚙ {}{}",
            if self.markdown { "on" } else { "off" },
            if self.show_thinking { "on" } else { "off" },
            model_label,
            select_indicator,
        );
        let style = Style::default().fg(Color::Black).bg(Color::Cyan);

        if credits.is_empty() {
            let max = area.width as usize;
            let text: String = left.chars().take(max).collect();
            f.render_widget(Paragraph::new(win_safe(&text).into_owned()).style(style), area);
            return;
        }
        // Reserve the right side for credits; clip only the left (title) side.
        // (win_safe maps each glyph 1:1 to a char, so widths are unchanged.)
        let cr = format!("{credits} ");
        let cr_w = cr.chars().count() as u16 + 1;
        let parts = Layout::horizontal([Constraint::Min(1), Constraint::Length(cr_w)]).split(area);
        let left_max = parts[0].width as usize;
        let left_txt: String = left.chars().take(left_max).collect();
        f.render_widget(Paragraph::new(win_safe(&left_txt).into_owned()).style(style), parts[0]);
        f.render_widget(
            Paragraph::new(win_safe(&cr).into_owned()).style(style).alignment(ratatui::layout::Alignment::Right),
            parts[1],
        );
    }

    // ---- input ----------------------------------------------------------

    pub fn page(&mut self, up: bool, h: u16) {
        self.scroll_lines(up, h.max(1));
    }

    pub fn scroll_line(&mut self, up: bool) {
        self.scroll_lines(up, 1);
    }

    /// Scroll the transcript by `n` lines. Scrolling up pins the view (no more
    /// auto-scroll); returning to the very bottom un-pins so new output streams
    /// into view again.
    pub fn scroll_lines(&mut self, up: bool, n: u16) {
        if up {
            self.follow = false;
            self.pinned = true;
            self.scroll = self.scroll.saturating_sub(n);
        } else {
            self.scroll = (self.scroll + n).min(self.max_scroll);
            if self.scroll >= self.max_scroll {
                self.follow = true;
                self.pinned = false;
            }
        }
    }

    /// Whether the user has scrolled up and pinned the transcript.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Jump back to the bottom and resume following live output.
    pub fn jump_to_bottom(&mut self) {
        self.pinned = false;
        self.follow = true;
        self.scroll = self.max_scroll;
    }

    /// Seed the transcript with prior conversation fetched on chat start.
    pub fn seed_history(&mut self, messages: Vec<crate::api::HistMsg>) {
        if messages.is_empty() {
            return;
        }
        self.rule("history");
        for m in messages {
            if m.text.trim().is_empty() {
                continue;
            }
            match m.author.as_str() {
                "user" => self.echo_user(&m.text),
                "agent" | "orchestrator" => {
                    self.flush_stream();
                    let agent = match m.author.as_str() {
                        "orchestrator" => "Strobes AI Supervisor".to_string(),
                        other => other.to_string(),
                    };
                    self.blocks.push(Block::Assistant { agent, md: m.text });
                }
                other => self.sys(&format!("· {other}: {}", m.text)),
            }
        }
        self.rule("live");
        self.follow = true;
    }

    /// Seed the transcript from full-fidelity persisted events (ordered by seq):
    /// user/agent messages, tool calls + results, tasks, thinking.
    pub fn seed_history_events(&mut self, events: Vec<Value>) {
        if events.is_empty() {
            return;
        }
        self.rule("history");
        for e in &events {
            let etype = e.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let p = e.get("payload").cloned().unwrap_or(Value::Null);
            let pstr = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let actor = e.get("actor").and_then(|a| a.as_str()).unwrap_or("");
            let agent = e.get("agentName").and_then(|a| a.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "Strobes AI Supervisor".into());

            match etype {
                "message.created" if actor == "user" => {
                    let t = pstr("text");
                    if !t.trim().is_empty() {
                        self.echo_user(&t);
                    }
                }
                "message.segment.completed" => {
                    self.push_assistant(agent, pstr("text"));
                }
                "thinking.completed" => {
                    self.push_thinking(pstr("text"));
                }
                "tool.start" => {
                    // Historical calls are already complete → Done (green dot);
                    // a following tool.failed downgrades it to Failed.
                    let name = pstr("toolName");
                    let args = compact_json(p.get("arguments"), 120);
                    self.flush_stream();
                    self.blocks.push(Block::Tool { name, detail: args, status: ToolStatus::Done });
                }
                "tool.output" => {
                    let dur = p.get("durationMs").and_then(|d| d.as_i64())
                        .map(|d| format!("  ·{d}ms")).unwrap_or_default();
                    let res = compact_json(p.get("result"), 220);
                    let body = if res.is_empty() { "(ok)".to_string() } else { res };
                    self.push(Line::from(Span::styled(
                        format!("  ⎿ {body}{dur}"), Style::default().fg(Color::DarkGray))));
                }
                "tool.failed" => {
                    self.finish_last_tool(false);
                    self.push(Line::from(Span::styled(
                        format!("  ⎿ ✗ {}", pstr("error")),
                        Style::default().fg(Color::Red))));
                }
                t if t.starts_with("task.") => {
                    let title = pstr("title");
                    let task_id = e.get("taskId").and_then(|v| v.as_str()).map(|s| s.to_string());
                    let hist_status = match t {
                        "task.started"   => TaskStatus::Running,
                        "task.completed" => TaskStatus::Completed,
                        "task.failed"    => TaskStatus::Failed,
                        _                => TaskStatus::Created,
                    };
                    // Upsert into the tasks panel.
                    if let Some(ref id) = task_id {
                        if let Some(existing) = self.tasks.iter_mut().find(|t| &t.id == id) {
                            existing.status = hist_status;
                            if !title.trim().is_empty() {
                                existing.title = title.clone();
                            }
                        } else if !title.trim().is_empty() {
                            self.tasks.push(Task {
                                id: id.clone(),
                                title: title.clone(),
                                status: hist_status,
                                agent: e.get("agentName").and_then(|v| v.as_str()).map(|s| s.to_string()),
                            });
                        }
                    }
                    // Inline marker only for task.created events.
                    if t == "task.created" && !title.trim().is_empty()
                        && self.last_task.as_deref() != Some(title.as_str())
                    {
                        self.flush_stream();
                        self.blocks.push(Block::Plain(Line::from(Span::styled(
                            format!("◇ {title}"),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM),
                        ))));
                        self.last_task = Some(title);
                        self.last_tool = None;
                    }
                }
                "plan.updated" => {
                    // Batch task upserts from workspace_add_tasks — panel only, no inline markers.
                    if let Some(tasks) = p.get("tasks").and_then(|t| t.as_array()) {
                        for task in tasks {
                            let tid = task.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                            let title = task.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            if title.is_empty() { continue; }
                            let status_str = task.get("status").and_then(|v| v.as_str()).unwrap_or("pending");
                            let hist_status = match status_str {
                                "running"   => TaskStatus::Running,
                                "completed" => TaskStatus::Completed,
                                "failed"    => TaskStatus::Failed,
                                _           => TaskStatus::Created,
                            };
                            if let Some(ref id) = tid {
                                if let Some(existing) = self.tasks.iter_mut().find(|t| &t.id == id) {
                                    existing.status = hist_status;
                                    if !title.is_empty() { existing.title = title.clone(); }
                                } else {
                                    self.tasks.push(Task {
                                        id: id.clone(),
                                        title,
                                        status: hist_status,
                                        agent: task.get("agentName").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                    });
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.rule("live");
        self.follow = true;
    }

    /// Show that a run was already in progress when the session opened.
    pub fn note_active_run(&mut self, status: &str) {
        // Only set running=true for statuses that mean the run is genuinely still executing.
        // "completed" / "failed" runs are returned by the history endpoint as the most recent
        // run — treating them as active causes the TUI to be permanently stuck in "running".
        let in_progress = matches!(status, "running" | "pending" | "queued" | "in_progress");
        if in_progress {
            self.running = true;
            self.status = format!("run {status} (in progress)");
            self.push(Line::from(Span::styled(
                format!("▶ a run is already {status} — streaming live updates…"),
                Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
            )));
        }
    }

    // ---- overlays (workspaces / threads / findings / approvals) --------

    pub fn open_overlay(&mut self, kind: OverlayKind, title: String, items: Vec<OverlayItem>) {
        self.overlay = Some(Overlay {
            kind, title, items, sel: 0, detail_open: false, dscroll: 0, filter: String::new(),
        });
    }

    /// Append a char to the overlay's type-to-search filter (resets cursor).
    pub fn overlay_filter_push(&mut self, c: char) {
        if let Some(o) = &mut self.overlay {
            o.filter.push(c);
            o.sel = 0;
        }
    }

    /// Remove the last char from the overlay's search filter (resets cursor).
    pub fn overlay_filter_pop(&mut self) {
        if let Some(o) = &mut self.overlay {
            o.filter.pop();
            o.sel = 0;
        }
    }

    /// Whether the active overlay supports type-to-search (the pickers do).
    pub fn overlay_searchable(&self) -> bool {
        matches!(
            self.overlay.as_ref().map(|o| o.kind),
            Some(OverlayKind::Threads) | Some(OverlayKind::Workspaces) | Some(OverlayKind::Models)
        )
    }

    pub fn overlay_active(&self) -> bool {
        self.overlay.is_some()
    }

    pub fn overlay_kind(&self) -> Option<OverlayKind> {
        self.overlay.as_ref().map(|o| o.kind)
    }

    pub fn overlay_detail_open(&self) -> bool {
        self.overlay.as_ref().map(|o| o.detail_open).unwrap_or(false)
    }

    pub fn close_overlay(&mut self) {
        self.overlay = None;
    }

    pub fn set_title(&mut self, title: String) {
        self.title = title;
    }

    /// Name of the bound workspace (shown top-right in the transcript).
    pub fn set_workspace_name(&mut self, name: String) {
        self.workspace_name = if name.is_empty() { None } else { Some(name) };
    }

    /// Seed the current thread's lifetime credit usage (from the credits API).
    pub fn set_thread_credits(&mut self, credits: f64, tokens: i64) {
        self.thread_credits = credits;
        self.thread_tokens = tokens;
    }

    /// Set the AI model for this session (None = org default).
    pub fn set_model(&mut self, model: Option<i64>) {
        self.current_model = model;
    }

    /// The currently selected model id (None means org default).
    pub fn current_model(&self) -> Option<i64> {
        self.current_model
    }

    /// Advance the working-spinner animation (called on a timer while running).
    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
    }

    /// Plain-text dump of the whole conversation (for ^Y → clipboard).
    pub fn transcript_plaintext(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            match b {
                Block::User(t) => out.push_str(&format!("You: {t}\n")),
                Block::Assistant { agent, md } => out.push_str(&format!("{agent}: {md}\n")),
                Block::Thinking(t) => out.push_str(&format!("[thinking] {t}\n")),
                Block::Plain(line) => {
                    for sp in &line.spans {
                        out.push_str(&sp.content);
                    }
                    out.push('\n');
                }
                Block::Tool { name, detail, .. } => {
                    if detail.is_empty() {
                        out.push_str(&format!("⏺ {name}\n"));
                    } else {
                        out.push_str(&format!("⏺ {name}({detail})\n"));
                    }
                }
                Block::Rule(t) => out.push_str(&format!("{t}\n")),
            }
        }
        if let Some(s) = &self.stream {
            out.push_str(&s.buf);
            out.push('\n');
        }
        out
    }

    // ---- slash-command autocomplete -----------------------------------

    pub fn set_slash_commands(&mut self, cmds: Vec<crate::api::SlashCmd>) {
        self.slash_commands = cmds;
    }

    /// The command-name prefix being typed (input is "/name…" with no space yet).
    fn slash_prefix(&self) -> Option<&str> {
        let t = self.input.strip_prefix('/')?;
        if t.contains(' ') {
            None // past the command name → arguments
        } else {
            Some(t)
        }
    }

    fn slash_matches(&self) -> Vec<usize> {
        let prefix = match self.slash_prefix() {
            Some(p) => p.to_lowercase(),
            None => return Vec::new(),
        };
        self.slash_commands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name.to_lowercase().starts_with(&prefix))
            .map(|(i, _)| i)
            .take(50)
            .collect()
    }

    pub fn slash_open(&self) -> bool {
        self.slash_prefix().is_some() && !self.slash_matches().is_empty()
    }

    pub fn slash_move(&mut self, up: bool) {
        let n = self.slash_matches().len();
        if n == 0 {
            return;
        }
        self.slash_sel = if up { (self.slash_sel + n - 1) % n } else { (self.slash_sel + 1) % n };
    }

    /// Complete the input to the selected command name (+ trailing space).
    pub fn slash_complete(&mut self) {
        let matches = self.slash_matches();
        if let Some(&idx) = matches.get(self.slash_sel.min(matches.len().saturating_sub(1))) {
            let name = self.slash_commands[idx].name.clone();
            self.input = format!("/{name} ");
            self.input_cursor = self.input.len();
            self.slash_sel = 0;
        }
    }

    fn draw_slash_popup(&self, f: &mut Frame, transcript_area: Rect) {
        let matches = self.slash_matches();
        if matches.is_empty() {
            return;
        }
        let shown = matches.len().min(10);
        let height = (shown as u16) + 2;
        let w = transcript_area.width;
        let area = Rect {
            x: transcript_area.x,
            y: transcript_area.y + transcript_area.height.saturating_sub(height),
            width: w,
            height: height.min(transcript_area.height),
        };
        let sel = self.slash_sel.min(matches.len().saturating_sub(1));
        let items: Vec<ListItem> = matches.iter().take(shown).map(|&i| {
            let c = &self.slash_commands[i];
            let hint = if c.argument_hint.is_empty() { String::new() } else { format!(" {}", c.argument_hint) };
            ListItem::new(Line::from(vec![
                Span::styled(format!("/{}", c.name), Style::default().fg(Color::Cyan)),
                Span::styled(hint, Style::default().fg(Color::DarkGray)),
                Span::styled(format!("  {}", truncate(&c.description, 60)), Style::default().fg(Color::Gray)),
            ]))
        }).collect();
        let mut state = ListState::default();
        state.select(Some(sel.min(shown.saturating_sub(1))));
        let list = List::new(items)
            .block(TuiBlock::default().borders(Borders::ALL)
                .title(format!(" /commands ({}) — Tab complete · Esc cancel ", matches.len()))
                .border_style(Style::default().fg(Color::Cyan)))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
            .highlight_symbol("➤ ");
        f.render_widget(ratatui::widgets::Clear, area);
        let mut s = state;
        f.render_stateful_widget(list, area, &mut s);
    }

    /// A non-error informational line (workspace bound, hints, etc.).
    pub fn notice(&mut self, text: &str) {
        self.push(Line::from(Span::styled(
            format!("  · {text}"),
            Style::default().fg(Color::Cyan),
        )));
    }

    /// Duration (seconds) the most recently finished run took, if known —
    /// used to decide whether to fire a "response done" notification.
    pub fn last_run_secs(&self) -> Option<u64> {
        self.last_elapsed.map(|d| d.as_secs())
    }

    pub fn overlay_move(&mut self, up: bool) {
        if let Some(o) = &mut self.overlay {
            if o.detail_open {
                o.dscroll = if up { o.dscroll.saturating_sub(1) } else { o.dscroll.saturating_add(1) };
            } else {
                let n = o.visible().len();
                if n > 0 {
                    o.sel = if up { (o.sel + n - 1) % n } else { (o.sel + 1) % n };
                }
            }
        }
    }

    pub fn overlay_page(&mut self, up: bool, h: u16) {
        if let Some(o) = &mut self.overlay {
            if o.detail_open {
                o.dscroll = if up { o.dscroll.saturating_sub(h) } else { o.dscroll.saturating_add(h) };
            }
        }
    }

    /// Handle Enter inside an overlay. For findings/approvals it toggles the
    /// detail view (returns None); for threads/workspaces it returns the chosen
    /// (kind, action-id) so the caller can act.
    pub fn overlay_enter(&mut self) -> Option<(OverlayKind, String)> {
        let o = self.overlay.as_mut()?;
        match o.kind {
            OverlayKind::Findings | OverlayKind::Approvals | OverlayKind::Files => {
                o.detail_open = !o.detail_open;
                o.dscroll = 0;
                None
            }
            OverlayKind::Threads | OverlayKind::Workspaces | OverlayKind::Models => {
                let idx = o.current()?;
                o.items.get(idx).and_then(|it| it.action.clone()).map(|a| (o.kind, a))
            }
        }
    }

    /// Esc inside an overlay: close the detail view first, else close overlay.
    /// Returns true if it consumed the key.
    pub fn overlay_esc(&mut self) -> bool {
        if let Some(o) = &mut self.overlay {
            if o.detail_open {
                o.detail_open = false;
                return true;
            }
            self.overlay = None;
            return true;
        }
        false
    }

    /// Clear the transcript for a fresh thread (used when switching threads).
    pub fn reset_for_thread(&mut self, thread_id: String) {
        self.blocks.clear();
        self.stream = None;
        self.pending = None;
        self.overlay = None;
        self.last_task = None;
        self.scroll = u16::MAX; // clamped to max_scroll on first draw
        self.follow = true;
        self.pinned = false;
        // Start each thread with an empty message box.
        self.input_clear();
        self.thread_id = thread_id;
        self.title = String::new();
        self.task_label = None;
        self.thread_credits = 0.0;
        self.thread_tokens = 0;
        self.tasks.clear();
        self.sys("Strobes Agents AI — Ratatui client");
    }

    pub fn echo_user(&mut self, text: &str) {
        self.flush_stream();
        self.blocks.push(Block::User(text.to_string()));
        self.last_task = None;
        self.last_tool = None;
        // Sending a message returns the user to the live bottom of the chat.
        self.follow = true;
        self.pinned = false;
        // Start the turn's elapsed timer at send (covers the wait before the
        // server's run.started arrives).
        self.run_started = Some(std::time::Instant::now());
        self.last_elapsed = None;
    }

    /// Push a dim labelled separator (run boundaries, history/live markers).
    pub fn rule(&mut self, label: &str) {
        self.flush_stream();
        self.blocks.push(Block::Rule(label.to_string()));
        self.last_task = None;
        self.last_tool = None;
        self.follow = true;
    }
}

fn agent_header(agent: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("◆ {agent}"),
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
    ))
}

/// Append a blank line unless the last line is already blank (avoid doubles).
fn blank(out: &mut Vec<Line<'static>>) {
    if out.last().map(|l| l.spans.is_empty()).unwrap_or(true) {
        return;
    }
    out.push(Line::default());
}

fn rule_line(label: &str) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    if label.is_empty() {
        Line::from(Span::styled("─".repeat(48), dim))
    } else {
        Line::from(vec![
            Span::styled("── ".to_string(), dim),
            Span::styled(label.to_string(), Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {}", "─".repeat(40usize.saturating_sub(label.len()))), dim),
        ])
    }
}

/// Render Markdown to fully-owned ('static) ratatui lines (markers stripped,
/// real bold/italic/code/heading/list styling). See `crate::markdown`.
fn md_to_owned(md: &str) -> Vec<Line<'static>> {
    crate::markdown::render(md)
}

fn short(id: &str) -> String {
    if id.len() > 8 { format!("{}…", &id[..8]) } else { id.to_string() }
}

fn fmt_tokens(t: i64) -> String {
    if t >= 1000 {
        format!("{:.1}k tok", t as f64 / 1000.0)
    } else {
        format!("{t} tok")
    }
}

/// Windows console renders many decorative glyphs as double-width while
/// ratatui counts them as one cell — that desyncs the grid and leaves ghost
/// text. Map the offenders to width-1 ASCII on Windows only.
#[cfg(windows)]
fn win_glyph(c: char) -> Option<&'static str> {
    Some(match c {
        '●' | '◆' | '⏺' | '◇' | '✻' | '◈' | '⛁' | '⊞' | '◐' | '★' => "*",
        '○' => "o",
        '⎿' => "L",
        '▷' | '➤' | '›' | '↪' => ">",
        '✔' => "+",
        '✗' => "x",
        '⚠' => "!",
        '➕' => "+",
        '⬆' => "^",
        '⟳' => "~",
        '·' => "-",
        _ => return None,
    })
}

/// Replace ambiguous-width glyphs with ASCII (Windows only; identity elsewhere).
#[allow(unused_variables)]
fn win_safe(s: &str) -> std::borrow::Cow<'_, str> {
    #[cfg(windows)]
    {
        if s.chars().any(|c| win_glyph(c).is_some()) {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match win_glyph(c) {
                    Some(rep) => out.push_str(rep),
                    None => out.push(c),
                }
            }
            return std::borrow::Cow::Owned(out);
        }
    }
    std::borrow::Cow::Borrowed(s)
}

#[cfg(windows)]
fn win_safe_line(line: &mut Line<'static>) {
    for span in line.spans.iter_mut() {
        if span.content.chars().any(|c| win_glyph(c).is_some()) {
            let replaced = win_safe(&span.content).into_owned();
            span.content = std::borrow::Cow::Owned(replaced);
        }
    }
}

/// Human elapsed time: 8s · 1:07 · 12:30.
fn fmt_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Compact a JSON value to a single-line, length-capped string.
pub fn compact_json(v: Option<&Value>, limit: usize) -> String {
    let s = match v {
        None | Some(Value::Null) => return String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    let s = s.replace('\n', " ");
    if s.chars().count() > limit {
        format!("{}…", s.chars().take(limit).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod md_tests {
    use super::md_to_owned;
    use ratatui::style::Modifier;

    #[test]
    fn renders_markdown_styles() {
        let lines = md_to_owned("# Title\n\nSome **bold** text and `code`.\n\n- one\n- two\n");
        assert!(!lines.is_empty(), "expected rendered lines");
        let any_bold = lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(any_bold, "expected at least one bold span from markdown");
        // The list items should survive as text somewhere.
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(joined.contains("one") && joined.contains("two"), "list text missing: {joined:?}");
        // Heading markers must be stripped (the tui-markdown bug we replaced).
        assert!(!joined.contains('#'), "heading '#' markers should be stripped: {joined:?}");
        assert!(joined.contains("Title"), "heading text missing: {joined:?}");
        // List items should be bulleted, not raw '-'.
        assert!(joined.contains('•'), "expected bullet marker: {joined:?}");
    }

    #[test]
    fn renders_table_and_heading_after_text() {
        let md = "Ready to use it for real? 🚀\n\n## Greeting\n\n| Field | Value |\n|---|---|\n| Target URL | https://test.com |\n| Scope | a |\n";
        let lines = crate::markdown::render(md);
        let joined: String = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        // Table cells must be separated, not run together.
        assert!(joined.contains("Target URL"), "table cell missing: {joined:?}");
        assert!(joined.contains('│'), "table column separator missing: {joined:?}");
        assert!(!joined.contains("Target URLhttps"), "table cells run together: {joined:?}");
        // Heading after text renders without the literal '##'.
        assert!(joined.contains("Greeting"), "heading missing: {joined:?}");
        assert!(!joined.contains("##"), "heading marker leaked: {joined:?}");
    }
}

#[cfg(test)]
mod input_editor_tests {
    use super::*;

    fn app() -> App {
        App::new("t".into(), "http://x".into(), "ten".into(), "org".into())
    }

    #[test]
    fn insert_and_horizontal_movement() {
        let mut a = app();
        for c in "hello".chars() {
            a.input_insert_char(c);
        }
        assert_eq!(a.input, "hello");
        assert_eq!(a.input_cursor, 5);
        a.input_left();
        a.input_left();
        assert_eq!(a.input_cursor, 3); // "hel|lo"
        a.input_insert_char('X');
        assert_eq!(a.input, "helXlo");
        assert_eq!(a.input_cursor, 4);
        a.input_home();
        assert_eq!(a.input_cursor, 0);
        a.input_end();
        assert_eq!(a.input_cursor, a.input.len());
    }

    #[test]
    fn backspace_and_delete_at_cursor() {
        let mut a = app();
        for c in "abc".chars() {
            a.input_insert_char(c);
        }
        a.input_left(); // "ab|c"
        a.input_backspace(); // remove 'b' -> "a|c"
        assert_eq!(a.input, "ac");
        assert_eq!(a.input_cursor, 1);
        a.input_delete(); // remove 'c' at cursor -> "a"
        assert_eq!(a.input, "a");
    }

    #[test]
    fn multiline_newline_and_vertical_nav() {
        let mut a = app();
        for c in "ab".chars() {
            a.input_insert_char(c);
        }
        a.input_newline();
        for c in "cde".chars() {
            a.input_insert_char(c);
        }
        assert_eq!(a.input, "ab\ncde");
        assert_eq!(a.input_cursor, 6);
        assert!(a.input_up());
        assert_eq!(a.input_cursor, 2); // column clamped to "ab"
        assert!(!a.input_up()); // first line → caller scrolls transcript
        assert_eq!(a.input_cursor, 2);
        assert!(a.input_down());
        assert_eq!(a.input_cursor, 5); // "cde" col 2
        assert!(!a.input_down()); // last line → caller scrolls transcript
    }

    #[test]
    fn long_lines_wrap_into_visual_rows() {
        let mut a = app();
        for c in "abcdef".chars() {
            a.input_insert_char(c);
        }
        // Width 3 → "abc" / "def"; cursor at end is row 1, col 3.
        let (rows, crow, ccol) = a.input_wrap(3);
        assert_eq!(rows.len(), 2);
        assert_eq!((crow, ccol), (1, 3));
        assert_eq!(a.input_visual_rows(3), 2);
        // A newline also forces a fresh visual row.
        a.input_clear();
        for c in "a\nb".chars() {
            a.input_insert_char(c);
        }
        assert_eq!(a.input_visual_rows(10), 2);
    }

    #[test]
    fn clear_resets_cursor() {
        let mut a = app();
        a.input_insert_char('x');
        a.input_clear();
        assert_eq!(a.input, "");
        assert_eq!(a.input_cursor, 0);
        assert_eq!(a.input_visual_rows(10), 1);
    }

    #[test]
    fn utf8_boundaries_are_respected() {
        let mut a = app();
        for c in "héllo".chars() {
            a.input_insert_char(c); // é is 2 bytes
        }
        a.input_left(); // before 'o'
        a.input_backspace(); // remove 'l' -> "hélo"
        assert_eq!(a.input, "hélo");
        a.input_home();
        a.input_right(); // after 'h'
        a.input_delete(); // remove 'é'
        assert_eq!(a.input, "hlo");
    }

    #[test]
    fn scrolling_up_pins_through_streaming() {
        let mut a = app();
        a.max_scroll = 100;
        a.scroll = 100;
        a.follow = true;

        // User scrolls up to read history.
        a.scroll_lines(true, 10);
        assert!(a.is_pinned());
        assert!(!a.follow);
        assert_eq!(a.scroll, 90);

        // Streaming output forces follow=true (as feed()/tool events do) — the
        // pin must keep the view from being yanked back to the bottom.
        a.follow = true;
        assert!(a.is_pinned(), "must stay pinned while reading during a run");

        // Returning to the bottom un-pins and resumes following.
        a.scroll_lines(false, 50); // clamps to max_scroll (bottom)
        assert!(!a.is_pinned());
        assert!(a.follow);

        // Esc/jump also clears the pin.
        a.scroll_lines(true, 5);
        assert!(a.is_pinned());
        a.jump_to_bottom();
        assert!(!a.is_pinned());
        assert!(a.follow);
    }
}
