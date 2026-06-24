# Strobes Agents AI — CLI

A local, terminal-native client for **Strobes Agents AI** — think *Claude Code,
but for your Strobes pentest agents*. It binds to a **remote** Strobes
organization / workspace over MasterKey auth, streams agent runs live in a clean
Ratatui UI, and runs the **sandbox (shell) and browser on your local machine**.

```
┌──────────────┐   pulse WS (chat + tool.local_execute)   ┌──────────────────┐
│   strobes    │ ◀──────────────────────────────────────▶ │  Strobes backend │
│   (local)    │   workspace files · findings · approvals  │  remote agents,  │
└──────────────┘                                            │ orchestration,   │
   ▲   ▲                                                     │ guardrails, LLM  │
   │   └─ local Chrome (browser_* tools)                     └──────────────────┘
   └───── local shell sandbox (execute_command / execute_code)
```

> **Security:** no credentials are committed to this repo. You provide your own
> MasterKey at runtime (env vars or interactive `login`); it's stored 0600 under
> your platform config dir and is git-ignored.

## Install

Prebuilt binaries for every platform are published on
[**Releases**](../../releases/latest). Pick the one-liner for your OS — it
detects your architecture, downloads the latest release, and installs `strobes`
onto your `PATH`.

### macOS / Linux — one-liner

```bash
curl -fsSL https://raw.githubusercontent.com/strobes-co/strobes-agents-cli/main/install.sh | bash
```

<details>
<summary>or, without the install script (pure curl):</summary>

```bash
OS=$(uname -s); ARCH=$(uname -m); case "$OS-$ARCH" in
  Darwin-arm64)  T=aarch64-apple-darwin ;;
  Darwin-x86_64) T=x86_64-apple-darwin ;;
  Linux-x86_64)  T=x86_64-unknown-linux-gnu ;;
  Linux-aarch64) T=aarch64-unknown-linux-gnu ;;
  *) echo "unsupported platform: $OS-$ARCH" >&2; exit 1 ;;
esac
curl -fsSL "https://github.com/strobes-co/strobes-agents-cli/releases/latest/download/strobes-$T.tar.gz" \
  | tar -xz && sudo install -m755 "strobes-$T/strobes" /usr/local/bin/strobes && rm -rf "strobes-$T"
strobes --help | head -1
```
</details>

### Windows (PowerShell) — one-liner

```powershell
$ErrorActionPreference='Stop'; $T='x86_64-pc-windows-msvc'; $dst="$env:LOCALAPPDATA\Programs\strobes"; `
New-Item -ItemType Directory -Force $dst | Out-Null; `
Invoke-WebRequest "https://github.com/strobes-co/strobes-agents-cli/releases/latest/download/strobes-$T.tar.gz" -OutFile "$env:TEMP\strobes.tgz"; `
tar -xzf "$env:TEMP\strobes.tgz" -C $env:TEMP; Copy-Item "$env:TEMP\strobes-$T\strobes.exe" "$dst\strobes.exe" -Force; `
[Environment]::SetEnvironmentVariable('Path',$env:Path+";$dst",'User'); `
Write-Host "installed to $dst\strobes.exe — open a new terminal, then run: strobes --help"
```

### Pick a binary manually

| Platform | Asset |
|----------|-------|
| macOS (Apple Silicon) | `strobes-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `strobes-x86_64-apple-darwin.tar.gz` |
| Linux (x86-64) | `strobes-x86_64-unknown-linux-gnu.tar.gz` |
| Windows (x86-64) | `strobes-x86_64-pc-windows-msvc.tar.gz` |

Each ships with a `.sha256` checksum. Verify with
`shasum -a 256 -c <file>.sha256` (macOS) or `sha256sum -c <file>.sha256` (Linux).

### Build from source

```bash
cargo build --release        # -> target/release/strobes
cp target/release/strobes /usr/local/bin/    # optional: put it on your PATH
```

Requirements: **Rust** (`rustup`). **Google Chrome / Chromium** is optional — only
needed if you want the agent to drive a local browser (`browser_*` tools).

