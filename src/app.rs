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
use std::collections::HashMap;

use crate::pulse::{AppEvent, Field, StreamItem};

/// Status of a tracked agent task.
///
/// `Blocked`/`Inconclusive` exist so a future backend fix for strobes-co/strobes#9203
/// (orchestrator currently collapses these into "Completed") has somewhere to land —
/// without them, an unrecognized status string falls through to `Created`, which
/// reads as "not started yet" rather than "ran but didn't reach a real result".
#[derive(Clone, Copy, PartialEq)]
pub enum TaskStatus {
    Created,
    Running,
    Completed,
    Failed,
    Blocked,
    Inconclusive,
}

impl TaskStatus {
    fn icon(self) -> &'static str {
        match self {
            TaskStatus::Created      => "○",
            TaskStatus::Running      => "⟳",
            TaskStatus::Completed    => "✓",
            TaskStatus::Failed       => "✗",
            TaskStatus::Blocked      => "⊘",
            TaskStatus::Inconclusive => "◐",
        }
    }
    fn color(self) -> Color {
        match self {
            TaskStatus::Created      => Color::DarkGray,
            TaskStatus::Running      => Color::Cyan,
            TaskStatus::Completed    => Color::Green,
            TaskStatus::Failed       => Color::Red,
            TaskStatus::Blocked      => Color::Yellow,
            TaskStatus::Inconclusive => Color::Magenta,
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
/// Max lines of a code/command tool call shown before a "ctrl+o" hint.
const CODE_PREVIEW_LINES: usize = 5;
/// Lines of a tool's output shown under the call before "+N lines".
const RESULT_PREVIEW_LINES: usize = 3;

enum Block {
    User(String),
    Assistant { agent: String, md: String },
    Thinking(String),
    Plain(Line<'static>),
    Rule(String),
    /// A tool call line ("⏺ name(args)") with a live status: the dot blinks
    /// while Running, then turns green (Done) or red (Failed). `agent` is
    /// which agent made the call — needed because a supervisor dispatching
    /// to a sub-agent means two tool calls can be in flight at once, and
    /// only matching by tool NAME (as this used to) let one agent's
    /// completion event settle a same-named call that actually belonged to
    /// the other, leaving the real match stuck Running (blinking) forever.
    Tool { name: String, detail: String, status: ToolStatus, agent: String },
    /// The agent's self-managed to-do list, rendered as a checklist. Each
    /// add/complete/delete finds and updates THIS agent's existing Todos
    /// block in place rather than appending a fresh one — a checklist
    /// filled in one item at a time used to leave one full-list snapshot
    /// per item, so a 5-item list rendered as 5 growing, near-duplicate
    /// checklists stacked on top of each other.
    Todos { agent: String, items: Vec<TodoItem> },
    /// A tool call's result, shown as "  ⎿  first lines…" under the call.
    ToolResult { text: String, failed: bool },
}

#[derive(Clone)]
struct TodoItem {
    id: Option<String>,
    content: String,
    status: String, // "pending" | "in_progress" | "completed" | anything else
}

impl TodoItem {
    fn icon_and_color(&self) -> (&'static str, Color) {
        match self.status.as_str() {
            "completed" => ("✓", Color::Green),
            "in_progress" => ("⟳", Color::Cyan),
            "pending" => ("○", Color::DarkGray),
            _ => ("✗", Color::Red),
        }
    }

    /// The sub-agent scratchpad tool (`todo`) shape: `{id, text, done}`.
    fn from_text_done(v: &Value) -> Option<Self> {
        let content = v.get("text").and_then(|t| t.as_str())?.to_string();
        let done = v.get("done").and_then(|d| d.as_bool()).unwrap_or(false);
        Some(TodoItem {
            id: json_id_string(v.get("id")),
            content,
            status: if done { "completed".into() } else { "pending".into() },
        })
    }

    /// The orchestrator's Agent-Tables-backed tools (write_todos, add_todo,
    /// update_todo_status, list_todos) shape: `{id, content|title, status}`.
    fn from_content_status(v: &Value) -> Option<Self> {
        let content = v
            .get("content")
            .or_else(|| v.get("title"))
            .and_then(|c| c.as_str())?
            .to_string();
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("pending").to_string();
        Some(TodoItem { id: json_id_string(v.get("id")), content, status })
    }
}

/// A todo/item id may come back as a JSON string (`"todo_abc123"`) or a
/// number (the sub-agent tool's `id`) — normalize either to a String so the
/// two todo systems' ids can be compared uniformly.
fn json_id_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Tool names that manage a todo checklist — either the per-sub-agent
/// scratchpad (`todo`, in subagent_session_tools.py) or the orchestrator's
/// Agent-Tables-backed suite (todo_tools.py). `tool_start` suppresses these
/// entirely (no generic "⏺ tool(...)" line); `tool_output` renders a
/// checklist snapshot from the result instead — see `App::apply_todo_result`.
fn is_todo_tool(name: &str) -> bool {
    matches!(
        name,
        "todo" | "write_todos" | "add_todo" | "update_todo_status" | "list_todos"
            | "remove_todo" | "clear_todos"
    )
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
    /// When true, code/command tool calls render their full source; otherwise
    /// they show up to CODE_PREVIEW_LINES with a "ctrl+o" hint. Toggled by ^O.
    pub show_full_code: bool,
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
    /// (agent, tool name) of the most recent tool_start seen, so a repeated
    /// "empty start then detailed start" pair for the SAME call enriches in
    /// place — scoped by agent so a different agent's call with the same
    /// tool name never gets merged into it.
    last_tool: Option<(String, String)>,
    /// Current todo-checklist state per agent (each sub-agent's `todo`
    /// scratchpad is independent, per subagent_session_tools.py's session
    /// key) — updated incrementally as add/complete/delete/list results
    /// stream in, so each one can render a fresh full-list snapshot without
    /// re-deriving history from the transcript.
    todo_state: HashMap<String, Vec<TodoItem>>,
    /// Set while waiting on a network round-trip that isn't an agent turn
    /// (opening the threads/workspaces overlay, switching threads, etc.) —
    /// shown as a spinner + message in the status bar so a genuine slow
    /// network request reads as "working", not "frozen". `running` covers
    /// the agent-turn case already; this covers everything else that used
    /// to give no feedback at all while it awaited a response.
    busy: Option<String>,
    /// When set, a centered animated "Loading…" overlay is drawn on top of the
    /// chat UI (for in-chat fetches like opening the Threads/Workspaces overlay
    /// or switching threads). Distinct from `busy`, which is the bottom
    /// status-bar spinner used for lighter/inline waits and upload progress.
    loading: Option<String>,
    /// Per-appearance seed so the loader's flavor message varies each time.
    loading_seed: usize,
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
    /// Memoized `build_lines()` output — rebuilding re-parses every block's
    /// Markdown from scratch, which used to happen on EVERY draw (every
    /// keystroke, every 120ms spinner tick, every streamed token) regardless
    /// of whether the transcript actually changed. Invalidated by comparing
    /// `lines_fingerprint` against a cheap content hash each frame instead.
    lines_cache: Vec<Line<'static>>,
    lines_fingerprint: u64,
    /// `?` on an empty prompt shows the shortcuts panel under the input.
    pub show_help: bool,
    /// One-shot footer hint ("Press Ctrl-C again to exit", …) and when it
    /// was armed; it expires after a couple of seconds.
    armed: Option<(Armed, std::time::Instant)>,
    /// Previously sent prompts (oldest first) for ↑/↓ recall.
    history: Vec<String>,
    /// Position while browsing `history`, plus the draft it replaced.
    hist_pos: Option<(usize, String)>,
}

/// A key that needs a second press to act.
#[derive(Clone, Copy, PartialEq)]
pub enum Armed {
    Quit,
    ClearInput,
}

/// Client-side slash commands, handled by the CLI itself (never sent to the
/// agent). Listed first in the `/` menu.
pub const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("help", "Show commands and keyboard shortcuts"),
    ("new", "Start a new chat (alias /clear)"),
    ("resume", "Switch to another chat in this workspace"),
    ("workspace", "Switch or bind a workspace"),
    ("model", "Choose the AI model"),
    ("findings", "Browse this workspace's findings"),
    ("approvals", "Review pending approvals"),
    ("files", "Browse the workspace's local files"),
    ("copy", "Copy the transcript to the clipboard"),
    ("thinking", "Show or hide the agent's reasoning"),
    ("status", "Show tenant, workspace, chat and usage"),
    ("exit", "Quit (alias /quit)"),
];

/// Resolve a typed `/name` to a built-in command (aliases included).
pub fn builtin_command(name: &str) -> Option<&'static str> {
    let n = match name {
        "clear" | "reset" => "new",
        "quit" | "q" => "exit",
        "threads" | "chats" => "resume",
        "workspaces" | "ws" => "workspace",
        "models" => "model",
        "?" | "h" => "help",
        other => other,
    };
    BUILTIN_COMMANDS.iter().find(|(c, _)| *c == n).map(|(c, _)| *c)
}

fn history_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("strobes-ai").join("prompt_history"))
}

