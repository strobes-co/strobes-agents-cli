# Strobes Agents AI — CLI

A local, terminal-native client for **Strobes Agents AI** — think *Claude Code,
but for your Strobes pentest agents*. It binds to a **remote** Strobes
organization / workspace over MasterKey auth, streams agent runs live in your
terminal, and runs the **sandbox (shell) and browser on your local machine**.

```
┌──────────────┐   pulse WS (chat + tool.local_execute)   ┌──────────────────┐
│ strobes CLI  │ ◀──────────────────────────────────────▶ │  Strobes backend │
│  (local)     │   workspace files · findings · approvals  │  (remote agents) │
└──────────────┘                                            │ orchestration,   │
   ▲   ▲                                                     │ guardrails, LLM  │
   │   └─ local Chrome (browser_* tools)                     └──────────────────┘
   └───── local shell sandbox (execute_command / execute_code)
```

This repo ships **two clients** that share the same config and protocol:

| Client | Path | Best for |
|--------|------|----------|
| **Ratatui TUI** (Rust) | [`tui/`](tui/) | The recommended interactive experience — clean streaming UI, slash commands, findings/approvals/workspaces/threads browsers, local Chrome browser. |
| **Python CLI** | [`src/strobes_ai/`](src/strobes_ai/) | Scriptable commands + standalone **bridge daemons** (persistent shell + Playwright browser attached to a workspace). |

> **Security:** no credentials are committed to this repo. You provide your own
> MasterKey at runtime (env vars or an interactive `login`); it's stored 0600
> under your platform config dir and is git-ignored.

---

## Prerequisites

- A Strobes deployment URL, your **organization UUID**, and a **MasterKey**
  (Strobes UI → Organization → API access, or `POST /v1/organizations/<org>/master_key/`).
- **Rust** (`rustup`) for the TUI, and/or **Python 3.10+** for the Python CLI.
- Google Chrome / Chromium installed if you want the agent to drive a local browser.

## Configuration

Both clients read the same config (`~/.config/strobes-ai/config.json` on Linux,
`~/Library/Application Support/strobes-ai/config.json` on macOS) and honor these
environment overrides:

```bash
export STROBES_AI_BASE_URL=https://app.strobes.co     # your deployment
export STROBES_AI_ORG_ID=<ORG_UUID>
export STROBES_AI_MASTER_KEY=<40-char-hex-key>
export STROBES_AI_DEPLOYMENT=enterprise               # 'saas' or 'enterprise'
```

> `deployment` controls the REST/GraphQL path prefix. Behind an nginx-fronted
> deployment that strips a `/api` prefix, use `enterprise` (gives `/api/v1`).

---

## Option A — Ratatui TUI (recommended)

```bash
cd tui
cargo build --release           # -> target/release/strobes-tui
```

Run it (env vars from above, or after a Python `strobes-ai login`):

```bash
./target/release/strobes-tui status        # check connectivity
./target/release/strobes-tui workspaces    # list workspaces
./target/release/strobes-tui threads       # list threads
./target/release/strobes-tui chat          # interactive chat (thread picker)
./target/release/strobes-tui chat --thread <UUID> --model 4   # resume a thread
./target/release/strobes-tui bind --download                  # bind + pull a workspace locally
./target/release/strobes-tui pull --workspace <UUID>          # download a workspace's files
```

**In-chat keys** (shown in the bottom bar):

| Key | Action |
|-----|--------|
| `Enter` | send message |
| `/` | slash-command autocomplete (Tab/Enter to complete) |
| `^W` | workspaces browser (Enter binds → then pick new / existing thread) |
| `^O` | threads browser (Enter switches) |
| `^F` / `^A` | findings / approvals for the bound workspace (Enter → detail) |
| `^T` / `^R` | toggle thinking / markdown |
| `^C` | cancel the running turn (or quit when idle) |
| `Esc` | quit · wheel / PgUp/PgDn / ↑↓ scroll |

The TUI renders a clean transcript: `◆ Agent` headers (only on agent change),
`⏺ tool(args)` + `⎿ result`, dimmed `✻ thinking`, Markdown (incl. tables), and
loads the **entire thread history** on open. When a workspace is bound it syncs
that workspace's files into a local sandbox so the agent's `workspace_get_meta`
/ `execute_command` see the real files, and drives a local **Chrome** for
`browser_*` tools (`STROBES_AI_BROWSER_HEADLESS=1` for headless).

Model picker ids: `4` Haiku 4.5 · `18` Sonnet 4.6 · `21` Opus 4.7 (Bedrock), or
your org's BYOM id.

---

## Option B — Python CLI

```bash
pip install -e .                 # core (chat + shell sandbox)
pip install -e '.[browser]'      # + Playwright browser
playwright install chromium      # one-time browser download
```

```bash
strobes-ai login --base-url https://app.strobes.co --org-id <ORG> --master-key <KEY> --deployment saas
strobes-ai status
strobes-ai workspaces
strobes-ai bind <WORKSPACE_ID>   # or --new
strobes-ai chat                  # interactive agent session (local tools)
strobes-ai bridge                # run persistent local shell + browser bridges
```

The Python client also runs **bridge daemons** (`strobes-ai bridge` /
`strobes-ai-bridge`) that register this machine as a persistent shell + browser
the workspace's cloud agents route to — the same mechanism the web UI uses,
including interactive PTY terminals.

---

## How it maps to the backend

| CLI piece | Backend counterpart |
|-----------|---------------------|
| MasterKey auth (`Authorization: token …`, WS `?api_key=`) | `MasterKeyAuthentication`, `channels_middleware` |
| `chat` stream | `PulseConsumer` (`ws/<org>/pulse/<thread>/`) |
| local tools (CLI_LOCAL) | `LocalProxyTool` + `tool.local_execute` events |
| shell bridge / browser bridge | `ShellBridgeConsumer` / `BrowserBridgeConsumer` |
| workspaces · threads · history · files · findings · approvals · slash-commands | `cli_views` REST (MasterKey) |

## Development

```bash
# Rust
cd tui && cargo test            # protocol + render unit tests

# Python
pip install -e '.[dev]' && pytest
```

## License

Proprietary — © Strobes Security.