## Configure

Set your deployment + MasterKey via env vars (get a MasterKey from the Strobes
UI → Organization → API access, or `POST /v1/organizations/<org>/master_key/`):

```bash
export STROBES_AI_BASE_URL=https://app.strobes.co     # your deployment
export STROBES_AI_ORG_ID=<ORG_UUID>
export STROBES_AI_MASTER_KEY=<40-char-hex-key>
```

These persist to `~/.config/strobes-ai/config.json` (macOS:
`~/Library/Application Support/strobes-ai/config.json`).

> The API path prefix defaults to `/api/v1` (what nginx/ALB-fronted Strobes
> deployments expose) — you normally set nothing else. Only if you hit the
> Django app **directly** (no proxy) set `STROBES_AI_DEPLOYMENT=direct` for `/v1`.

## Use

```bash
strobes status                  # check connectivity
strobes workspaces              # list remote workspaces
strobes threads                 # list your threads
strobes chat                    # interactive chat (thread picker)
strobes chat --thread <UUID> --model 4    # resume a thread with a chosen model
strobes bind --download         # pick + download a workspace locally
strobes pull --workspace <UUID> # download a workspace's files to a folder
strobes update                  # self-update to the latest release
strobes --version               # print the version
```

### In-chat keys (shown in the bottom bar)

| Key | Action |
|-----|--------|
| `Enter` | send message |
| `/` | slash-command autocomplete (Tab/Enter to complete) |
| `^W` | workspaces browser → Enter binds, then pick a new / existing thread |
| `^O` | threads browser (Enter switches) |
| `^F` / `^A` | findings / approvals for the bound workspace (Enter → detail) |
| `^L` | list the synced local workspace files (Enter → path/size detail) |
| `^E` | open the local workspace folder in Finder / Explorer / file manager |
| `^Y` | copy the whole transcript to the clipboard |
| `^T` / `^R` | toggle thinking / markdown |
| `^C` | cancel the running turn (or quit when idle) |
| `Esc` | back (chat → threads → workspaces) · PgUp/PgDn / ↑↓ scroll |

The mouse isn't captured, so your terminal's native **click-drag selection +
copy** works in the transcript; `^Y` copies the entire transcript. A small
spinner appears in the status bar while a turn is running (incl. tool/HTTP
waits). The CLI checks for newer releases on chat start and `strobes status`,
and suggests the update one-liner if one is available.

The transcript renders cleanly: `◆ Agent` headers (only on agent change),
`⏺ tool(args)` + `⎿ result`, dimmed `✻ thinking`, Markdown (incl. tables), and
loads the **entire thread history** on open. When a workspace is bound it syncs
that workspace's files into a local sandbox so the agent's `workspace_get_meta` /
`execute_command` see the real files, and drives a local **Chrome** for the
`browser_*` tools (`STROBES_AI_BROWSER_HEADLESS=1` for headless).

**Browser setup:** the `browser_*` tools need Google Chrome / Chromium. The CLI
auto-detects it in the usual places; if it's missing, the tool returns a
platform-specific install hint. Point at a non-standard install with
`STROBES_AI_CHROME=/path/to/chrome`, or set `STROBES_AI_BROWSER_AUTOINSTALL=1`
to have the CLI download a self-contained **Chrome for Testing** build (cached
under the config dir) on first use.

**Browser isolation:** parallel agents each get their own CDP tab (page) within
a shared Chrome process per workspace, so their navigation state never bleeds
into each other while still sharing cookies/auth.

Model picker ids: `4` Haiku 4.5 · `18` Sonnet 4.6 · `21` Opus 4.7 (Bedrock), or
your org's BYOM id.

### Credits & tokens

The status bar shows **AI credits + token usage**: the current run's usage while
it streams (`◈ 0.036 cr · 1.8k tok`), and the **session total** (`◈ Σ …`) once
idle — accumulated from the backend's `credit.update` events and the
`run.completed` metrics.

---

## Workflows