/// Prompts are stored one per line with `\n` escaped, newest last.
fn load_prompt_history() -> Vec<String> {
    history_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.lines().filter(|l| !l.is_empty()).map(|l| l.replace("\\n", "\n")).collect())
        .unwrap_or_default()
}

/// ASCII banner shown at the top of a fresh chat transcript and the pickers.
/// The subtitle carries the build version (via `concat!` + `env!`).
/// The Strobes "strobes" wordmark in white (+ ™) with a dim tagline underneath.
/// Each returned line is already indented by `indent`. Used by the chat banner
/// and every picker page so the brand is consistent.
pub fn banner_spans(indent: &str) -> Vec<Line<'static>> {
    // "strobes" in a lowercase block font (5 rows), matching the logo wordmark.
    const WORD: [&str; 5] = [
        "      _             _               ",
        "  ___| |_ _ __ ___ | |__   ___  ___ ",
        " / __| __| '__/ _ \\| '_ \\ / _ \\/ __|",
        " \\__ \\ |_| | | (_) | |_) |  __/\\__ \\",
        " |___/\\__|_|  \\___/|_.__/ \\___||___/",
    ];
    let white = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let tm = Style::default().fg(Color::DarkGray);
    let dim = Style::default().fg(Color::DarkGray);

    let mut out: Vec<Line<'static>> = Vec::with_capacity(6);
    for i in 0..5 {
        let mut spans = vec![
            Span::raw(indent.to_string()),
            Span::styled(WORD[i], white),
        ];
        if i == 0 {
            spans.push(Span::styled(" ™", tm));
        }
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        format!("{indent} agents ai · v{} · terminal client", env!("CARGO_PKG_VERSION")),
        dim,
    )));
    out
}

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
            show_full_code: false,
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
            todo_state: HashMap::new(),
            busy: None,
            loading: None,
            loading_seed: 0,
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
            lines_cache: Vec::new(),
            // 0 never matches a real fingerprint's first computation in
            // practice, but even if it did, an empty cache just means the
            // first draw does one harmless extra rebuild.
            lines_fingerprint: 0,
            show_help: false,
            armed: None,
            history: load_prompt_history(),
            hist_pos: None,
        };
        let org = if org_id.len() > 8 { &org_id[..8] } else { &org_id };
        app.auth_line = format!("✔ authenticated · tenant: {tenant} · {} · org {org}", app.base);
        app.push_banner();
        app.set_slash_commands(Vec::new());
        app
    }

    /// The head of every transcript: the brand banner, the authenticated
    /// tenant and the key hints, indented to match the picker pages. Pushed
    /// when the chat opens and again whenever the transcript is cleared for
    /// another thread, so the logo is always the very first thing in it.
    fn push_banner(&mut self) {
        let dim = Style::default().fg(Color::DarkGray);
        self.blocks.push(Block::Plain(Line::from(vec![
            Span::styled("✻ ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled("Strobes Agents", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), dim),
        ])));
        self.blocks.push(Block::Plain(Line::from(Span::styled(
            format!("  {}", self.auth_line.trim_start_matches("✔ authenticated · ")),
            dim,
        ))));
        self.blocks.push(Block::Plain(Line::from(vec![
            Span::styled("  ", dim),
            Span::styled("/", Style::default().fg(Color::White)),
            Span::styled(" commands · ", dim),
            Span::styled("?", Style::default().fg(Color::White)),
            Span::styled(" shortcuts · ", dim),
            Span::styled("esc", Style::default().fg(Color::White)),
            Span::styled(" interrupts the agent", dim),
        ])));
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

    /// Mark the most recent still-running tool call FROM THIS AGENT as
    /// finished (green on success, red on failure), stopping its blink.
    /// Scoped by agent: a supervisor dispatching to a sub-agent can have two
    /// tool calls in flight at once, and a completion event only ever
    /// belongs to its own agent's call — matching "most recent Running,
    /// full stop" across all agents let one agent's completion wrongly
    /// settle a different agent's still-running call, leaving the real
    /// match's dot blinking forever with nothing left to ever settle it.
    fn finish_last_tool(&mut self, agent: &str, ok: bool) {
        for b in self.blocks.iter_mut().rev() {
            if let Block::Tool { status, agent: a, .. } = b {
                if *status == ToolStatus::Running && a == agent {
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

    /// Fold a todo-tool's JSON result into `self.todo_state[agent]` and
    /// return the agent's updated full list, for the caller to render as a
    /// fresh `Block::Todos` snapshot. `None` if the call failed, or if it's
    /// a tool/action this doesn't know how to apply (`remove_todo` doesn't
    /// echo back the removed id, so it can't be applied locally — the next
    /// `list_todos`/`write_todos` will resync) — the caller falls back to
    /// the normal generic tool-output line in that case.
    fn apply_todo_result(&mut self, agent: &str, tool: &str, result: &Value) -> Option<Vec<TodoItem>> {
        if !result.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
            return None;
        }
        let list = self.todo_state.entry(agent.to_string()).or_default();
        match tool {
            // Sub-agent scratchpad (subagent_session_tools.py): text/done,
            // one action per call.
            "todo" => match result.get("action").and_then(|a| a.as_str())? {
                "add" => list.push(TodoItem::from_text_done(result.get("item")?)?),
                "complete" => {
                    let id = json_id_string(result.get("id"));
                    if let Some(t) = list.iter_mut().find(|t| t.id == id) {
                        t.status = "completed".into();
                    }
                }
                "delete" => {
                    let id = json_id_string(result.get("id"));
                    list.retain(|t| t.id != id);
                }
                "list" => {
                    *list = result.get("items")?.as_array()?.iter()
                        .filter_map(TodoItem::from_text_done).collect();
                }
                _ => return None,
            },
            // Orchestrator suite (todo_tools.py): content/title + status.
            // write_todos replaces the whole list; list_todos is a read but
            // shares the same {todos: [...]} result shape, so a "peek" also
            // resyncs local state for free.
            "write_todos" | "list_todos" => {
                *list = result.get("todos")?.as_array()?.iter()
                    .filter_map(TodoItem::from_content_status).collect();
            }
            "add_todo" => list.push(TodoItem::from_content_status(result.get("todo")?)?),
            "update_todo_status" => {
                let updated = TodoItem::from_content_status(result.get("todo")?)?;
                match list.iter_mut().find(|t| t.id == updated.id) {
                    Some(t) => *t = updated,
                    None => list.push(updated),
                }
            }
            "clear_todos" => list.clear(),
            _ => return None,
        }
        Some(list.clone())
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
        // The server can replay a finished segment as one more token carrying
        // the whole text. Appending it would print the answer twice.
        if text.trim().len() >= 8 {
            let replay = match &self.stream {
                Some(s) if s.thinking == thinking => s.buf.trim() == text.trim(),
                Some(_) => false,
                None => matches!(self.blocks.last(),
                    Some(Block::Assistant { md, .. }) if !thinking && md.trim() == text.trim())
                    || matches!(self.blocks.last(), Some(Block::Thinking(t)) if thinking && t.trim() == text.trim()),
            };
            if replay {
                return;
            }
        }
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
                if label != "completed" {
                    self.flush_stream();
                    let msg = match label.as_str() {
                        "cancelled" | "canceled" | "interrupted" => "  ⎿  Interrupted · what should I do instead?".to_string(),
                        other => format!("  ⎿  Run {other}"),
                    };
                    self.blocks.push(Block::Plain(Line::from(Span::styled(msg, Style::default().fg(Color::Red)))));
                }
                self.follow = true;
            }
            AppEvent::Notice(n) => self.notice(&n),
            AppEvent::Progress(Some(msg)) => self.set_busy(msg),
            AppEvent::Progress(None) => self.clear_busy(),
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
                let name = item.tool_name.unwrap_or_default();
                let detail = item.detail.unwrap_or_default();
                let agent = item.agent.unwrap_or_else(|| "agent".into());
                // Todo-tool calls render as a checklist snapshot from their
                // RESULT (tool_output) instead of a generic call line here —
                // the args alone (e.g. `todo(action='complete', id=3)`)
                // don't carry enough to render the full list anyway.
                if is_todo_tool(&name) {
                    return;
                }
                // One compact line per tool call: "⏺ name(args)". A tool often
                // emits an empty `start` then a detailed `local_execute` for the
                // same call — collapse them by replacing the previous line.
                // The server's wrapper tool (e.g. workspace_execute_shell_command)
                // and the local execution it triggers (execute_command) are one
                // call to the reader: a local start, or a repeated start of a
                // call still running, enriches that call instead of adding a line.
                let open = self.blocks.iter().rev().take(4).position(|b| matches!(b,
                    Block::Tool { status: ToolStatus::Running, agent: a, name: n, .. }
                        if a == &agent && (item.local || n == &name)));
                // The server also re-announces a wrapper call (with its args)
                // after it finished: same tool, nothing but its result and
                // reasoning since → it's the call already on screen.
                let open = open.or_else(|| {
                    let label = tool_label(&name);
                    for (back, b) in self.blocks.iter().rev().take(4).enumerate() {
                        match b {
                            Block::ToolResult { .. } | Block::Thinking(_) => continue,
                            Block::Tool { agent: a, name: n, detail: d, .. }
                                if a == &agent && tool_label(n) == label
                                    && (d.trim() == detail.trim() || d.trim().is_empty() || detail.trim().is_empty()) =>
                            {
                                return Some(back);
                            }
                            _ => return None,
                        }
                    }
                    None
                });
                if open.is_some() {
                    self.flush_stream();
                }
                if let Some(back) = open {
                    let i = self.blocks.len() - 1 - back;
                    if let Block::Tool { detail: d, .. } = &mut self.blocks[i] {
                        if d.trim().is_empty() && !detail.trim().is_empty() {
                            *d = detail;
                        }
                    }
                } else if self.last_tool.as_ref() == Some(&(agent.clone(), name.clone())) {
                    // Same call from the same agent: enrich the earlier
                    // (often empty) start in place.
                    if let Some(Block::Tool { detail: d, .. }) = self.blocks.last_mut() {
                        if !detail.is_empty() {
                            *d = detail;
                        }
                    } else {
                        self.blocks.push(Block::Tool { name: name.clone(), detail, status: ToolStatus::Running, agent: agent.clone() });
                    }
                } else {
                    self.flush_stream();
                    self.blocks.push(Block::Tool { name: name.clone(), detail, status: ToolStatus::Running, agent: agent.clone() });
                    self.last_task = None;
                    self.last_tool = Some((agent, name));
                }
                self.follow = true;
            }
            "tool_output" => {
                let name = item.tool_name.clone().unwrap_or_default();
                let agent = item.agent.clone().unwrap_or_else(|| "agent".into());
                // Todo tools never got a Block::Tool pushed at tool_start
                // above — nothing to settle. Try to render a fresh checklist
                // snapshot from the result; if that fails (call errored, or
                // an action this doesn't know how to apply locally, e.g.
                // remove_todo), fall through to the normal "⎿ ..." line so
                // the failure/result is still visible somewhere.
                if is_todo_tool(&name) {
                    self.last_tool = None;
                    let parsed = item.detail.as_deref().and_then(|d| serde_json::from_str::<Value>(d).ok());
                    if let Some(items) = parsed.and_then(|v| self.apply_todo_result(&agent, &name, &v)) {
                        self.flush_stream();
                        let existing = self.blocks.iter_mut().rev().find_map(|b| match b {
                            Block::Todos { agent: a, items: i } if a == &agent => Some(i),
                            _ => None,
                        });
                        match existing {
                            Some(i) => *i = items,
                            None => self.blocks.push(Block::Todos { agent, items }),
                        }
                        self.follow = true;
                        return;
                    }
                }
                // Result rendered as a dim child block under the tool call.
                let d = item.detail.unwrap_or_default();
                let body = if d.is_empty() { "(ok)".to_string() } else { concise_tool_result(&d) };
                self.flush_stream();
                self.finish_last_tool(&agent, true); // dot → green
                self.push_tool_result(body, false);
                self.last_tool = None;
                self.follow = true;
            }
            "tool_failed" => {
                if is_todo_tool(item.tool_name.as_deref().unwrap_or_default()) {
                    self.last_tool = None;
                }
                let agent = item.agent.unwrap_or_else(|| "agent".into());
                self.flush_stream();
                self.finish_last_tool(&agent, false); // dot → red
                self.push_tool_result(truncate(&item.detail.unwrap_or_default(), 600), true);
                self.last_tool = None;
                self.follow = true;
            }
            "task" => {
                let title = item.text.unwrap_or_default();
                let task_status = match item.status.as_deref() {
                    Some("started")      => TaskStatus::Running,
                    Some("completed")    => TaskStatus::Completed,
                    Some("failed")       => TaskStatus::Failed,
                    Some("blocked")      => TaskStatus::Blocked,
                    Some("inconclusive") => TaskStatus::Inconclusive,
                    _                    => TaskStatus::Created,
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

    /// Cheap stand-in for "has anything `build_lines()` would render changed"
    /// — lengths and discriminants only, never re-walks or re-formats any
    /// Markdown. Must stay defensive: anything `build_lines()`/`render_md`/
    /// `render_thinking` read has to be reflected here, or the cache goes
    /// stale and the transcript stops updating.
    fn content_fingerprint(&self, width: u16) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // Width changes rewrap the code boxes, so it must bust the cache.
        width.hash(&mut h);
        for b in &self.blocks {
            match b {
                Block::User(t) => {
                    0u8.hash(&mut h);
                    t.len().hash(&mut h);
                }
                Block::Assistant { agent, md } => {
                    1u8.hash(&mut h);
                    agent.len().hash(&mut h);
                    md.len().hash(&mut h);
                }
                Block::Thinking(s) => {
                    2u8.hash(&mut h);
                    s.len().hash(&mut h);
                }
                Block::Plain(_) => 3u8.hash(&mut h),
                Block::Tool { name, detail, status, agent } => {
                    4u8.hash(&mut h);
                    name.len().hash(&mut h);
                    detail.len().hash(&mut h);
                    agent.len().hash(&mut h);
                    // Only a Running tool's dot animates, so its phase is
                    // the only thing that can change frame-to-frame.
                    match status {
                        ToolStatus::Running => {
                            5u8.hash(&mut h);
                            (self.spinner / 3 % 2).hash(&mut h);
                        }
                        ToolStatus::Done => 6u8.hash(&mut h),
                        ToolStatus::Failed => 7u8.hash(&mut h),
                    }
                }
                Block::Rule(l) => {
                    8u8.hash(&mut h);
                    l.len().hash(&mut h);
                }
                Block::ToolResult { text, failed } => {
                    10u8.hash(&mut h);
                    text.len().hash(&mut h);
                    failed.hash(&mut h);
                }
                Block::Todos { agent, items } => {
                    9u8.hash(&mut h);
                    agent.len().hash(&mut h);
                    for t in items {
                        t.content.len().hash(&mut h);
                        t.status.hash(&mut h);
                    }
                }
            }
        }
        match &self.stream {
            Some(s) => {
                1u8.hash(&mut h);
                s.thinking.hash(&mut h);
                s.agent.len().hash(&mut h);
                s.buf.len().hash(&mut h);
                if s.thinking && !self.show_thinking {
                    // The collapsed "thinking…" line shows a live
                    // elapsed timer + token count + blink phase.
                    self.run_started.map(|t| t.elapsed().as_secs()).hash(&mut h);
                    self.run_tokens.hash(&mut h);
                    (self.spinner / 3 % 2).hash(&mut h);
                }
            }
            None => 0u8.hash(&mut h),
        }
        self.show_thinking.hash(&mut h);
        self.markdown.hash(&mut h);
        self.show_full_code.hash(&mut h);
        h.finish()
    }

    fn build_lines(&self, width: u16) -> Vec<Line<'static>> {
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
                        Span::styled("> ".to_string(), Style::default().fg(Color::DarkGray)),
                        Span::styled(text.clone(), Style::default().fg(Color::Gray)),
                    ]));
                    blank(&mut out);
                }
                Block::Assistant { agent, md } => {
                    let first = cur_agent.as_deref() != Some(agent.as_str());
                    if first {
                        blank(&mut out);
                        if !is_main_agent(agent) {
                            out.push(agent_header(agent));
                        }
                        cur_agent = Some(agent.clone());
                    }
                    self.render_reply(md, first, &mut out);
                }
                Block::Thinking(s) => self.render_thinking(s, &mut out),
                Block::Plain(l) => out.push(l.clone()),
                Block::Tool { name, detail, status, agent: _ } => {
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
                    let name_span = Span::styled(
                        tool_label(name),
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                    );
                    let dot = Span::styled("⏺ ".to_string(), dot_style);
                    let code_style = Style::default().fg(Color::Gray);
                    let dim = Style::default().fg(Color::DarkGray);

                    blank(&mut out);
                    cur_agent = None;
                    if crate::pulse::is_verbatim_tool(name) && detail.trim().lines().count() > 1 {
                        // Code / shell-command calls render their source in a box:
                        // the tool name on its own line, then up to
                        // CODE_PREVIEW_LINES source lines wrapped INSIDE the box
                        // (never overflowing), in a code-style color, with a
                        // "+N more · ctrl+o" hint in the bottom border. ^O expands.
                        out.push(Line::from(vec![dot, name_span]));
                        out.extend(code_box_lines(detail, width, self.show_full_code));
                    } else {
                        // Other tools: "⏺ name(args)" on one line, args in a
                        // readable gray with dim parens.
                        let mut spans = vec![dot, name_span];
                        let detail = detail.trim();
                        if !detail.is_empty() {
                            spans.push(Span::styled("(".to_string(), dim));
                            spans.push(Span::styled(truncate(detail, 160), code_style));
                            spans.push(Span::styled(")".to_string(), dim));
                        }
                        out.push(Line::from(spans));
                    }
                }
                Block::Rule(label) => {
                    cur_agent = None;
                    blank(&mut out);
                    out.push(rule_line(label));
                }
                Block::ToolResult { text, failed } => {
                    let style = if *failed {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                    let limit = if self.show_full_code { usize::MAX } else { RESULT_PREVIEW_LINES };
                    if lines.is_empty() {
                        out.push(Line::from(Span::styled("  ⎿  (no output)".to_string(), style)));
                    }
                    for (i, l) in lines.iter().take(limit).enumerate() {
                        let lead = if i == 0 { "  ⎿  " } else { "     " };
                        out.push(Line::from(Span::styled(format!("{lead}{}", truncate(l, 400)), style)));
                    }
                    if lines.len() > limit {
                        out.push(Line::from(Span::styled(
                            format!("     … +{} lines (ctrl+o to expand)", lines.len() - limit),
                            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                        )));
                    }
                }
                Block::Todos { agent: _, items } => {
                    cur_agent = None;
                    blank(&mut out);
                    for t in items {
                        let (icon, color) = t.icon_and_color();
                        let text_style = if t.status == "completed" {
                            Style::default().fg(Color::DarkGray).add_modifier(Modifier::CROSSED_OUT)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        out.push(Line::from(vec![
                            Span::styled(format!("{icon} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                            Span::styled(t.content.clone(), text_style),
                        ]));
                    }
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
                let first = cur_agent.as_deref() != Some(s.agent.as_str());
                if first {
                    blank(&mut out);
                    if !is_main_agent(&s.agent) {
                        out.push(agent_header(&s.agent));
                    }
                }
                self.render_reply(&s.buf, first, &mut out);
            }
        }
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

    /// An agent reply: "⏺ " on its first line, the rest indented under it.
    fn render_reply(&self, md: &str, bullet: bool, out: &mut Vec<Line<'static>>) {
        let start = out.len();
        self.render_md(md, out);
        let mut lead = bullet;
        for l in out[start..].iter_mut() {
            if lead && l.spans.iter().all(|s| s.content.trim().is_empty()) {
                continue; // keep the bullet for the first visible line
            }
            let pre = if lead {
                Span::styled("⏺ ", Style::default().fg(Color::White))
            } else {
                Span::raw("  ")
            };
            lead = false;
            l.spans.insert(0, pre);
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
        let help = if self.show_help { self.help_lines() } else { Vec::new() };
        let footer_h = (help.len() as u16).max(1);
        let cap = f.area().height.saturating_sub(3 + footer_h).max(3);
        let input_h = (self.input_visual_rows(tw) + 2).clamp(3, cap);
        let activity_h = if self.activity_line().is_some() { 1 } else { 0 };
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(activity_h),
            Constraint::Length(input_h),
            Constraint::Length(footer_h),
        ])
        .split(f.area());
        if self.overlay.is_some() {
            // Close the chat view: show only the picker, anchored to the bottom
            // of the screen (just above the input bar).
            self.draw_overlay(f, chunks[0]);
        } else {
            // Show tasks side panel when there are tasks and the screen is wide enough.
            let body_area = chunks[0];
            // One task ("Exploring & Planning") is just noise; the panel earns
            // its space once the agent has a real plan.
            if self.tasks.len() >= 2 && body_area.width > 80 {
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
        if let Some(l) = self.activity_line() {
            f.render_widget(Paragraph::new(l), chunks[1]);
        }
        self.draw_input(f, chunks[2]);
        if self.show_help {
            f.render_widget(Paragraph::new(help), chunks[3]);
        } else {
            self.draw_footer(f, chunks[3]);
        }
        // Centered animated loader, drawn LAST so it floats on top of the chat
        // UI during an in-chat fetch (overlay open, thread switch, …).
        if let Some(label) = &self.loading {
            crate::picker::draw_loader(f, label, self.spinner, self.loading_seed);
        }
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
            // Keys live in the footer; the title just names the list and
            // echoes the live filter.
            let search = if o.filter.is_empty() { String::new() } else { format!(" · “{}”", o.filter) };
            let list = List::new(items)
                .block(
                    TuiBlock::default()
                        .borders(Borders::ALL)
                        .border_type(ratatui::widgets::BorderType::Rounded)
                        .title(format!(" {} ({count}){search} ", o.title))
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
                .highlight_symbol("➤ ");
            // ASCII banner (+ authenticated line) above the bottom-anchored picker.
            let top_h = area.height.saturating_sub(h);
            if top_h >= 3 {
                // Left indent + a blank top line so the art isn't in the corner.
                const LEFT: &str = "   ";
                let mut banner: Vec<Line<'static>> = vec![Line::default()];
                banner.extend(banner_spans(LEFT));
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
        // Borders (2) + one column of padding on each side (2).
        let inner_w = area.width.saturating_sub(4) as usize;
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
                .border_style(Style::default().fg(Color::DarkGray))
                .padding(Padding::horizontal(1)));
        f.render_widget(panel, area);
    }

    #[allow(dead_code)]
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
        // Width available to the transcript text (no side borders, just padding).
        let text_w = area.width.saturating_sub(2 * PAD_X);
        let fp = self.content_fingerprint(text_w);
        if fp != self.lines_fingerprint || self.lines_cache.is_empty() {
            self.lines_cache = self.build_lines(text_w);
            self.lines_fingerprint = fp;
        }
        let lines = self.lines_cache.clone();
        // Only top + bottom bars (no left/right borders) for a cleaner, wider
        // transcript; horizontal padding keeps text off the edges.
        // No chrome around the transcript — just padding, like a terminal.
        let block = TuiBlock::default().padding(Padding::new(PAD_X, PAD_X, PAD_Y, 0));
        let inner_w = area.width.saturating_sub(2 * PAD_X);
        let inner_h = area.height.saturating_sub(PAD_Y);

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

    /// Insert a block of text (e.g. a bracketed paste) at the cursor.
    pub fn input_insert_str(&mut self, s: &str) {
        self.input.insert_str(self.input_cursor, s);
        self.input_cursor += s.len();
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

    /// Delete the word before the cursor: any whitespace directly behind the
    /// cursor, then the run of non-whitespace before that.
    pub fn input_backspace_word(&mut self) {
        let head = &self.input[..self.input_cursor];
        let word_end = head.trim_end().len();
        let start = head[..word_end]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        self.input.replace_range(start..self.input_cursor, "");
        self.input_cursor = start;
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
            (format!(" {label} "), Color::Cyan)
        } else if self.input.starts_with('/') {
            (String::new(), Color::Cyan)
        } else if self.input.starts_with('!') {
            (" bash ".to_string(), Color::Magenta)
        } else {
            (String::new(), Color::DarkGray)
        };
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
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
                let prefix = if i == 0 { "> " } else { "  " };
                Line::from(win_safe(&format!("{prefix}{}", &self.input[s..e])).into_owned())
            })
            .collect();
        // Placeholder on an empty prompt, like a search box.
        let lines = if self.input.is_empty() && self.pending.is_none() {
            let tip = if self.running {
                "Type to queue a follow-up · esc to interrupt"
            } else {
                "Ask anything — e.g. \"find exposed admin panels on example.com\""
            };
            vec![Line::from(vec![
                Span::raw("> "),
                Span::styled(tip, Style::default().fg(Color::DarkGray)),
            ])]
        } else {
            lines
        };

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

    /// The line above the prompt while something is in flight:
    /// "✻ Working… (12s · ◈ 0.012 cr · esc to interrupt)". None when idle.
    fn activity_line(&self) -> Option<Line<'static>> {
        const SPIN: [&str; 6] = ["✻", "✺", "✹", "✸", "✷", "✶"];
        let glyph = SPIN[(self.spinner / 2) % SPIN.len()];
        let (label, extra) = if let Some(msg) = &self.busy {
            (msg.clone(), String::new())
        } else if self.running {
            let mut parts: Vec<String> = Vec::new();
            if let Some(t) = self.run_started {
                parts.push(fmt_elapsed(t.elapsed()));
            }
            if self.run_tokens > 0 {
                parts.push(format!("↓ {}", fmt_tokens(self.run_tokens)));
            }
            parts.push("esc to interrupt".into());
            let what = self.task_label.clone().map(|t| truncate(&t, 48)).unwrap_or_else(|| "Working…".into());
            (what, format!(" ({})", parts.join(" · ")))
        } else if !self.connected && !self.status.is_empty() && self.status != "connecting…" {
            return Some(Line::from(Span::styled(
                format!(" ○ {} — retrying…", self.status),
                Style::default().fg(Color::Red),
            )));
        } else {
            return None;
        };
        Some(Line::from(vec![
            Span::styled(format!(" {glyph} "), Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            Span::styled(label, Style::default().fg(Color::Magenta)),
            Span::styled(extra, Style::default().fg(Color::DarkGray)),
        ]))
    }

    /// One quiet line under the prompt: a contextual hint on the left, where
    /// the work runs (workspace · model · usage) on the right.
    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let dim = Style::default().fg(Color::DarkGray);
        let left = if let Some(a) = self.armed_now() {
            match a {
                Armed::Quit => "  Press Ctrl-C again to exit".to_string(),
                Armed::ClearInput => "  Esc again to clear".to_string(),
            }
        } else if self.overlay.is_some() {
            let search = if self.overlay_searchable() { "type to filter · " } else { "" };
            format!("  {search}↑↓ select · enter open · esc back")
        } else if self.pending.is_some() {
            "  type your answer · enter to submit".to_string()
        } else if self.select_mode {
            "  select mode — drag to select, ⌘C copies · ctrl+s to exit".to_string()
        } else if self.slash_open() {
            "  ↑↓ select · tab/enter complete · esc cancel".to_string()
        } else if self.input.starts_with('!') {
            "  ! bash mode — runs locally in the agent's sandbox, not sent to the agent".to_string()
        } else if self.input.contains('\n') {
            "  enter to send · ctrl+j newline".to_string()
        } else if !self.input.is_empty() {
            "  enter to send · ctrl+j newline · ctrl+c clear".to_string()
        } else {
            "  ? for shortcuts".to_string()
        };
        let mut right: Vec<String> = Vec::new();
        match &self.workspace_name {
            Some(ws) => right.push(format!("⊞ {}", truncate(ws, 24))),
            None => right.push("no workspace".into()),
        }
        right.push(crate::api::model_name(self.current_model).to_string());
        if self.thread_credits > 0.0 {
            right.push(format!("{:.2} cr", self.thread_credits));
        }
        let right = format!("{} ", right.join(" · "));
        let rw = (right.chars().count() as u16).min(area.width / 2);
        let parts = Layout::horizontal([Constraint::Min(1), Constraint::Length(rw)]).split(area);
        let lstyle = if self.armed_now().is_some() { Style::default().fg(Color::Yellow) } else { dim };
        f.render_widget(Paragraph::new(win_safe(&left).into_owned()).style(lstyle), parts[0]);
        f.render_widget(
            Paragraph::new(win_safe(&right).into_owned()).style(dim).alignment(ratatui::layout::Alignment::Right),
            parts[1],
        );
    }

    /// The `?` shortcuts panel (replaces the footer until the next key).
    fn help_lines(&self) -> Vec<Line<'static>> {
        let rows: [(&str, &str, &str, &str); 7] = [
            ("/", "commands", "ctrl+p", "model"),
            ("!", "run a shell command locally", "ctrl+l", "local files"),
            ("esc", "interrupt the agent", "ctrl+w", "workspaces"),
            ("ctrl+c", "clear input · twice to quit", "ctrl+f", "findings"),
            ("ctrl+j", "newline (shift+enter on kitty)", "ctrl+o", "expand tool output"),
            ("↑ / ↓", "previous prompts", "ctrl+t", "show thinking"),
            ("pgup/pgdn", "scroll the chat", "ctrl+y", "copy transcript"),
        ];
        let key = Style::default().fg(Color::White);
        let dim = Style::default().fg(Color::DarkGray);
        rows.iter()
            .map(|(k1, d1, k2, d2)| {
                Line::from(vec![
                    Span::styled(format!("  {k1:<10}"), key),
                    Span::styled(format!("{d1:<34}"), dim),
                    Span::styled(format!("{k2:<8}"), key),
                    Span::styled(d2.to_string(), dim),
                ])
            })
            .collect()
    }

    /// Arm a two-press key; the footer shows its hint until it expires.
    pub fn arm(&mut self, a: Armed) {
        self.armed = Some((a, std::time::Instant::now()));
    }

    /// True if `a` was armed within the last two seconds (and disarms it).
    pub fn take_armed(&mut self, a: Armed) -> bool {
        let hit = self.armed_now() == Some(a);
        self.armed = None;
        hit
    }

    pub fn disarm(&mut self) {
        self.armed = None;
    }

    fn armed_now(&self) -> Option<Armed> {
        self.armed
            .filter(|(_, t)| t.elapsed() < std::time::Duration::from_secs(2))
            .map(|(a, _)| a)
    }

    /// Record a sent prompt for ↑ recall (and persist it, capped).
    pub fn remember_prompt(&mut self, text: &str) {
        self.hist_pos = None;
        if text.trim().is_empty() || self.history.last().map(|l| l == text).unwrap_or(false) {
            return;
        }
        self.history.push(text.to_string());
        if self.history.len() > 500 {
            let cut = self.history.len() - 500;
            self.history.drain(..cut);
        }
        if let Some(p) = history_path() {
            let body: Vec<String> = self.history.iter().map(|h| h.replace('\n', "\\n")).collect();
            let _ = std::fs::write(p, body.join("\n") + "\n");
        }
    }

    /// ↑ on an empty (or recalled) prompt: step back through sent prompts.
    /// Returns false when there's nothing to recall (caller scrolls instead).
    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() || (self.hist_pos.is_none() && !self.input.is_empty()) {
            return false;
        }
        let idx = match &self.hist_pos {
            None => {
                self.hist_pos = Some((self.history.len() - 1, self.input.clone()));
                self.history.len() - 1
            }
            Some((0, _)) => return true,
            Some((i, d)) => {
                let i = i - 1;
                self.hist_pos = Some((i, d.clone()));
                i
            }
        };
        self.input = self.history[idx].clone();
        self.input_cursor = self.input.len();
        true
    }

    /// ↓ while browsing history: step forward, restoring the draft at the end.
    pub fn history_next(&mut self) -> bool {
        let Some((i, draft)) = self.hist_pos.clone() else { return false };
        if i + 1 >= self.history.len() {
            self.input = draft;
            self.hist_pos = None;
        } else {
            self.hist_pos = Some((i + 1, draft));
            self.input = self.history[i + 1].clone();
        }
        self.input_cursor = self.input.len();
        true
    }

    /// Any edit ends history browsing (the recalled text becomes the draft).
    pub fn end_history_browse(&mut self) {
        self.hist_pos = None;
    }

    /// Print the built-in commands + shortcuts into the transcript (`/help`).
    pub fn show_help_text(&mut self) {
        self.flush_stream();
        let key = Style::default().fg(Color::White);
        let dim = Style::default().fg(Color::DarkGray);
        self.blocks.push(Block::Plain(Line::default()));
        self.blocks.push(Block::Plain(Line::from(Span::styled("Commands", key.add_modifier(Modifier::BOLD)))));
        for (name, desc) in BUILTIN_COMMANDS {
            self.blocks.push(Block::Plain(Line::from(vec![
                Span::styled(format!("  /{name:<12}"), key),
                Span::styled(desc.to_string(), dim),
            ])));
        }
        let skills = self.slash_commands.len();
        if skills > 0 {
            self.blocks.push(Block::Plain(Line::from(Span::styled(
                format!("  …plus {skills} agent skills — type / to browse them"),
                dim,
            ))));
        }
        self.blocks.push(Block::Plain(Line::default()));
        self.blocks.push(Block::Plain(Line::from(Span::styled("Shortcuts", key.add_modifier(Modifier::BOLD)))));
        for l in self.help_lines() {
            self.blocks.push(Block::Plain(l));
        }
        self.follow = true;
        self.pinned = false;
    }

    /// `/status`: where this chat runs and what it has used.
    pub fn show_status_text(&mut self) {
        self.flush_stream();
        let key = Style::default().fg(Color::White);
        let dim = Style::default().fg(Color::DarkGray);
        let ws = self.workspace_name.clone().unwrap_or_else(|| "(none)".into());
        let title = if self.title.is_empty() { "(untitled)".into() } else { self.title.clone() };
        let rows = vec![
            ("connection", format!("{} {}", if self.connected { "● connected" } else { "○ offline" }, self.base)),
            ("workspace", ws),
            ("chat", format!("{title}  ({})", self.thread_id)),
            ("model", crate::api::model_name(self.current_model).to_string()),
            ("usage", format!("{:.3} credits · {} this chat", self.thread_credits, fmt_tokens(self.thread_tokens))),
        ];
        self.blocks.push(Block::Plain(Line::default()));
        for (k, v) in rows {
            self.blocks.push(Block::Plain(Line::from(vec![
                Span::styled(format!("  {k:<12}"), key),
                Span::styled(v, dim),
            ])));
        }
        self.follow = true;
        self.pinned = false;
    }

    #[allow(dead_code)]
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

        // Working spinner while a turn is running (covers tool/http waits),
        // or while waiting on a non-agent network round-trip (`busy`) — e.g.
        // opening the threads/workspaces overlay, switching threads. Without
        // this a slow-but-genuine request looked identical to a frozen app.
        const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let status = if let Some(msg) = &self.busy {
            format!("{} {}", SPIN[self.spinner % SPIN.len()], msg)
        } else if self.running {
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
            " {dot} {chat}  ·  {status}  ·  md:{}  think:{}  code:{}  ⚙ {}{}",
            if self.markdown { "on" } else { "off" },
            if self.show_thinking { "on" } else { "off" },
            if self.show_full_code { "full" } else { "5" },
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
        self.follow = true;
    }

    /// Seed the transcript from full-fidelity persisted events (ordered by seq):
    /// user/agent messages, tool calls + results, tasks, thinking.
    pub fn seed_history_events(&mut self, events: Vec<Value>) {
        if events.is_empty() {
            return;
        }
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
                    // a following tool.failed downgrades it to Failed. Summarize
                    // the args the same way the live path does (code/command,
                    // not a raw JSON dump).
                    let name = pstr("toolName");
                    let args = crate::pulse::tool_detail(&name, p.get("arguments").unwrap_or(&Value::Null));
                    self.flush_stream();
                    // Same wrapper/local pairing as the live path: one call, one line.
                    let dup = self.blocks.iter().rev().take(3).any(|b| matches!(b,
                        Block::Tool { agent: a, detail: d, .. } if a == &agent && (d == &args || args.trim().is_empty())));
                    if !dup {
                        self.blocks.push(Block::Tool { name, detail: args, status: ToolStatus::Done, agent: agent.clone() });
                    }
                }
                "tool.output" => {
                    let body = match p.get("result") {
                        Some(v) => concise_result_value(v),
                        None => "(ok)".to_string(),
                    };
                    let body = if body.is_empty() { "(ok)".to_string() } else { body };
                    self.push_tool_result(body, false);
                }
                "tool.failed" => {
                    self.finish_last_tool(&agent, false);
                    self.push_tool_result(pstr("error"), true);
                }
                t if t.starts_with("task.") => {
                    let title = pstr("title");
                    let task_id = e.get("taskId").and_then(|v| v.as_str()).map(|s| s.to_string());
                    let hist_status = match t {
                        "task.started"      => TaskStatus::Running,
                        "task.completed"    => TaskStatus::Completed,
                        "task.failed"       => TaskStatus::Failed,
                        "task.blocked"      => TaskStatus::Blocked,
                        "task.inconclusive" => TaskStatus::Inconclusive,
                        _                   => TaskStatus::Created,
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
                                "running"      => TaskStatus::Running,
                                "completed"    => TaskStatus::Completed,
                                "failed"       => TaskStatus::Failed,
                                "blocked"      => TaskStatus::Blocked,
                                "inconclusive" => TaskStatus::Inconclusive,
                                _              => TaskStatus::Created,
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
                Block::ToolResult { text, .. } => out.push_str(&format!("  ⎿ {text}\n")),
                Block::Todos { agent: _, items } => {
                    for t in items {
                        let mark = match t.status.as_str() {
                            "completed" => "x",
                            "in_progress" => "~",
                            _ => " ",
                        };
                        out.push_str(&format!("[{mark}] {}\n", t.content));
                    }
                }
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
        // Built-ins first; a server command can't shadow one.
        let mut all: Vec<crate::api::SlashCmd> = BUILTIN_COMMANDS
            .iter()
            .map(|(n, d)| crate::api::SlashCmd { name: n.to_string(), description: d.to_string(), argument_hint: String::new() })
            .collect();
        all.extend(cmds.into_iter().filter(|c| builtin_command(&c.name).is_none()));
        self.slash_commands = all;
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
                Span::styled(format!("/{:<22}", format!("{}{hint}", c.name)), Style::default().fg(Color::Cyan)),
                Span::styled(format!("  {}", truncate(&c.description, 70)), Style::default().fg(Color::DarkGray)),
            ]))
        }).collect();
        let mut state = ListState::default();
        state.select(Some(sel.min(shown.saturating_sub(1))));
        let list = List::new(items)
            .block(TuiBlock::default().borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)))
            .highlight_style(Style::default().fg(Color::White).bg(Color::Rgb(40, 44, 52)).add_modifier(Modifier::BOLD))
            .highlight_symbol("› ");
        f.render_widget(ratatui::widgets::Clear, area);
        let mut s = state;
        f.render_stateful_widget(list, area, &mut s);
    }

    /// Mark a non-agent network round-trip as in flight (opening an
    /// overlay, switching threads, …) — shows a spinner + message in the
    /// status bar. Call `clear_busy` when the result (success or error)
    /// arrives; if a second `set_busy` call happens before the first
    /// clears, the newer message just replaces the old one.
    pub fn set_busy(&mut self, msg: impl Into<String>) {
        self.busy = Some(msg.into());
    }

    pub fn clear_busy(&mut self) {
        self.busy = None;
    }

    pub fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    /// Show the centered "Loading…" overlay on top of the chat UI (for in-chat
    /// fetches). Cleared with `clear_loading` when the result arrives.
    pub fn set_loading(&mut self, msg: impl Into<String>) {
        self.loading = Some(msg.into());
        self.loading_seed = crate::picker::loader_seed();
    }

    pub fn clear_loading(&mut self) {
        self.loading = None;
    }

    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
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
        self.push_banner();
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

    /// `!cmd` bash mode: show the command as a running call.
    pub fn bash_started(&mut self, cmd: &str) {
        self.flush_stream();
        self.blocks.push(Block::Tool {
            name: "__bash__".into(),
            detail: cmd.to_string(),
            status: ToolStatus::Running,
            agent: "__user__".into(),
        });
        self.follow = true;
        self.pinned = false;
    }

    /// `!cmd` finished: settle the call and show its output.
    pub fn bash_done(&mut self, output: String, ok: bool) {
        self.finish_last_tool("__user__", ok);
        let text = if output.trim().is_empty() { "(no output)".to_string() } else { output };
        self.blocks.push(Block::ToolResult { text, failed: !ok });
        self.follow = true;
    }

    /// Attach a tool's result under its call. The wrapper tool and the local
    /// execution it drives both report the same output; show it once.
    fn push_tool_result(&mut self, text: String, failed: bool) {
        let dup = self.blocks.iter().rev().take(3).any(|b| matches!(b,
            Block::ToolResult { text: t, .. } if t.trim() == text.trim()));
        if !dup {
            self.blocks.push(Block::ToolResult { text, failed });
        }
        self.follow = true;
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

/// Reader-facing tool name: `workspace_execute_shell_command` → "Shell",
/// `browser_navigate` → "Browser navigate", anything else in sentence case.
fn tool_label(name: &str) -> String {
    let known = match name {
        "__bash__" => Some("Bash"),
        "execute_command" | "workspace_execute_shell_command" => Some("Shell"),
        "execute_code" | "workspace_execute_code" => Some("Run code"),
        "read_file" | "workspace_read_file" => Some("Read"),
        "write_file" | "workspace_write_file" => Some("Write"),
        "edit_file" | "workspace_edit_file" => Some("Edit"),
        "web_search" => Some("Web search"),
        "web_fetch" | "fetch_url" => Some("Fetch"),
        _ => None,
    };
    if let Some(k) = known {
        return k.to_string();
    }
    let base = name.strip_prefix("workspace_").unwrap_or(name).replace('_', " ");
    let mut c = base.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => name.to_string(),
    }
}

