# strobes-tui — Ratatui client for Strobes Agents AI

A Rust + [Ratatui](https://ratatui.rs) terminal client for Strobes Agents AI.
It binds a **remote** Strobes workspace/thread over MasterKey auth, streams the
agent run live in a full-screen TUI, and runs the agent's tools **on your local
machine** (CLI_LOCAL mode) — your machine is the sandbox.

It shares `~/.config/strobes-ai/config.json` with the Python CLI, so
`strobes-ai login` configures this client too (or use the `STROBES_AI_*` env
vars).

## Build

```bash
cd tui
cargo build --release        # -> target/release/strobes-tui
```

## Use

```bash
strobes-tui status                       # profile + connectivity
strobes-tui workspaces                   # list remote workspaces
strobes-tui threads                      # list your threads
strobes-tui chat                         # pick a thread (interactive) then chat
strobes-tui chat --thread <UUID>         # resume a specific thread
strobes-tui chat --new                   # force the picker / start a new thread
strobes-tui bind                         # pick or --new a workspace to bind
strobes-tui bind --download              # …and download its files locally
strobes-tui pull --workspace <UUID>      # download a workspace's files to a folder
strobes-tui probe --thread <UUID>        # headless: stream events to stdout
```

**In-chat command bar** (shown at the bottom): **^W** workspaces · **^O**
threads · **^F** findings · **^A** approvals (^F/^A appear once a workspace is
bound) · **^T** thinking · **^R** markdown. Each opens an overlay browser
(↑/↓ move, **Enter** view-detail or select, **Esc** close): workspaces/threads
select-to-bind/switch; findings/approvals open a scrollable detail view. This
lets you list & view the findings and approvals attached to a workspace, and
switch threads or bind a workspace without leaving chat.

`chat` with no thread shows an **interactive picker** (↑/↓/Enter; first item
creates a new thread). On open it loads the **entire thread history** (messages,
tool calls, tasks — full fidelity from persisted events) and reflects any
in-progress run. You can keep typing/sending while a run is active — extra
messages are **queued** server-side and injected at the next turn boundary.
`bind`/`pull` **download a workspace locally** (zip → extracted folder) and
record the folder↔workspace binding in config.

In `chat`: **Enter** sends · **Ctrl-C** cancels a run (or quits when idle) ·
**Esc** quits · **mouse wheel** / **PgUp/PgDn** / **↑/↓** scroll freely (scroll
to the bottom to resume auto-follow) · **Ctrl-T** toggles thinking ·
**Ctrl-R** toggles Markdown rendering.

**`request_human_input`** — when the agent calls this tool (OTP, a decision, a
confirmation), the input box turns into a prompt for each form field; type the
answer and press **Enter** and the client replies with `interrupt.response`,
resuming the run.

On startup `chat` **loads the thread's existing conversation** (prior user +
agent messages) and, if a run is still in progress, shows a banner and reflects
the running state — so you never open to a blank screen on an active thread.

Assistant messages are rendered as **Markdown** (headings, **bold**/_italic_,
`code`, lists, block quotes) via a `pulldown-cmark` renderer that *strips* the
markup (no literal `#`/`-`) and applies real terminal styling — re-rendered
every frame so partial streamed Markdown formats live. Toggle to raw text with
**Ctrl-R**.

The TUI shows three panes: a scrolling **transcript** (streaming assistant
text, dimmed thinking, `⚙` tool cards with results, task checklist, run
lifecycle), a **message** input box, and a **status bar** (connection · thread ·
run state).

## Layout

```
tui/src/
  main.rs     clap commands (chat/status/workspaces/threads/probe) + async loop
  config.rs   shared config.json, profile, api_prefix / ws_url helpers
  api.rs      reqwest MasterKey REST client (workspaces/threads)
  pulse.rs    tokio-tungstenite pulse WS client (flat StreamEvent parsing,
              tool.local_execute -> local exec -> tool.local_result, approvals)
  local.rs    local shell/code execution (the sandbox)
  app.rs      Ratatui app: transcript building, streaming, rendering, input
```

## Notes

- The pulse wire format is the **flat** StreamEvent the consumer forwards
  (top-level `type`; fields in `data` for ephemeral events or `payload` for
  persisted ones) — not a `pulse_event` wrapper.
- Behind an nginx-fronted deployment use `deployment=enterprise` so REST hits
  `/api/v1` and GraphQL `/api/graphql/` (nginx strips the `/api` prefix).
- Login / workspace+thread creation go through the Python CLI (private GraphQL
  rejects MasterKey); this client consumes an existing thread.
- Browser tools are reported as unsupported by this build (the shell sandbox is
  the core); the Python CLI carries the Playwright browser bridge.

## Tests

```bash
cargo test    # local_execute_roundtrip (mock WS server) + ws_url/prefix
```