Workflows let you define multi-agent tasks in a YAML file and execute them
offline — the CLI creates a dedicated workspace, spins up threads, and runs
everything in a live Ratatui TUI with a task tree and streamed output.

```
┌─ Bug Bounty Recon ──── ws:a1b2c3d4 ── 2m 14s ─────────────────────────────┐
│  PHASES & TASKS              │  LIVE OUTPUT                                 │
│  ◆ Reconnaissance            │                                              │
│    ✓ scope-definition  12s   │  ▶ execute_command(subfinder -d example.com) │
│    ⟳ subdomain-enum   1m20s  │  ◀ execute_command: found 47 subdomains      │
│    ○ port-scan waiting       │                                              │
│    ○ tech-fingerprint        │  Scanning for open ports on the discovered   │
│  ○ Phase 2  Exploitation     │  subdomains. This may take a few minutes...  │
│  ○ Phase 3  Reporting        │                                              │
│  ↑↓ select · Enter: chat · Tab: log · PgUp: scroll · q: quit               │
└────────────────────────────────────────────────────────────────────────────┘
```

### Quick start

```bash
# Use a built-in template
strobes workflow init --output myflow.yaml
strobes workflow run myflow.yaml

# Use a bundled security template
strobes workflow run workflows/bugbounty-recon.yaml \
  -v TARGET=example.com \
  -v PROGRAM="Example Bug Bounty"

# Headless (no TUI) — useful for CI
strobes workflow run myflow.yaml --no-tui -v TARGET=example.com
```

If you omit `-v` flags, the CLI prompts interactively for each variable with
its YAML default shown in brackets.

### Workflow YAML format

```yaml
name: "Web App Pentest"
description: "Automated security assessment"

# workspace: { name: "Custom Name" }   # optional; auto-created from workflow name

variables:
  TARGET: "https://example.com"        # default — overridden by -v or prompt
  CREDENTIALS: ""

phases:
  - name: "Reconnaissance"
    tasks:
      - name: port-scan
        prompt: |
          Scan ${TARGET} for open ports and services.

      - name: tech-stack
        prompt: |
          Identify the technology stack of ${TARGET}.

  - name: "Testing"
    tasks:
      # Runs only after port-scan AND tech-stack complete.
      - name: vuln-scan
        depends_on: [port-scan, tech-stack]
        prompt: |
          Run a vulnerability scan on ${TARGET} using the recon results.

      # Runs in parallel with vuln-scan (same depends_on satisfied).
      - name: auth-test
        depends_on: [port-scan, tech-stack]
        prompt: |
          Test the authentication endpoints of ${TARGET}.

  - name: "Report"
    tasks:
      - name: final-report
        depends_on: [vuln-scan, auth-test]
        prompt: |
          Produce a final pentest report for ${TARGET}.
```

**Variable interpolation:** use `${VAR}` or `$VAR` in any string field (`name`,
`prompt`, workspace `name`). Required variables with no default must be supplied
via `-v` or interactive prompt.

**DAG scheduling within a phase:** tasks without `depends_on` start immediately
in parallel. Tasks with `depends_on` block until all named tasks complete. Tasks
whose dependency failed are skipped.

### All workflow commands

| Command | Description |
|---------|-------------|
| `strobes workflow run <file> [-v KEY=VAL…] [--no-tui]` | Execute a workflow |
| `strobes workflow validate <file>` | Parse and validate without running |
| `strobes workflow list` | Find `.yaml` files with `phases:` in current dir |
| `strobes workflow init [--output file.yaml]` | Write a starter template |
| `strobes workflow history` | List past runs with status and progress |
| `strobes workflow resume <run-id>` | Continue an interrupted run |

### TUI keys (workflow)

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate task list |
| `Enter` | Open selected task's full chat view |
| `Tab` | Toggle between task output and combined log |
| `PgUp` / `PgDn` | Scroll output pane manually |
| `f` | Re-enable auto-follow (scroll to latest output) |
| `q` / `Esc` | Quit (after workflow finishes) |