/// The orchestrator is "the assistant" — it needs no name tag; sub-agents do.
fn is_main_agent(agent: &str) -> bool {
    agent.is_empty() || agent == "agent" || agent.contains("Supervisor") || agent == "orchestrator"
}

fn agent_header(agent: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("◆ {agent}"),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
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

/// Some terminals/fonts render these decorative glyphs as double-width while
/// ratatui (correctly, per Unicode's own width tables) counts them as one
/// cell — that desyncs the grid and leaves ghost/leftover characters behind
/// after a scroll or redraw shifts what's on screen. First noticed on
/// Windows console, but the same font-dependent double-width rendering can
/// happen in terminals on any OS, so this applies everywhere rather than
/// gating on `cfg(windows)`.
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

/// Replace ambiguous-width glyphs with guaranteed-width-1 ASCII.
fn win_safe(s: &str) -> std::borrow::Cow<'_, str> {
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
    std::borrow::Cow::Borrowed(s)
}

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

/// Split a string into `w`-character chunks (character-based, since it's code —
/// not word-wrapped). Always returns at least one (possibly empty) segment.
fn wrap_chars(s: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return vec![s.to_string()];
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(w).map(|c| c.iter().collect()).collect()
}

/// Render a code/command blob as a bordered box, wrapping each source line to
/// fit INSIDE the box (so nothing ever overflows the transcript width). Shows up
/// to `CODE_PREVIEW_LINES` source lines unless `show_full`, with a
/// "+N more · ctrl+o" hint in the bottom border. `width` is the transcript's
/// text width.
fn code_box_lines(detail: &str, width: u16, show_full: bool) -> Vec<Line<'static>> {
    const INDENT: usize = 2;
    // Code-ish foreground (soft green) + dim box borders.
    let code_style = Style::default().fg(Color::Rgb(126, 192, 128));
    let border = Style::default().fg(Color::DarkGray);

    // Keep a 1-col safety margin so an exactly-full line never wraps.
    let box_w = (width as usize).saturating_sub(INDENT + 1).max(12);
    let inner = box_w.saturating_sub(4); // "│ " + " │"
    let pad = " ".repeat(INDENT);

    let src: Vec<&str> = detail.lines().collect();
    let total = src.len();
    let shown = if show_full { total } else { CODE_PREVIEW_LINES.min(total) };
    let more = total.saturating_sub(shown);

    let mut out: Vec<Line<'static>> = Vec::new();
    // Top border.
    out.push(Line::from(Span::styled(
        format!("{pad}┌{}┐", "─".repeat(box_w.saturating_sub(2))),
        border,
    )));
    // Wrapped code lines.
    for line in src.iter().take(shown) {
        for seg in wrap_chars(line, inner) {
            let gap = inner.saturating_sub(seg.chars().count());
            out.push(Line::from(vec![
                Span::styled(format!("{pad}│ "), border),
                Span::styled(seg, code_style),
                Span::styled(format!("{} │", " ".repeat(gap)), border),
            ]));
        }
    }
    // Bottom border, carrying the truncation hint when collapsed.
    let bottom = if more > 0 && !show_full {
        let label = format!("─ +{more} more · ctrl+o ");
        let dashes = box_w.saturating_sub(2).saturating_sub(label.chars().count());
        format!("{pad}└{label}{}┘", "─".repeat(dashes))
    } else {
        format!("{pad}└{}┘", "─".repeat(box_w.saturating_sub(2)))
    };
    out.push(Line::from(Span::styled(bottom, border)));
    out
}

