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
    Tool {
        name: String,
        detail: String,
        status: ToolStatus,
        agent: String,
        /// Server call id (`callId`) and the local execution's request id —
        /// whichever an event carries finds this call.
        call_id: Option<String>,
        local_id: Option<String>,
    },
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

/// Tools whose call line is redundant with what the UI already shows (the
/// human-input prompt renders as its own question + answer).
fn is_silent_tool(name: &str) -> bool {
    matches!(name, "request_human_input" | "workspace_request_human_input" | "ask_user")
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
    /// Highlighted option for a choice field.
    sel: usize,
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
    /// `@` file mentions: files under the launch directory (relative paths),
    /// listed lazily the first time `@` is typed.
    mention_files: Option<Vec<String>>,
    mention_sel: usize,
    /// Counter for `!cmd` call ids.
    bash_seq: u64,
    /// Percent of the context window in use (from `context.usage`).
    context_pct: Option<f64>,
    /// Big pastes, shown in the prompt as "[Pasted text #n +L lines]" and
    /// expanded back when the message is sent.
    pastes: Vec<String>,
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
    ("model", "Choose the AI model (or /model sonnet)"),
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

/// Files under the current directory for `@` mentions — relative paths,
/// skipping VCS/build/dependency dirs, capped so a huge tree stays instant.
fn list_mention_files() -> Vec<String> {
    const SKIP: &[&str] = &[".git", "node_modules", "target", "dist", "build", ".venv", "venv", "__pycache__", ".next", ".cache", "Library"];
    let root = match std::env::current_dir() { Ok(d) => d, Err(_) => return Vec::new() };
    let mut out = Vec::new();
    let mut stack = vec![(root.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth < 8 && !SKIP.contains(&name.as_str()) && !(name.starts_with('.') && depth == 0 && name != ".github") {
                    stack.push((e.path(), depth + 1));
                }
            } else if ft.is_file() {
                if let Ok(rel) = e.path().strip_prefix(&root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
            if out.len() >= 5000 {
                return out;
            }
        }
    }
    out
}

fn history_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("strobes-ai").join("prompt_history"))
}

/// Prompts are stored one JSON string per line, newest last (older files
/// used a lossy `\n` escape; those lines are read the old way).
fn load_prompt_history() -> Vec<String> {
    history_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str::<String>(l).unwrap_or_else(|_| l.replace("\\n", "\n")))
                .collect()
        })
        .unwrap_or_default()
}