Output is rendered as **Markdown** (headings, bold, code blocks, tables) with
color-coded event lines: `▶ tool(args)` in cyan, `◀ result` in blue,
`✗ error` in red, `💭 thinking` in magenta.

### History and resume

Every workflow run is recorded locally. If a run is interrupted (crash, network
drop, `Ctrl-C`), you can continue from where it left off:

```bash
strobes workflow history
# RUN ID                                  WORKFLOW                    STATUS      DONE    STARTED
# ─────────────────────────────────────────────────────────────────────────────────────────────────
# 20260624-143021-bug-bounty-recon        Bug Bounty Recon            partial     3/9     2026-06-24 14:30:21
# 20260624-120015-webapp-pentest          WebApp Pentest              completed   8/8     2026-06-24 12:00:15

strobes workflow resume 20260624-143021-bug-bounty-recon
```

The resumed run reuses the same workspace and skips already-completed tasks
(shown as `↷` in the TUI). Run records are stored in
`~/.config/strobes-ai/workflow-runs/`.

### Bundled templates

| Template | Phases | Tasks | Variables |
|----------|--------|-------|-----------|
| `workflows/bugbounty-recon.yaml` | 4 | 9 | `TARGET`, `PROGRAM`, `SCOPE`, `OUT_OF_SCOPE`, `DEPTH` |
| `workflows/webapp-pentest.yaml` | 3 | 8 | `TARGET`, `AUTH_URL`, `CREDENTIALS`, `SEVERITY_THRESHOLD` |
| `workflows/full-bugbounty-hunt.yaml` | 5 | 11 | `TARGET`, `PROGRAM`, `PLATFORM`, `TEST_ACCOUNT`, `SECOND_ACCOUNT` |

---

## How it maps to the backend

| CLI piece | Backend counterpart |
|-----------|---------------------|
| MasterKey auth (`Authorization: token …`, WS `?api_key=`) | `MasterKeyAuthentication`, `channels_middleware` |
| `chat` stream | `PulseConsumer` (`ws/<org>/pulse/<thread>/`) |
| local tools (shell / code / browser) | `LocalProxyTool` + `tool.local_execute` events |
| workspaces · threads · history · files · findings · approvals · slash-commands | `cli_views` REST (MasterKey) |
| workflow workspace | `create_workspace` → shared workspace for all workflow threads |
| workflow threads | `create_thread` per task, one pulse connection each |

## Project layout

```
src/
  main.rs           clap commands + async entry point
  config.rs         profiles, secret storage, URL/path helpers
  api.rs            reqwest MasterKey REST client
  pulse.rs          pulse WebSocket client (flat StreamEvents, CLI_LOCAL tool dispatch)
  local.rs          local shell/code execution sandbox
  browser.rs        local Chrome automation (chromiumoxide); per-agent tab isolation
  markdown.rs       Markdown → ratatui Line renderer (headings, tables, code blocks)
  picker.rs         full-screen list selector widget
  app.rs            Ratatui chat app: transcript, overlays, slash popup, input, status

  workflow.rs       YAML schema + parser + variable interpolation + validator
  workflow_runner.rs DAG executor: parallel task dispatch, pulse connection per task,
                    run-record persistence (save on each task completion)
  workflow_state.rs RunRecord JSON persistence (~/.config/strobes-ai/workflow-runs/)
  workflow_tui.rs   Ratatui workflow TUI: task tree + markdown output pane + chat drill-down

workflows/
  bugbounty-recon.yaml       Subdomain enum, port scan, tech fingerprint, dork search
  webapp-pentest.yaml        Auth, injection, access control, XSS, SSRF, file upload
  full-bugbounty-hunt.yaml   End-to-end hunt: recon → exploit → PoC → report
```

## Development

```bash
cargo test       # protocol + render unit tests
cargo run -- chat
cargo run -- workflow run workflows/bugbounty-recon.yaml --no-tui -v TARGET=example.com -v PROGRAM=test
```

## License

Proprietary — © Strobes Security.
