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

/// One transcript entry.
enum Block {
    User(String),
    Assistant { agent: String, md: String },
    Thinking(String),
    Plain(Line<'static>),
    Rule(String),
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
    sel: usize,
    detail_open: bool,
    dscroll: u16,
}

pub struct App {
    blocks: Vec<Block>,
    stream: Option<Stream>,
    pub input: String,
    pub status: String,
    pub connected: bool,
    pub running: bool,
    pub show_thinking: bool,
    pub markdown: bool,
    pub scroll: u16,
    pub max_scroll: u16,
    pub follow: bool,
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
            status: "connecting…".into(),
            connected: false,
            running: false,
            show_thinking: false,
            markdown: true,
            scroll: 0,
            max_scroll: 0,
            follow: true,
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
        };
        // Banner (cyan) + the authenticated tenant on top.
        for line in BANNER.lines() {
            app.blocks.push(Block::Plain(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::Cyan),
            ))));
        }
        let org = if org_id.len() > 8 { &org_id[..8] } else { &org_id };
        app.auth_line = format!("✔ authenticated · tenant: {tenant} · {} · org {org}", app.base);
        app.blocks.push(Block::Plain(Line::from(Span::styled(
            format!("  {}", app.auth_line),
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
                self.status = format!("disconnected — {why}");
            }
            AppEvent::RunStarted => {
                self.running = true;
                self.status = "running…".into();
                self.run_credits = 0.0;
                self.run_tokens = 0;
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
                let mut spans = vec![Span::styled(
                    format!("⏺ {name}"),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )];
                if !detail.is_empty() {
                    spans.push(Span::styled(format!("({detail})"), Style::default().fg(Color::DarkGray)));
                }
                let line = Line::from(spans);
                if self.last_tool.as_deref() == Some(&name) {
                    if let Some(Block::Plain(l)) = self.blocks.last_mut() {
                        *l = line; // replace the empty/earlier start for the same call
                    } else {
                        self.blocks.push(Block::Plain(line));
                    }
                } else {
                    self.flush_stream();
                    self.blocks.push(Block::Plain(line));
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
                self.blocks.push(Block::Plain(Line::from(Span::styled(
                    format!("  ⎿ {body}{ms}"),
                    Style::default().fg(Color::DarkGray),
                ))));
                self.last_tool = None;
                self.follow = true;
            }
            "tool_failed" => {
                self.flush_stream();
                self.blocks.push(Block::Plain(Line::from(Span::styled(
                    format!("  ⎿ ✗ {}", truncate(&item.detail.unwrap_or_default(), 220)),
                    Style::default().fg(Color::Red),
                ))));
                self.last_tool = None;
                self.follow = true;
            }
            "task" => {
                // Subtle, deduped by title (ignore started/completed churn).
                let title = item.text.unwrap_or_default();
                if title.trim().is_empty() || self.last_task.as_deref() == Some(&title) {
                    return;
                }
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
                    // Collapsed: a live "thinking…" indicator with elapsed time.
                    let el = self
                        .run_started
                        .map(|t| format!("  {}", fmt_elapsed(t.elapsed())))
                        .unwrap_or_default();
                    out.push(Line::from(Span::styled(
                        format!("✻ thinking…{el}"),
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                    )));
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
        let dim = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
        out.push(Line::from(Span::styled("✻ thinking".to_string(), dim)));
        for l in s.split('\n') {
            out.push(Line::from(Span::styled(format!("  {l}"), dim)));
        }
    }

    // ---- rendering ------------------------------------------------------

    pub fn draw(&mut self, f: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());
        if self.overlay.is_some() {
            // Close the chat view: show only the picker, anchored to the bottom
            // of the screen (just above the input bar).
            self.draw_overlay(f, chunks[0]);
        } else {
            self.draw_transcript(f, chunks[0]);
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
                .items
                .get(o.sel)
                .map(|it| it.detail.iter().map(|l| Line::from(l.clone())).collect())
                .unwrap_or_default();
            let block = TuiBlock::default()
                .borders(Borders::ALL)
                .title(format!(" {} — detail (Esc back) ", o.title))
                .border_style(Style::default().fg(Color::Magenta));
            f.render_widget(
                Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((o.dscroll, 0)),
                area,
            );
        } else {
            let items: Vec<ListItem> = o
                .items
                .iter()
                .map(|it| ListItem::new(Line::from(it.label.clone())))
                .collect();
            let mut state = ListState::default();
            state.select(Some(o.sel));
            let count = o.items.len();
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
                _ => "Esc close",
            };
            let list = List::new(items)
                .block(
                    TuiBlock::default()
                        .borders(Borders::ALL)
                        .title(format!(" {} ({count}) — Enter view/select · {esc_hint} · ^C quit ", o.title))
                        .border_style(Style::default().fg(Color::Magenta)),
                )
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD))
                .highlight_symbol("➤ ");
            // ASCII banner (+ authenticated line) above the bottom-anchored picker.
            let top_h = area.height.saturating_sub(h);
            if top_h >= 3 {
                let mut banner: Vec<Line<'static>> = BANNER
                    .lines()
                    .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(Color::Cyan))))
                    .collect();
                if !self.auth_line.is_empty() {
                    banner.push(Line::from(Span::styled(
                        format!("  {}", self.auth_line),
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
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

    fn draw_hint(&self, f: &mut Frame, area: Rect) {
        let text = if self.overlay.is_some() {
            let back = match self.overlay_kind() {
                Some(OverlayKind::Threads) => "Esc → workspaces",
                Some(OverlayKind::Workspaces) => "Esc → quit",
                _ => "Esc close",
            };
            format!("  ↑/↓ move · Enter view/select · {back} · ^C quit")
        } else if self.pending.is_some() {
            "  type your answer · Enter submit".to_string()
        } else {
            let star = if self.has_workspace { "" } else { "*" };
            // ^F/^A are always offered; without a bound workspace they open the
            // workspace picker first (marked with *).
            format!("  ^W workspaces · ^O threads · ^F findings{star} · ^A approvals{star} · ^L files · ^E open · ^Y copy · ^T thinking · ^R md · Esc back · ^C quit")
        };
        f.render_widget(
            Paragraph::new(win_safe(&text).into_owned()).style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }

    fn draw_transcript(&mut self, f: &mut Frame, area: Rect) {
        // Inner padding so messages don't touch the borders.
        const PAD_X: u16 = 2;
        const PAD_Y: u16 = 1;
        let lines = self.build_lines();
        let mut block = TuiBlock::default()
            .borders(Borders::ALL)
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
        let inner_w = area.width.saturating_sub(2 + 2 * PAD_X);
        let inner_h = area.height.saturating_sub(2 + 2 * PAD_Y);

        let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
        let total = para.line_count(inner_w) as u16;
        self.max_scroll = total.saturating_sub(inner_h);
        if self.follow {
            self.scroll = self.max_scroll;
        } else if self.scroll > self.max_scroll {
            self.scroll = self.max_scroll;
        }
        f.render_widget(para.scroll((self.scroll, 0)), area);
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let (title, color) = if let Some(label) = self.pending_label() {
            (format!(" answer: {label} (Enter to submit) "), Color::Cyan)
        } else if self.running {
            (" message (running — Ctrl-C cancels) ".to_string(), Color::Yellow)
        } else {
            (" message ".to_string(), Color::Green)
        };
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(color))
            .padding(Padding::horizontal(1));
        f.render_widget(Paragraph::new(win_safe(&format!("› {}", self.input)).into_owned()).block(block), area);
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
        let left = format!(
            " {dot} {chat}  ·  {status}  ·  md:{}  think:{}",
            if self.markdown { "on" } else { "off" },
            if self.show_thinking { "on" } else { "off" },
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

    /// Scroll the transcript by `n` lines. Re-enables follow when scrolled to
    /// the very bottom so new output keeps streaming into view.
    pub fn scroll_lines(&mut self, up: bool, n: u16) {
        if up {
            self.follow = false;
            self.scroll = self.scroll.saturating_sub(n);
        } else {
            self.scroll = (self.scroll + n).min(self.max_scroll);
            self.follow = self.scroll >= self.max_scroll;
        }
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
                    let name = pstr("toolName");
                    let args = compact_json(p.get("arguments"), 120);
                    let mut spans = vec![Span::styled(
                        format!("⏺ {name}"),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))];
                    if !args.is_empty() {
                        spans.push(Span::styled(format!("({args})"), Style::default().fg(Color::DarkGray)));
                    }
                    self.push(Line::from(spans));
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
                    self.push(Line::from(Span::styled(
                        format!("  ⎿ ✗ {}", pstr("error")),
                        Style::default().fg(Color::Red))));
                }
                t if t.starts_with("task.") => {
                    let title = pstr("title");
                    if !title.trim().is_empty() && self.last_task.as_deref() != Some(title.as_str()) {
                        self.flush_stream();
                        self.blocks.push(Block::Plain(Line::from(Span::styled(
                            format!("◇ {title}"),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM),
                        ))));
                        self.last_task = Some(title);
                        self.last_tool = None;
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
        self.running = true;
        self.status = format!("run {status} (in progress)");
        self.push(Line::from(Span::styled(
            format!("▶ a run is already {status} — streaming live updates…"),
            Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
        )));
    }

    // ---- overlays (workspaces / threads / findings / approvals) --------

    pub fn open_overlay(&mut self, kind: OverlayKind, title: String, items: Vec<OverlayItem>) {
        self.overlay = Some(Overlay { kind, title, items, sel: 0, detail_open: false, dscroll: 0 });
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

    pub fn overlay_move(&mut self, up: bool) {
        if let Some(o) = &mut self.overlay {
            if o.detail_open {
                o.dscroll = if up { o.dscroll.saturating_sub(1) } else { o.dscroll.saturating_add(1) };
            } else if !o.items.is_empty() {
                let n = o.items.len();
                o.sel = if up { (o.sel + n - 1) % n } else { (o.sel + 1) % n };
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
            OverlayKind::Threads | OverlayKind::Workspaces => {
                o.items.get(o.sel).and_then(|it| it.action.clone()).map(|a| (o.kind, a))
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
        self.scroll = 0;
        self.follow = true;
        self.thread_id = thread_id;
        self.title = String::new();
        self.task_label = None;
        self.thread_credits = 0.0;
        self.thread_tokens = 0;
        self.sys("Strobes Agents AI — Ratatui client");
    }

    pub fn echo_user(&mut self, text: &str) {
        self.flush_stream();
        self.blocks.push(Block::User(text.to_string()));
        self.last_task = None;
        self.last_tool = None;
        self.follow = true;
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
fn compact_json(v: Option<&Value>, limit: usize) -> String {
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