/// Beautify a parsed tool-result value for the dim "⎿ …" child line: when it's
/// a JSON object (as code-exec / shell tools return), surface just the outcome
/// — errors if any, else the program output — and drop the repetitive
/// `edit_hint` / `environment` / `script_id` / `note` boilerplate the agent
/// needs but the reader doesn't. Anything else is compacted as-is.
fn concise_result_value(v: &Value) -> String {
    if let Value::Object(obj) = v {
        // render_ui result: {display_type:"ui_block", ui_id, nodes:[…]} — show a
        // widget summary rather than the whole node tree.
        if obj.get("display_type").and_then(|d| d.as_str()) == Some("ui_block") {
            return format!("⛶ rendered {}", crate::pulse::summarize_ui_nodes(obj.get("nodes")));
        }
        let join_strs = |val: &Value| -> String {
            val.as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "))
                .unwrap_or_default()
        };
        if let Some(errs) = obj.get("errors") {
            let e = join_strs(errs);
            if !e.trim().is_empty() {
                return format!("✗ {}", truncate(e.trim(), 2000));
            }
        }
        // Output-bearing results (outputs/stdout_tail/message/note) → show that.
        for key in ["outputs", "stdout_tail", "message", "note"] {
            if let Some(val) = obj.get(key) {
                let o = match val {
                    Value::Array(_) => join_strs(val),
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                let o = o.trim();
                if !o.is_empty() {
                    return truncate(o, 4000);
                }
            }
        }
        // An execution result that simply produced nothing — don't dump its JSON.
        if obj.contains_key("outputs") || obj.contains_key("errors") || obj.contains_key("stdout_tail") {
            return "(no output)".to_string();
        }
        // A bare success/ok flag with nothing else to show.
        if obj.get("success").and_then(|s| s.as_bool()) == Some(true)
            || obj.get("ok").and_then(|s| s.as_bool()) == Some(true)
        {
            return "(ok)".to_string();
        }
    }
    if let Value::String(s) = v {
        return truncate(s.trim(), 4000);
    }
    compact_json(Some(v), 300)
}

/// Beautify a raw tool-result string (live path). Parses JSON when possible,
/// else shows the text truncated.
fn concise_tool_result(detail: &str) -> String {
    match serde_json::from_str::<Value>(detail) {
        Ok(v) => concise_result_value(&v),
        Err(_) => truncate(detail, 4000),
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
mod tool_render_tests {
    use super::concise_tool_result;

    #[test]
    fn concise_tool_result_surfaces_outcome_not_boilerplate() {
        // The code-exec result shape: outputs + the agent-facing edit_hint noise.
        let r = concise_tool_result(
            r#"{"edit_hint":"To fix or tweak this script, call workspace_edit_code with script_id='scr_x' …","environment":"bedrock","errors":[],"outputs":["S4 expiry: 2026-05-22"]}"#,
        );
        assert_eq!(r, "S4 expiry: 2026-05-22");
        assert!(!r.contains("edit_hint") && !r.contains("bedrock"));

        // Errors take priority and are flagged.
        let e = concise_tool_result(r#"{"errors":["Traceback: NameError"],"outputs":[]}"#);
        assert_eq!(e, "✗ Traceback: NameError");

        // Empty output reads clearly; non-JSON passes through unchanged.
        assert_eq!(concise_tool_result(r#"{"errors":[],"outputs":[]}"#), "(no output)");
        assert_eq!(concise_tool_result("plain text"), "plain text");
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
    fn backspace_word_removes_the_word_before_the_cursor() {
        let mut a = app();
        a.input_insert_str("scan héllo wörld  ");
        a.input_backspace_word(); // trailing spaces + "wörld"
        assert_eq!(a.input, "scan héllo ");
        assert_eq!(a.input_cursor, a.input.len());
        a.input_left(); // "scan héllo| "
        a.input_backspace_word(); // only the word behind the cursor
        assert_eq!(a.input, "scan  ");
        assert_eq!(a.input_cursor, 5);
        a.input_backspace_word();
        assert_eq!(a.input, " ");
        assert_eq!(a.input_cursor, 0);
        a.input_backspace_word(); // nothing before the cursor: no-op
        assert_eq!(a.input, " ");
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
