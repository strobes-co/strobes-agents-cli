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
```

### In-chat keys (shown in the bottom bar)

| Key | Action |
|-----|--------|
| `Enter` | send message |
| `/` | slash-command autocomplete (Tab/Enter to complete) |
| `^W` | workspaces browser → Enter binds, then pick a new / existing thread |
| `^O` | threads browser (Enter switches) |
| `^F` / `^A` | findings / approvals for the bound workspace (Enter → detail) |
| `^T` / `^R` | toggle thinking / markdown |
| `^C` | cancel the running turn (or quit when idle) |
| `Esc` | quit · mouse-wheel / PgUp/PgDn / ↑↓ scroll |

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

Model picker ids: `4` Haiku 4.5 · `18` Sonnet 4.6 · `21` Opus 4.7 (Bedrock), or
your org's BYOM id.

### Credits & tokens

The status bar shows **AI credits + token usage**: the current run's usage while
it streams (`◈ 0.036 cr · 1.8k tok`), and the **session total** (`◈ Σ …`) once
idle — accumulated from the backend's `credit.update` events and the
`run.completed` metrics.

## How it maps to the backend

| CLI piece | Backend counterpart |
|-----------|---------------------|
| MasterKey auth (`Authorization: token …`, WS `?api_key=`) | `MasterKeyAuthentication`, `channels_middleware` |
| `chat` stream | `PulseConsumer` (`ws/<org>/pulse/<thread>/`) |
| local tools (shell / code / browser) | `LocalProxyTool` + `tool.local_execute` events |
| workspaces · threads · history · files · findings · approvals · slash-commands | `cli_views` REST (MasterKey) |

## Project layout

```
src/
  main.rs      clap commands (chat/status/workspaces/threads/bind/pull/probe) + async loop
  config.rs    profiles, secret storage, URL/path helpers
  api.rs       reqwest MasterKey REST client
  pulse.rs     pulse chat WebSocket client (flat StreamEvents, CLI_LOCAL tools)
  local.rs     local shell/code execution (the sandbox)
  browser.rs   local Chrome automation (chromiumoxide) for browser_* tools
  markdown.rs  Markdown → ratatui renderer
  picker.rs    full-screen list selector
  app.rs       Ratatui app: transcript, overlays, slash popup, input, status
```

## Development

```bash
cargo test       # protocol + render unit tests
cargo run -- chat
```

## License

Proprietary — © Strobes Security.