/// Write the history privately (prompts can contain secrets).
fn save_prompt_history(lines: &[String]) {
    let Some(p) = history_path() else { return };
    let body: String = lines.iter().map(|h| serde_json::to_string(h).unwrap_or_default() + "\n").collect();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&p) {
        use std::io::Write;
        let _ = f.write_all(body.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
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
            pastes: Vec::new(),
            mention_files: None,
            mention_sel: 0,
            bash_seq: 0,
            context_pct: None,
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
        let host = self.base.split("://").nth(1).unwrap_or(&self.base).split('/').next().unwrap_or("").to_string();
        let tenant = self.auth_line.split("tenant: ").nth(1).and_then(|t| t.split(" · ").next()).unwrap_or("").to_string();
        self.blocks.push(Block::Plain(Line::from(Span::styled(format!("  {tenant} · {host}"), dim))));
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
        // The server's cancel receipt reads like the agent talking; show it
        // as a quiet status line instead.
        if is_cancel_receipt(&md) {
            self.push_interrupted();
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
                // Settle the last reply now (also turns a cancel receipt into
                // the "Interrupted" line).
                self.flush_stream();
                self.running = false;
                self.settle_running_tools();
                self.last_elapsed = self.run_started.take().map(|t| t.elapsed());
                let took = self.last_elapsed.map(|d| format!(" · {}", fmt_elapsed(d))).unwrap_or_default();
                self.status = format!("idle ({label}){took}");
                if label != "completed" {
                    self.flush_stream();
                    match label.as_str() {
                        "cancelled" | "canceled" | "interrupted" => self.push_interrupted(),
                        other => self.blocks.push(Block::Plain(Line::from(Span::styled(
                            format!("  ⎿  Run {other}"),
                            Style::default().fg(Color::Red),
                        )))),
                    }
                }
                self.follow = true;
            }
            AppEvent::Context(p) => self.context_pct = Some(p),
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
                self.flush_stream();
                self.push(Line::default());
                self.push(Line::from(vec![
                    Span::styled("? ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::styled(title, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                ]));
                if !message.is_empty() {
                    for l in message.lines() {
                        self.push(Line::from(Span::styled(format!("  {l}"), Style::default().fg(Color::Gray))));
                    }
                }
                let mut p = Pending { id, fields, idx: 0, answers: Map::new(), sel: 0 };
                p.sel = default_choice(&p);
                self.pending = Some(p);
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

    /// Choices for the field being answered (select options, or Yes/No for a
    /// checkbox); empty for free-text fields.
    pub fn pending_choices(&self) -> Vec<String> {
        let Some(p) = &self.pending else { return Vec::new() };
        let Some(f) = p.fields.get(p.idx) else { return Vec::new() };
        field_choices(f).into_iter().map(|(l, _)| l).collect()
    }

    pub fn pending_move(&mut self, up: bool) {
        let n = self.pending_choices().len();
        if let Some(p) = &mut self.pending {
            if n > 0 {
                p.sel = if up { (p.sel + n - 1) % n } else { (p.sel + 1) % n };
            }
        }
    }

    /// Answer the current choice field with option `idx` (or the highlighted
    /// one). Returns the full response once every field is answered.
    pub fn submit_choice(&mut self, idx: Option<usize>) -> Option<(String, Value)> {
        let (label, value) = {
            let p = self.pending.as_ref()?;
            let f = p.fields.get(p.idx)?;
            let ch = field_choices(f);
            let i = idx.unwrap_or(p.sel);
            ch.get(i)?.clone()
        };
        self.record_answer(&label, value)
    }

    /// Stop answering: the run is cancelled by the caller.
    pub fn decline_interrupt(&mut self) {
        if self.pending.take().is_some() {
            self.push(Line::from(Span::styled("  ⎿  Declined", Style::default().fg(Color::DarkGray))));
        }
    }

    fn record_answer(&mut self, shown: &str, value: Value) -> Option<(String, Value)> {
        let (key, label) = {
            let p = self.pending.as_ref()?;
            let f = p.fields.get(p.idx)?;
            (f.key.clone(), f.label.clone())
        };
        self.push(Line::from(vec![
            Span::styled(format!("  ⎿  {label}: "), Style::default().fg(Color::DarkGray)),
            Span::styled(shown.to_string(), Style::default().fg(Color::White)),
        ]));
        let p = self.pending.as_mut()?;
        p.answers.insert(key, value);
        p.idx += 1;
        if p.idx >= p.fields.len() {
            let done = self.pending.take().unwrap();
            self.running = true;
            self.status = "resuming…".into();
            self.run_started.get_or_insert_with(std::time::Instant::now);
            Some((done.id, Value::Object(done.answers)))
        } else {
            let mut np = self.pending.take().unwrap();
            np.sel = default_choice(&np);
            self.pending = Some(np);
            None
        }
    }

    /// Feed the typed value as the answer to the current interrupt field.
    /// Returns `Some((interrupt_id, response_data))` once all fields are filled.
    pub fn submit_interrupt_value(&mut self, text: &str) -> Option<(String, Value)> {
        // Choice fields: a typed number or option text picks that option.
        let choices = self.pending_choices();
        if !choices.is_empty() {
            let t = text.trim().to_lowercase();
            let idx = t.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= choices.len()).map(|n| n - 1)
                .or_else(|| choices.iter().position(|c| c.to_lowercase().starts_with(&t) && !t.is_empty()));
            return self.submit_choice(idx);
        }
        // Empty answer takes the field's default, if it has one.
        let default_text = self.pending.as_ref()
            .and_then(|p| p.fields.get(p.idx))
            .and_then(|f| f.default.clone())
            .map(|d| d.as_str().map(String::from).unwrap_or_else(|| d.to_string()));
        let owned;
        let text = if text.trim().is_empty() {
            match default_text { Some(d) => { owned = d; owned.as_str() } None => text }
        } else {
            text
        };
        // Read the current field (short immutable borrow), compute the value.
        let (_key, _label, value) = {
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
        let is_secret = self.pending.as_ref().and_then(|p| p.fields.get(p.idx)).map(|f| f.ftype == "password").unwrap_or(false);
        let shown = if is_secret { "••••••".to_string() } else { text.to_string() };
        self.record_answer(&shown, value)
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
                if is_todo_tool(&name) || is_silent_tool(&name) {
                    return;
                }
                // One line per call. The server sends an arg-less "early"
                // start while the model streams arguments, then a full start;
                // the local execution it triggers (execute_command for
                // workspace_execute_shell_command) and a re-announce after it
                // finished are the same call too. Fill/skip instead of adding.
                self.flush_stream();
                let id = item.call_id.clone();
                if let Some(i) = self.tool_by_id(id.as_deref()) {
                    // Known call: an early start getting its args, or a
                    // re-announce. Its args also tell us whether a local
                    // execution got paired with the wrong parallel call.
                    self.settle_call_args(i, &detail);
                    self.follow = true;
                    return;
                }
                let found = self.match_tool_start(&agent, &name, &detail, item.local);
                if let (Some(i), Some(new_id)) = (found, id.clone()) {
                    if let Block::Tool { call_id, local_id, .. } = &mut self.blocks[i] {
                        if item.local && local_id.is_none() {
                            *local_id = Some(new_id);
                        } else if !item.local && call_id.is_none() {
                            *call_id = Some(new_id);
                        }
                    }
                }
                match found {
                    Some(i) => {
                        if let Block::Tool { detail: d, .. } = &mut self.blocks[i] {
                            if d.trim().is_empty() && !detail.trim().is_empty() {
                                *d = detail;
                            }
                        }
                    }
                    None => {
                        let (call_id, local_id) = if item.local { (None, id) } else { (id, None) };
                        self.blocks.push(Block::Tool { name: name.clone(), detail, status: ToolStatus::Running, agent: agent.clone(), call_id, local_id });
                        self.last_task = None;
                        self.last_tool = Some((agent, name));
                    }
                }
                self.follow = true;
            }
            "tool_output" => {
                let name = item.tool_name.clone().unwrap_or_default();
                if is_silent_tool(&name) {
                    return;
                }
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
                // Result rendered as a dim child block under ITS call.
                let d = item.detail.unwrap_or_default();
                let body = if d.is_empty() { "(ok)".to_string() } else { concise_tool_result(&d) };
                self.flush_stream();
                if !self.attach_by_id(item.call_id.as_deref(), &body, false) {
                    self.attach_tool_result(&agent, &name, body, false);
                }
                self.last_tool = None;
                self.follow = true;
            }
            "tool_failed" => {
                if is_todo_tool(item.tool_name.as_deref().unwrap_or_default()) {
                    self.last_tool = None;
                }
                let agent = item.agent.unwrap_or_else(|| "agent".into());
                let name = item.tool_name.unwrap_or_default();
                self.flush_stream();
                let text = truncate(&item.detail.unwrap_or_default(), 600);
                if !self.attach_by_id(item.call_id.as_deref(), &text, true) {
                    self.attach_tool_result(&agent, &name, text, true);
                }
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
                Block::Tool { name, detail, status, agent, .. } => {
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
                Block::Tool { name, detail, status, .. } => {
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
                }
                // Collapsed thinking shows on the activity line instead.
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
        // Wrap here (not in the Paragraph) so continuation lines hang under
        // their text — "⏺ long reply…" continues under the reply, not at col 0.
        let w = width as usize;
        out.into_iter().flat_map(|l| wrap_hanging(l, w)).collect()
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
            if l.spans.iter().all(|s| s.content.trim().is_empty()) {
                continue; // blank stays blank (keeps paragraph spacing single)
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
        let help = if self.show_help { self.help_lines_for(f.area().width) } else { Vec::new() };
        let footer_h = (help.len() as u16).max(1);
        let cap = f.area().height.saturating_sub(3 + footer_h).max(3);
        let rows = match self.pending_choices().len() {
            0 => self.input_visual_rows(tw),
            n => n as u16,
        };
        let input_h = (rows + 2).clamp(3, cap);
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
            // …and only while there's work in flight: a finished plan gives the
            // width back to the transcript.
            let open = self.running || self.tasks.iter().any(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Created));
            if self.tasks.len() >= 2 && open && body_area.width > 80 {
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
            } else if self.mention_open_now() {
                self.draw_mention_popup(f, chunks[0]);
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
        // In-chat fetches show on the activity line (see activity_line), not
        // as a full-screen loader.
        let _ = self.loading_seed;
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
                .highlight_style(Style::default().fg(Color::White).bg(Color::Rgb(40, 44, 52)).add_modifier(Modifier::BOLD))
                .highlight_symbol("› ");
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
        if c == '@' {
            self.mention_sel = 0;
            let _ = self.mention_open(); // list files on first use
        }
    }

    /// Insert a block of text (e.g. a bracketed paste) at the cursor.
    pub fn input_insert_str(&mut self, s: &str) {
        self.input.insert_str(self.input_cursor, s);
        self.input_cursor += s.len();
    }

    /// A paste: small ones go in verbatim; big ones become a placeholder so
    /// the prompt stays readable (expanded by `take_message`).
    pub fn input_paste(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let lines = s.lines().count();
        if lines > 4 || s.len() > 600 {
            self.pastes.push(s);
            let tag = format!("[Pasted text #{} +{lines} lines]", self.pastes.len());
            self.input_insert_str(&tag);
        } else {
            self.input_insert_str(&s);
        }
        self.end_history_browse();
    }

    /// The prompt as it should be sent: paste placeholders expanded.
    pub fn expand_pastes(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (i, p) in self.pastes.iter().enumerate() {
            let n = i + 1;
            let lines = p.lines().count();
            out = out.replace(&format!("[Pasted text #{n} +{lines} lines]"), p);
        }
        out
    }

    /// `\` right before the cursor + Enter → replace it with a newline.
    pub fn input_backslash_newline(&mut self) -> bool {
        if self.input_cursor > 0 && self.input[..self.input_cursor].ends_with('\\') {
            self.input.remove(self.input_cursor - 1);
            self.input_cursor -= 1;
            self.input_newline();
            return true;
        }
        false
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
        self.pastes.clear();
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
        let choices = self.pending_choices();
        if !choices.is_empty() {
            let sel = self.pending.as_ref().map(|p| p.sel).unwrap_or(0);
            let lines: Vec<Line<'static>> = choices.iter().enumerate().map(|(i, c)| {
                let on = i == sel;
                let style = if on { Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Gray) };
                Line::from(vec![
                    Span::styled(if on { "❯ " } else { "  " }, style),
                    Span::styled(format!("{}. {c}", i + 1), style),
                ])
            }).collect();
            let block = TuiBlock::default()
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .title(format!(" {} ", self.pending_label().unwrap_or_default()))
                .border_style(Style::default().fg(Color::Cyan))
                .padding(Padding::horizontal(1));
            f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
            return;
        }
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
                if area.width >= 76 {
                    "Ask anything — e.g. \"find exposed admin panels on example.com\""
                } else {
                    "Ask anything…"
                }
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
        let (label, extra) = if let Some(msg) = &self.loading {
            (format!("{msg}…"), String::new())
        } else if let Some(msg) = &self.busy {
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
            // What the agent is doing right now, most specific first.
            let running_tool = self.blocks.iter().rev().take(3).find_map(|b| match b {
                Block::Tool { name, status: ToolStatus::Running, .. } => Some(tool_label(name)),
                _ => None,
            });
            let what = if let Some(t) = running_tool {
                format!("Running {t}…")
            } else if matches!(&self.stream, Some(s) if s.thinking) {
                "Thinking…".to_string()
            } else if self.stream.is_some() {
                "Writing…".to_string()
            } else {
                self.task_label.clone().map(|t| format!("{}…", truncate(&t, 48))).unwrap_or_else(|| "Working…".into())
            };
            (what, format!(" ({})", parts.join(" · ")))
        } else if !self.connected && !self.status.is_empty() && self.status != "connecting…" {
            return Some(Line::from(Span::styled(
                format!(" ○ {}", self.status),
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
        } else if self.pending.is_some() && !self.pending_choices().is_empty() {
            "  ↑↓ or 1-9 to choose · enter to confirm · esc to decline".to_string()
        } else if self.pending.is_some() {
            let def = self.pending.as_ref().and_then(|p| p.fields.get(p.idx)).and_then(|f| f.default.clone());
            match def {
                Some(d) => format!("  type your answer · enter to submit (empty = {}) · esc to decline", d.as_str().map(String::from).unwrap_or_else(|| d.to_string())),
                None => "  type your answer · enter to submit · esc to decline".to_string(),
            }
        } else if self.select_mode {
            "  select mode — drag to select, ⌘C copies · ctrl+s to exit".to_string()
        } else if self.slash_open() || self.mention_open_now() {
            "  ↑↓ select · tab/enter complete · esc cancel".to_string()
        } else if self.input.starts_with('!') {
            "  ! bash mode — runs locally in the agent's sandbox, not sent to the agent".to_string()
        } else if self.input.contains('\n') {
            "  enter to send · \\+enter newline".to_string()
        } else if !self.input.is_empty() {
            "  enter to send · \\+enter newline · ctrl+c clear".to_string()
        } else {
            "  ? for shortcuts".to_string()
        };
        let mut right: Vec<String> = Vec::new();
        match &self.workspace_name {
            Some(ws) => right.push(format!("⊞ {}", truncate(ws, 24))),
            None => right.push("no workspace".into()),
        }
        right.push(match self.current_model {
            None | Some(0) => "default model".to_string(),
            m => crate::api::model_name(m).to_string(),
        });
        if self.thread_credits > 0.0 {
            right.push(format!("{:.2} cr", self.thread_credits));
        }
        // Like Claude Code: only mention context once it's getting full.
        if let Some(p) = self.context_pct.filter(|p| *p >= 60.0) {
            right.insert(0, format!("context {:.0}% used — /new for a fresh chat", p));
        }
        // Narrow terminals keep the most useful item only.
        if area.width < 80 && right.len() > 1 {
            right.truncate(1);
        }
        let mut right = format!("{} ", right.join(" · "));
        let max_r = (area.width / 2) as usize;
        if right.chars().count() > max_r {
            right = format!("{} ", truncate(right.trim_end(), max_r.saturating_sub(2)));
        }
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
        self.help_lines_for(u16::MAX)
    }

    /// The shortcuts table: two columns when it fits, else one.
    fn help_lines_for(&self, width: u16) -> Vec<Line<'static>> {
        let rows: [(&str, &str, &str, &str); 8] = [
            ("/", "commands", "ctrl+p", "model"),
            ("!", "run a shell command locally", "ctrl+l", "local files"),
            ("@", "mention a file in this folder", "ctrl+e", "open workspace folder"),
            ("esc", "interrupt the agent", "ctrl+w", "workspaces"),
            ("ctrl+c", "clear input · twice to quit", "ctrl+f", "findings"),
            ("\\ enter", "newline (or ctrl+j)", "ctrl+o", "expand tool output"),
            ("↑ / ↓", "previous prompts", "ctrl+t", "show thinking"),
            ("pgup/pgdn", "scroll the chat", "ctrl+y", "copy transcript"),
        ];
        let key = Style::default().fg(Color::White);
        let dim = Style::default().fg(Color::DarkGray);
        if width < 80 {
            return rows
                .iter()
                .flat_map(|(k1, d1, k2, d2)| [(*k1, *d1), (*k2, *d2)])
                .map(|(k, d)| Line::from(vec![Span::styled(format!("  {k:<10}"), key), Span::styled(d.to_string(), dim)]))
                .collect();
        }
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
        save_prompt_history(&self.history);
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
                    if is_silent_tool(&name) {
                        continue;
                    }
                    let args = crate::pulse::tool_detail(&name, p.get("arguments").unwrap_or(&Value::Null));
                    self.flush_stream();
                    // Same wrapper/local pairing as the live path: one call, one line.
                    let dup = self.blocks.iter().rev().take(3).any(|b| matches!(b,
                        Block::Tool { agent: a, detail: d, .. } if a == &agent && (d == &args || args.trim().is_empty())));
                    let cid = Some(pstr("callId")).filter(|c| !c.is_empty());
                    if let Some(i) = self.tool_by_id(cid.as_deref()) {
                        self.settle_call_args(i, &args);
                    } else if !dup || cid.is_some() {
                        self.blocks.push(Block::Tool { name, detail: args, status: ToolStatus::Done, agent: agent.clone(), call_id: cid, local_id: None });
                    }
                }
                "tool.output" if is_silent_tool(&pstr("toolName")) => {}
                "tool.output" => {
                    let body = match p.get("result") {
                        Some(v) => concise_result_value(v),
                        None => "(ok)".to_string(),
                    };
                    let body = if body.is_empty() { "(ok)".to_string() } else { body };
                    let cid = pstr("callId");
                    if !self.attach_by_id(Some(&cid), &body, false) {
                        self.push_tool_result(body, false);
                    }
                }
                "tool.failed" => {
                    let cid = pstr("callId");
                    if !self.attach_by_id(Some(&cid), &pstr("error"), true) {
                        self.finish_last_tool(&agent, false);
                        self.push_tool_result(pstr("error"), true);
                    }
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
        let generic = title.is_empty() || title == "CLI session" || title == "Chat with Strobes AI";
        let first_ask = self.blocks.iter().find_map(|b| match b {
            Block::User(t) => Some(t.lines().next().unwrap_or("").to_string()),
            _ => None,
        });
        match (generic, first_ask) {
            (true, Some(ask)) => set_terminal_title(&ask),
            _ => set_terminal_title(&title),
        }
        self.title = title;
    }

    /// Name of the bound workspace (shown top-right in the transcript).
    pub fn workspace_label(&self) -> Option<String> {
        self.workspace_name.clone()
    }

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

    // ---- @ file mentions -----------------------------------------------

    /// The `@query` word the cursor is at the end of, if any.
    fn mention_query(&self) -> Option<(usize, &str)> {
        let before = &self.input[..self.input_cursor];
        let start = before.rfind(|c: char| c.is_whitespace()).map(|i| i + 1).unwrap_or(0);
        let word = &before[start..];
        let q = word.strip_prefix('@')?;
        // "@" inside an email-ish word isn't a mention.
        if start > 0 && !before[..start].ends_with(char::is_whitespace) {
            return None;
        }
        Some((start, q))
    }

    fn mention_matches(&self) -> Vec<String> {
        let Some((_, q)) = self.mention_query() else { return Vec::new() };
        let Some(files) = &self.mention_files else { return Vec::new() };
        let q = q.to_lowercase();
        // Filename hits rank above path hits; shorter paths first.
        let mut hits: Vec<(u8, &String)> = files
            .iter()
            .filter_map(|f| {
                let lf = f.to_lowercase();
                let name = lf.rsplit('/').next().unwrap_or(&lf);
                if q.is_empty() || name.starts_with(&q) {
                    Some((0, f))
                } else if name.contains(&q) {
                    Some((1, f))
                } else if lf.contains(&q) {
                    Some((2, f))
                } else {
                    None
                }
            })
            .collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.len().cmp(&b.1.len())));
        hits.into_iter().take(8).map(|(_, f)| f.clone()).collect()
    }

    /// True while an `@` completion menu should show (loads the file list
    /// on first use).
    pub fn mention_open(&mut self) -> bool {
        if self.mention_query().is_none() || self.pending.is_some() {
            return false;
        }
        if self.mention_files.is_none() {
            self.mention_files = Some(list_mention_files());
        }
        !self.mention_matches().is_empty()
    }

    fn mention_open_now(&self) -> bool {
        self.mention_query().is_some() && self.pending.is_none() && !self.mention_matches().is_empty()
    }

    pub fn mention_move(&mut self, up: bool) {
        let n = self.mention_matches().len();
        if n > 0 {
            self.mention_sel = if up { (self.mention_sel + n - 1) % n } else { (self.mention_sel + 1) % n };
        }
    }

    /// Replace the `@query` word with the chosen file's path (absolute, so the
    /// agent's local tools can open it from any working directory).
    pub fn mention_complete(&mut self) {
        let m = self.mention_matches();
        let Some(pick) = m.get(self.mention_sel.min(m.len().saturating_sub(1))).cloned() else { return };
        let Some((start, _)) = self.mention_query() else { return };
        let abs = std::env::current_dir().map(|d| d.join(&pick)).unwrap_or_else(|_| pick.clone().into());
        let rep = format!("@{} ", abs.display());
        self.input.replace_range(start..self.input_cursor, &rep);
        self.input_cursor = start + rep.len();
        self.mention_sel = 0;
    }

    fn draw_mention_popup(&self, f: &mut Frame, above: Rect) {
        let m = self.mention_matches();
        if m.is_empty() {
            return;
        }
        let h = (m.len() as u16 + 2).min(above.height);
        let area = Rect { x: above.x, y: above.y + above.height.saturating_sub(h), width: above.width, height: h };
        let sel = self.mention_sel.min(m.len() - 1);
        let items: Vec<ListItem> = m.iter().map(|p| ListItem::new(Line::from(Span::styled(p.clone(), Style::default().fg(Color::Gray))))).collect();
        let mut state = ListState::default();
        state.select(Some(sel));
        let list = List::new(items)
            .block(TuiBlock::default().borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)))
            .highlight_style(Style::default().fg(Color::White).bg(Color::Rgb(40, 44, 52)).add_modifier(Modifier::BOLD))
            .highlight_symbol("› ");
        f.render_widget(ratatui::widgets::Clear, area);
        f.render_stateful_widget(list, area, &mut state);
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
        self.context_pct = None;
        self.tasks.clear();
        set_terminal_title("");
        self.push_banner();
    }

    pub fn echo_user(&mut self, text: &str) {
        // Generic thread titles say nothing; name the tab after the first ask.
        let generic = self.title.is_empty() || self.title == "CLI session" || self.title == "Chat with Strobes AI";
        if generic && !self.blocks.iter().any(|b| matches!(b, Block::User(_))) {
            set_terminal_title(text.lines().next().unwrap_or(""));
        }
        self.flush_stream();
        self.blocks.push(Block::User(text.to_string()));
        self.last_task = None;
        self.last_tool = None;
        // Sending a message returns the user to the live bottom of the chat.
        self.follow = true;
        self.pinned = false;
        // Start the turn's elapsed timer at send (covers the wait before the
        // server's run.started arrives) — but a follow-up queued mid-turn
        // doesn't restart the running clock.
        if !self.running || self.run_started.is_none() {
            self.run_started = Some(std::time::Instant::now());
        }
        self.last_elapsed = None;
    }

    /// "⎿ Interrupted · …" — once per interrupt.
    fn push_interrupted(&mut self) {
        let already = matches!(self.blocks.last(), Some(Block::Plain(l))
            if l.spans.iter().any(|s| s.content.contains("Interrupted")));
        if !already {
            self.blocks.push(Block::Plain(Line::from(Span::styled(
                "  ⎿  Interrupted · what should the agent do instead?".to_string(),
                Style::default().fg(Color::Red),
            ))));
        }
        self.follow = true;
    }

    /// True when nobody has written in this chat yet (so `/new` can reuse it).
    pub fn chat_is_empty(&self) -> bool {
        !self.running && !self.blocks.iter().any(|b| matches!(b, Block::User(_) | Block::Assistant { .. }))
    }

    /// `!cmd` bash mode: show the command as a running call.
    pub fn bash_started(&mut self, cmd: &str) -> String {
        self.flush_stream();
        self.bash_seq += 1;
        let id = format!("__bash__{}", self.bash_seq);
        self.blocks.push(Block::Tool {
            name: "__bash__".into(),
            detail: cmd.to_string(),
            status: ToolStatus::Running,
            agent: "__user__".into(),
            call_id: Some(id.clone()),
            local_id: None,
        });
        self.follow = true;
        self.pinned = false;
        id
    }

    /// `!cmd` finished: settle the call and show its output.
    pub fn bash_done(&mut self, id: &str, output: String, ok: bool) {
        let text = if output.trim().is_empty() { "(no output)".to_string() } else { output };
        if !self.attach_by_id(Some(id), &text, !ok) {
            self.blocks.push(Block::ToolResult { text, failed: !ok });
        }
        self.follow = true;
    }

    /// The call an event's id refers to (server call id or local request id).
    fn tool_by_id(&self, id: Option<&str>) -> Option<usize> {
        let id = id.filter(|s| !s.is_empty())?;
        let lo = self.blocks.len().saturating_sub(200);
        (lo..self.blocks.len()).rev().find(|&i| matches!(&self.blocks[i],
            Block::Tool { call_id, local_id, .. }
                if call_id.as_deref() == Some(id) || local_id.as_deref() == Some(id)))
    }

    /// A known call announced its real args. Fill them in; if a local
    /// execution with *those* args was paired with a different parallel call,
    /// swap the two calls' server ids so each result lands where it belongs.
    fn settle_call_args(&mut self, i: usize, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            return;
        }
        let (cur, label, agent) = match &self.blocks[i] {
            Block::Tool { detail, name, agent, .. } => (detail.trim().to_string(), tool_label(name), agent.clone()),
            _ => return,
        };
        if cur.is_empty() {
            if let Block::Tool { detail, .. } = &mut self.blocks[i] {
                *detail = args.to_string();
            }
            return;
        }
        if cur == args {
            return;
        }
        let lo = self.blocks.len().saturating_sub(40);
        let other = (lo..self.blocks.len()).find(|&j| j != i && matches!(&self.blocks[j],
            Block::Tool { detail, name, agent: a, .. }
                if detail.trim() == args && tool_label(name) == label && a == &agent));
        if let Some(j) = other {
            let ci = match &self.blocks[i] { Block::Tool { call_id, .. } => call_id.clone(), _ => None };
            let cj = match &self.blocks[j] { Block::Tool { call_id, .. } => call_id.clone(), _ => None };
            if let Block::Tool { call_id, .. } = &mut self.blocks[i] { *call_id = cj; }
            if let Block::Tool { call_id, .. } = &mut self.blocks[j] { *call_id = ci; }
        }
    }

    /// Put a result under the call with this id. False if no call has it.
    fn attach_by_id(&mut self, id: Option<&str>, text: &str, failed: bool) -> bool {
        let Some(i) = self.tool_by_id(id) else { return false };
        if let Block::Tool { status, .. } = &mut self.blocks[i] {
            if *status == ToolStatus::Running || failed {
                *status = if failed { ToolStatus::Failed } else { ToolStatus::Done };
            }
        }
        // One result per call: the wrapper and its local mirror both report.
        if !matches!(self.blocks.get(i + 1), Some(Block::ToolResult { .. })) {
            self.blocks.insert(i + 1, Block::ToolResult { text: text.to_string(), failed });
        }
        self.follow = true;
        true
    }

    /// Index of an on-screen call that a new tool start belongs to, if any.
    fn match_tool_start(&self, agent: &str, name: &str, detail: &str, local: bool) -> Option<usize> {
        let label = tool_label(name);
        let lo = self.blocks.len().saturating_sub(40);
        let same = |n: &str| n == name || tool_label(n) == label;
        // Oldest first, so parallel calls of one tool fill in order.
        let running = (lo..self.blocks.len()).find(|&i| matches!(&self.blocks[i],
            Block::Tool { status: ToolStatus::Running, agent: a, name: n, detail: d, local_id: l, call_id: c, .. }
                if a == agent && same(n)
                    && (if local { l.is_none() } else { c.is_none() })
                    && (d.trim().is_empty() || (local && d.trim() == detail.trim()) || (local && detail.trim().is_empty()))));
        if running.is_some() {
            return running;
        }
        if local {
            // A local execution whose wrapper already has its args.
            return (lo..self.blocks.len()).find(|&i| matches!(&self.blocks[i],
                Block::Tool { status: ToolStatus::Running, agent: a, name: n, local_id: None, .. } if a == agent && same(n)));
        }
        // The local execution can be announced before its wrapper: a wrapper
        // start then belongs to a local-only call with the same (or no) args,
        // as long as nobody has spoken since.
        let mut orphan = None;
        for i in (lo..self.blocks.len()).rev() {
            match &self.blocks[i] {
                Block::User(_) | Block::Assistant { .. } => break,
                Block::Tool { agent: a, name: n, detail: d, call_id: None, local_id: Some(_), .. }
                    if a == agent && same(n) && (detail.trim().is_empty() || d.trim() == detail.trim()) => orphan = Some(i),
                _ => {}
            }
        }
        if orphan.is_some() {
            return orphan;
        }
        // Re-announce of a finished call: same tool and args, and nobody has
        // spoken since.
        for i in (lo..self.blocks.len()).rev() {
            match &self.blocks[i] {
                Block::User(_) | Block::Assistant { .. } => return None,
                Block::Tool { agent: a, name: n, detail: d, status, .. }
                    if *status != ToolStatus::Running && a == agent && same(n)
                        && (d.trim() == detail.trim() || detail.trim().is_empty()) => return Some(i),
                _ => {}
            }
        }
        None
    }

    /// Put a result under the call it answers (oldest running call of that
    /// tool from that agent) and settle its dot; a duplicate report of the
    /// same output (wrapper + local mirror) is dropped.
    fn attach_tool_result(&mut self, agent: &str, name: &str, text: String, failed: bool) {
        let label = tool_label(name);
        let lo = self.blocks.len().saturating_sub(40);
        let target = (lo..self.blocks.len()).find(|&i| matches!(&self.blocks[i],
            Block::Tool { status: ToolStatus::Running, agent: a, name: n, .. }
                if a == agent && (n == name || tool_label(n) == label)))
;
        let Some(i) = target else {
            // No call waiting: this is the second report of a finished call
            // (wrapper + its local mirror) if that call already shows it.
            let shown = (lo..self.blocks.len()).rev().any(|i| matches!(&self.blocks[i],
                Block::Tool { agent: a, name: n, .. } if a == agent && tool_label(n) == label)
                && matches!(self.blocks.get(i + 1), Some(Block::ToolResult { text: t, .. }) if t.trim() == text.trim()));
            if !shown {
                self.push_tool_result(text, failed);
            }
            return;
        };
        if let Block::Tool { status, .. } = &mut self.blocks[i] {
            *status = if failed { ToolStatus::Failed } else { ToolStatus::Done };
        }
        let dup = matches!(self.blocks.get(i + 1), Some(Block::ToolResult { text: t, .. }) if t.trim() == text.trim());
        if !dup {
            self.blocks.insert(i + 1, Block::ToolResult { text, failed });
        }
        self.follow = true;
    }

    /// Attach a tool's result under its call. The wrapper tool and the local
    /// execution it drives both report the same output; show it once.
    fn push_tool_result(&mut self, text: String, failed: bool) {
        // Only a report repeated back-to-back is a duplicate; another call
        // that happens to return the same text keeps its own result.
        let dup = matches!(self.blocks.last(), Some(Block::ToolResult { text: t, .. }) if t.trim() == text.trim());
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

/// (label, value) choices a field offers: its options, or Yes/No for a checkbox.
fn field_choices(f: &Field) -> Vec<(String, Value)> {
    if !f.options.is_empty() {
        return f.options.clone();
    }
    if matches!(f.ftype.as_str(), "checkbox" | "boolean" | "bool" | "confirm") {
        return vec![("Yes".into(), Value::Bool(true)), ("No".into(), Value::Bool(false))];
    }
    Vec::new()
}

/// The option to highlight first: the field's default, else the first.
fn default_choice(p: &Pending) -> usize {
    let Some(f) = p.fields.get(p.idx) else { return 0 };
    let Some(d) = &f.default else { return 0 };
    field_choices(f).iter().position(|(_, v)| v == d).unwrap_or(0)
}

/// Columns a wrapped line's continuation should be indented by: its leading
/// whitespace plus a leading marker ("⏺ ", "⎿  ", "> ", "• ", "1. ", …).
fn hang_indent(text: &str) -> usize {
    let ws = text.chars().take_while(|c| *c == ' ').count();
    let rest = &text[ws..];
    for m in ["⏺ ", "⎿  ", "> ", "• ", "- ", "* ", "◆ ", "? ", "✻ "] {
        if rest.starts_with(m) {
            return ws + m.chars().count();
        }
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && rest[digits..].starts_with(". ") {
        return ws + digits + 2;
    }
    ws
}

/// Word-wrap a styled line to `width`, indenting continuation rows by
/// `hang_indent`. Box-drawn content (code boxes, tables) is left as is.
fn wrap_hanging(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let sw = |t: &str| Span::raw(t.to_string()).width();
    if width < 8 || line.width() <= width {
        return vec![line];
    }
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    if text.contains(['│', '┌', '└', '╭', '╰']) {
        return vec![line];
    }
    let indent = hang_indent(&text).min(width / 2);
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let push_tok = |cur: &mut Vec<Span<'static>>, tok: &str, style: Style| {
        match cur.last_mut() {
            Some(last) if last.style == style => {
                let mut c = last.content.to_string();
                c.push_str(tok);
                last.content = c.into();
            }
            _ => cur.push(Span::styled(tok.to_string(), style)),
        }
    };
    for span in &line.spans {
        let style = span.style;
        for tok in span.content.split_inclusive(' ') {
            let tw = sw(tok);
            let fits = cur_w + sw(tok.trim_end()) <= width;
            if !fits && cur_w > indent {
                rows.push(Line::from(std::mem::take(&mut cur)));
                cur.push(Span::raw(" ".repeat(indent)));
                cur_w = indent;
                let t = tok.trim_start();
                if t.is_empty() {
                    continue;
                }
                // A single word longer than the row: hard-split it.
                let mut rest = t.to_string();
                while cur_w + sw(rest.as_str()) > width && rest.chars().count() > 1 {
                    let room = width.saturating_sub(cur_w).max(1);
                    let head: String = rest.chars().take(room).collect();
                    rest = rest.chars().skip(room).collect();
                    push_tok(&mut cur, &head, style);
                    rows.push(Line::from(std::mem::take(&mut cur)));
                    cur.push(Span::raw(" ".repeat(indent)));
                    cur_w = indent;
                }
                cur_w += sw(rest.as_str());
                push_tok(&mut cur, &rest, style);
            } else {
                cur_w += tw;
                push_tok(&mut cur, tok, style);
            }
        }
    }
    if !cur.is_empty() {
        rows.push(Line::from(cur));
    }
    rows
}

/// Reader-facing tool name: `workspace_execute_shell_command` → "Shell",
/// `browser_navigate` → "Browser navigate", anything else in sentence case.
pub fn tool_label(name: &str) -> String {
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

/// Name the terminal tab after the chat (OSC title; ignored where unsupported).
pub fn set_terminal_title(title: &str) {
    let t = if title.trim().is_empty() || title == "CLI session" {
        "strobes".to_string()
    } else {
        format!("✻ {}", truncate(title.trim(), 40))
    };
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(t));
}

/// The server's "──── Run cancelled. Completed N operations…" message.
fn is_cancel_receipt(md: &str) -> bool {
    // e.g. "\n\n---\n**Run cancelled.** Completed 0 operations …"
    let t = md.trim().trim_start_matches(|c: char| matches!(c, '─' | '-' | '*' | '_') || c.is_whitespace());
    t.starts_with("Run cancelled") || t.starts_with("Run canceled")
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
/// ASCII fallbacks are for consoles that draw these glyphs at the wrong
/// width (classic Windows conhost). Elsewhere they render fine, so keep them —
/// unless STROBES_ASCII=1 asks for plain ASCII.
fn ascii_glyphs() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(windows) || std::env::var("STROBES_ASCII").map(|v| v == "1" || v == "true").unwrap_or(false)
    })
}

fn win_glyph(c: char) -> Option<&'static str> {
    if !ascii_glyphs() {
        return None;
    }
    Some(match c {
        '●' | '◆' | '⏺' | '◇' | '✻' | '✺' | '✹' | '✸' | '✷' | '✶' | '◈' | '⛁' | '⊞' | '◐' | '★' => "*",
        '○' => "o",
        '⎿' => "L",
        '▷' | '➤' | '›' | '❯' | '↪' => ">",
        '↓' => "v",
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

    fn tool_ev(kind: &str, name: &str, detail: &str, local: bool) -> StreamItem {
        StreamItem {
            kind: kind.into(),
            agent: Some("Strobes AI Supervisor".into()),
            text: None,
            tool_name: Some(name.into()),
            detail: Some(detail.into()),
            status: None,
            local,
            task_id: None, call_id: None,
        }
    }

    /// (label, detail, result) for every tool call on screen, in order.
    fn calls(a: &App) -> Vec<(String, String, Option<String>)> {
        let mut out = Vec::new();
        for (i, b) in a.blocks.iter().enumerate() {
            if let Block::Tool { name, detail, .. } = b {
                let r = match a.blocks.get(i + 1) {
                    Some(Block::ToolResult { text, .. }) => Some(text.clone()),
                    _ => None,
                };
                out.push((tool_label(name), detail.clone(), r));
            }
        }
        out
    }

    #[test]
    fn wrapper_local_and_reannounce_render_as_one_call() {
        // The exact sequence the backend sends for one shell command.
        let mut a = app();
        a.on_stream(tool_ev("tool_start", "workspace_execute_shell_command", "", false));
        a.on_stream(tool_ev("tool_start", "execute_command", "echo hi", true));
        a.on_stream(tool_ev("tool_output", "execute_command", "\"hi\"", false));
        a.on_stream(tool_ev("tool_start", "workspace_execute_shell_command", "echo hi", false));
        a.on_stream(tool_ev("tool_output", "workspace_execute_shell_command", "\"hi\"", false));
        assert_eq!(calls(&a), vec![("Shell".into(), "echo hi".into(), Some("hi".into()))]);
    }

    fn tool_id(kind: &str, name: &str, detail: &str, local: bool, id: &str) -> StreamItem {
        StreamItem { call_id: Some(id.into()), ..tool_ev(kind, name, detail, local) }
    }

    #[test]
    fn parallel_shell_calls_pair_results_by_id() {
        // Captured from the backend: locals run (and finish) in the opposite
        // order to the wrapper calls' announcement.
        let mut a = app();
        let w = "workspace_execute_shell_command";
        a.on_stream(tool_id("tool_start", w, "", false, "A"));
        a.on_stream(tool_id("tool_start", w, "", false, "B"));
        a.on_stream(tool_id("tool_start", "execute_command", "date +%Y", true, "L1"));
        a.on_stream(tool_id("tool_start", "execute_command", "uname -s", true, "L2"));
        a.on_stream(tool_id("tool_output", "execute_command", "\"2026\"", false, "L1"));
        a.on_stream(tool_id("tool_output", "execute_command", "\"Darwin\"", false, "L2"));
        a.on_stream(tool_id("tool_start", w, "uname -s", false, "A"));
        a.on_stream(tool_id("tool_start", w, "date +%Y", false, "B"));
        a.on_stream(tool_id("tool_output", w, "\"2026\"", false, "B"));
        a.on_stream(tool_id("tool_output", w, "\"Darwin\"", false, "A"));
        let mut c = calls(&a);
        c.sort();
        assert_eq!(c, vec![
            ("Shell".into(), "date +%Y".into(), Some("2026".into())),
            ("Shell".into(), "uname -s".into(), Some("Darwin".into())),
        ]);
    }

    #[test]
    fn local_announced_before_wrapper_is_still_one_call() {
        let mut a = app();
        let w = "workspace_execute_shell_command";
        a.on_stream(tool_id("tool_start", "execute_command", "date", true, "L1"));
        a.on_stream(tool_id("tool_start", w, "", false, "C1"));
        a.on_stream(tool_id("tool_output", "execute_command", "\"Thu\"", false, "L1"));
        a.on_stream(tool_id("tool_start", w, "date", false, "C1"));
        a.on_stream(tool_id("tool_output", w, "\"Thu\"", false, "C1"));
        assert_eq!(calls(&a), vec![("Shell".into(), "date".into(), Some("Thu".into()))]);
    }

    #[test]
    fn parallel_calls_keep_their_own_results() {
        let mut a = app();
        // Early (arg-less) starts for three parallel calls, then full ones.
        for _ in 0..3 {
            a.on_stream(tool_ev("tool_start", "complete_task", "", false));
        }
        for t in ["t1", "t2", "t3"] {
            a.on_stream(tool_ev("tool_start", "complete_task", t, false));
        }
        a.on_stream(tool_ev("tool_start", "write_table", "todos", false));
        a.on_stream(tool_ev("tool_output", "write_table", "\"no table\"", false));
        for t in ["done 1", "done 2", "done 3"] {
            a.on_stream(tool_ev("tool_output", "complete_task", &format!("\"{t}\""), false));
        }
        let c = calls(&a);
        assert_eq!(c.len(), 4, "{c:?}");
        assert_eq!(c[0], ("Complete task".into(), "t1".into(), Some("done 1".into())));
        assert_eq!(c[2], ("Complete task".into(), "t3".into(), Some("done 3".into())));
        assert_eq!(c[3], ("Write table".into(), "todos".into(), Some("no table".into())));
    }

    #[test]
    fn replayed_segment_token_is_dropped() {
        let mut a = app();
        let tok = |t: &str| StreamItem {
            kind: "token".into(), agent: Some("Strobes AI Supervisor".into()), text: Some(t.into()),
            tool_name: None, detail: None, status: None, local: false, task_id: None, call_id: None,
        };
        a.on_stream(tok("Hello there, "));
        a.on_stream(tok("friend."));
        a.on_stream(tok("Hello there, friend."));
        assert_eq!(a.stream.as_ref().map(|s| s.buf.as_str()), Some("Hello there, friend."));
    }

    fn choice_interrupt(a: &mut App) {
        let depth = Field {
            key: "depth".into(), label: "Depth".into(), ftype: "select".into(),
            options: vec![("Quick".into(), Value::from("quick")), ("Deep".into(), Value::from("deep"))],
            default: Some(Value::from("deep")),
        };
        let subs = Field { key: "subs".into(), label: "Subdomains".into(), ftype: "checkbox".into(), options: vec![], default: None };
        let note = Field { key: "note".into(), label: "Note".into(), ftype: "text".into(), options: vec![], default: Some(Value::from("none")) };
        a.on_app_event(AppEvent::Interrupt { id: "i1".into(), title: "Configure".into(), message: "".into(), fields: vec![depth, subs, note] });
    }

    #[test]
    fn choice_prompt_answers_by_key_default_and_text() {
        let mut a = app();
        choice_interrupt(&mut a);
        // Highlight starts on the default.
        assert_eq!(a.pending_choices(), vec!["Quick", "Deep"]);
        assert_eq!(a.pending.as_ref().unwrap().sel, 1);
        assert!(a.submit_choice(Some(0)).is_none()); // "1" → Quick
        assert_eq!(a.pending_choices(), vec!["Yes", "No"]); // checkbox
        a.pending_move(false);
        assert!(a.submit_choice(None).is_none()); // No
        // Free text, empty → default.
        let (id, data) = a.submit_interrupt_value("").unwrap();
        assert_eq!(id, "i1");
        assert_eq!(data, serde_json::json!({"depth": "quick", "subs": false, "note": "none"}));
        assert!(a.pending.is_none());
    }

    #[test]
    fn choice_prompt_accepts_typed_number_and_decline() {
        let mut a = app();
        choice_interrupt(&mut a);
        assert!(a.submit_interrupt_value("2").is_none());
        assert_eq!(a.pending.as_ref().unwrap().answers.get("depth"), Some(&Value::from("deep")));
        a.decline_interrupt();
        assert!(a.pending.is_none());
    }

    #[test]
    fn mention_completes_to_absolute_path() {
        let mut a = app();
        a.mention_files = Some(vec!["src/app.rs".into(), "src/main.rs".into(), "README.md".into()]);
        a.input_insert_str("review @mai");
        assert!(a.mention_open());
        assert_eq!(a.mention_matches(), vec!["src/main.rs".to_string()]);
        a.mention_complete();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(a.input, format!("review @{} ", cwd.join("src/main.rs").display()));
        // An email isn't a mention.
        a.input_clear();
        a.input_insert_str("mail me@exa");
        assert!(a.mention_query().is_none());
    }

    #[test]
    fn backslash_enter_makes_a_newline() {
        let mut a = app();
        a.input_insert_str("line one\\");
        assert!(a.input_backslash_newline());
        a.input_insert_str("two");
        assert_eq!(a.input, "line one\ntwo");
        assert!(!a.input_backslash_newline());
    }

    #[test]
    fn cancel_receipt_becomes_interrupted_line() {
        let mut a = app();
        a.push_assistant("Strobes AI Supervisor".into(), "\n\n---\n**Run cancelled.** Completed 0 operations before cancellation.".into());
        a.on_app_event(AppEvent::RunFinished("cancelled".into()));
        let text = a.transcript_plaintext();
        assert_eq!(text.matches("Interrupted").count(), 1, "{text}");
        assert!(!text.contains("Run cancelled"));
        assert!(!is_cancel_receipt("The run cancelled flag is set"));
    }

    #[test]
    fn long_lines_hang_under_their_marker() {
        let text = "⏺ one two three four five six seven eight nine ten";
        let rows = wrap_hanging(Line::from(text.to_string()), 20);
        let rows: Vec<String> = rows.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect();
        assert!(rows.len() > 1);
        for r in &rows[1..] {
            assert!(r.starts_with("  ") && !r.starts_with("   "), "{r:?}");
            assert!(r.chars().count() <= 20, "{r:?}");
        }
        assert_eq!(hang_indent("  ⎿  out"), 5);
        assert_eq!(hang_indent("  12. item"), 6);
        // Code boxes are left for the renderer.
        assert_eq!(wrap_hanging(Line::from("│ ".to_string() + &"x".repeat(50)), 20).len(), 1);
    }

    #[test]
    fn builtin_commands_resolve_aliases() {
        assert_eq!(builtin_command("clear"), Some("new"));
        assert_eq!(builtin_command("quit"), Some("exit"));
        assert_eq!(builtin_command("api-pentest"), None);
    }

    #[test]
    fn big_paste_becomes_placeholder_and_expands() {
        let mut a = app();
        let big = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        a.input_insert_str("see: ");
        a.input_paste(&big);
        assert_eq!(a.input, "see: [Pasted text #1 +10 lines]");
        assert_eq!(a.expand_pastes(&a.input.clone()), format!("see: {big}"));
        a.input_paste("small");
        assert!(a.input.ends_with("small"));
    }

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
