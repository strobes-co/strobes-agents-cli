<p align="center">
  <img src="assets/banner.svg" alt="Strobes Agents AI" width="800"/>
</p>

A local, terminal-native client for **Strobes Agents AI**. It connects to a
remote Strobes organization over MasterKey auth, streams agent runs live in a
clean terminal UI, runs the sandbox (shell) and browser on your local machine —
and ships a self-contained **CI scanning suite** (SAST · SCA · Container · IaC ·
DAST) that works without any extra services.

<p align="center">
  <img src="assets/ws-arch.svg" alt="WebSocket architecture" width="700"/>
</p>

> **Security:** no credentials are committed to this repo. You provide your own
> MasterKey at runtime (env vars or interactive `login`); it's stored 0600 under
> your platform config dir and is git-ignored.

## Install

Prebuilt binaries for every platform are published on
[**Releases**](../../releases/latest). Pick the one-liner for your OS:

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
strobes --version
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

### Platform binaries

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
cargo build --release        # → target/release/strobes
cp target/release/strobes /usr/local/bin/
```

Requirements: **Rust** (`rustup`). Chrome/Chromium is optional — only needed
for the `browser_*` tools in chat and DAST scanning.

## Configure

Get a MasterKey from **Strobes UI → Organization → API access**:

```bash
export STROBES_AI_BASE_URL=https://app.strobes.co     # your deployment
export STROBES_AI_ORG_ID=<ORG_UUID>
export STROBES_AI_MASTER_KEY=<40-char-hex-key>
```

These persist to `~/.config/strobes-ai/config.json`
(`~/Library/Application Support/strobes-ai/config.json` on macOS).

> The API path prefix defaults to `/api/v1` (nginx/ALB-fronted deployments).
> Only if you hit Django directly (no proxy) set `STROBES_AI_DEPLOYMENT=direct`.

---

## `strobes ci` — Security Scanning Suite

<p align="center">
  <img src="assets/ci-pipeline.svg" alt="CI scanning pipeline" width="800"/>
</p>

`strobes ci` is a self-contained security scanning engine built into the CLI.
It runs five scan types — source code, dependencies, container images,
infrastructure-as-code, and live web targets — and sends raw findings to
Strobes AI for reachability analysis and remediation guidance. All outputs are
**SARIF 2.1.0** compatible for GitHub Code Scanning and other CI platforms.

```bash
strobes ci sast .                          # static code analysis
strobes ci sca .                           # software composition analysis
strobes ci container nginx:1.24            # Docker image CVE scan
strobes ci iac ./infra                     # IaC misconfiguration scan
strobes ci dast https://staging.myapp.com  # live web app testing
```

### `strobes ci sast` — Static Application Security Testing

Copies the source tree into a local sandbox, prompts the AI to analyze it for
injection flaws, auth issues, secrets, and unsafe patterns, and streams findings
with severity and fix guidance.

```bash
strobes ci sast .
strobes ci sast ./src --output sarif -o sast.sarif
strobes ci sast ~/myapp --fail-on high --timeout 600
strobes ci sast . --exclude "*.lock" --exclude "vendor/**" --max-mb 200
```

| Flag | Default | Description |
|------|---------|-------------|
| `<dir>` | `.` | Directory to scan |
| `--output` | `text` | Output format: `text`, `json`, `sarif` |
| `-o / --output-file` | — | Save results to a file |
| `--fail-on <SEVERITY>` | — | Exit 1 if any finding ≥ severity (`critical` / `high` / `medium` / `low`) |
| `--exclude <GLOB>` | — | Exclude file patterns (repeatable); `node_modules`, `.git`, binaries always excluded |
| `--max-mb <MB>` | `100` | Maximum sandbox size in MB |
| `--prompt <TEXT>` | — | Override the default SAST prompt |
| `--timeout <SECS>` | `600` | Abort if the AI has not finished |

---

### `strobes ci sca` — Software Composition Analysis

Parses all manifest and lock files in the directory, queries **OSV.dev** for
known CVEs (no API key required), builds the full transitive dependency graph,
then uses AI to determine which vulnerabilities are actually reachable in your
code — so only real risk is surfaced.

```bash
strobes ci sca .
strobes ci sca ~/myapp --output sarif -o sca.sarif
strobes ci sca . --skip-ai --min-severity medium
strobes ci sca . --fail-on high
```

**Supported ecosystems:**

| Language | Files parsed |
|----------|-------------|
| Python | `requirements.txt`, `Pipfile.lock`, `poetry.lock`, `pyproject.toml` |
| Node.js | `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml` |
| Go | `go.sum`, `go.mod` |
| Rust | `Cargo.lock` |
| Ruby | `Gemfile.lock` |
| PHP | `composer.lock` |
| Java | `pom.xml` (Maven), `build.gradle` |
| .NET | `*.csproj`, `packages.lock.json`, `*.deps.json` |

| Flag | Default | Description |
|------|---------|-------------|
| `<dir>` | `.` | Directory to scan |
| `--output` | `text` | Output format: `text`, `json`, `sarif` |
| `-o / --output-file` | — | Save results to a file |
| `--fail-on <SEVERITY>` | — | Exit 1 if any finding ≥ severity |
| `--min-severity` | `low` | Minimum severity to include |
| `--skip-ai` | — | Skip AI reachability; report every CVE from OSV.dev |
| `--timeout <SECS>` | `600` | Abort AI analysis after this many seconds |

---

### `strobes ci container` — Docker Image Scanning

Pulls the image, creates a temporary container to extract the OS package
database (dpkg/apk) and any app-level manifests, queries OSV.dev for CVEs,
then uses AI reachability analysis. **Requires Docker.**

```bash
strobes ci container nginx:1.24
strobes ci container python:3.9-slim --skip-ai
strobes ci container myapp:latest --fail-on critical
strobes ci container ubuntu:20.04 --output sarif -o container.sarif
strobes ci container myapp:latest --platform linux/amd64
```

**Supported base images:** Debian/Ubuntu (dpkg), Alpine (apk), plus any
app-level language manifests found on the image filesystem.

| Flag | Default | Description |
|------|---------|-------------|
| `<image>` | — | Docker image to scan (e.g. `nginx:1.24`, `myapp:latest`) |
| `--output` | `text` | Output format: `text`, `json`, `sarif` |
| `-o / --output-file` | — | Save results to a file |
| `--fail-on <SEVERITY>` | — | Exit 1 if any finding ≥ severity |
| `--min-severity` | `low` | Minimum severity to include |
| `--skip-ai` | — | Skip AI analysis; report all CVEs directly |
| `--platform <PLATFORM>` | — | Target platform for multi-arch images (e.g. `linux/amd64`) |
| `--timeout <SECS>` | `600` | Abort AI analysis after this many seconds |

---

### `strobes ci iac` — Infrastructure-as-Code Scanning

Auto-detects all IaC files in the directory tree (by filename, extension, and
content sniffing), copies only those files into a sandbox, and uses AI to find
real misconfigurations — privilege escalation, open ports, missing encryption,
insecure defaults, and more.

```bash
strobes ci iac .
strobes ci iac ./infra --output sarif -o iac.sarif
strobes ci iac . --fail-on high
strobes ci iac . --only terraform --only kubernetes
```

**Supported IaC types:**

| Type | Files detected |
|------|---------------|
| Terraform | `*.tf`, `*.tfvars` |
| CloudFormation | `template.yaml`, `cloudformation*.yml`, SAM templates |
| Kubernetes | Pod, Deployment, Service, Ingress, Role, RBAC manifests |
| Helm | `Chart.yaml`, `values.yaml` + `templates/*.yaml` |
| Dockerfile | `Dockerfile`, `Dockerfile.*` |
| Docker Compose | `docker-compose.yml`, `docker-compose.yaml`, `compose.yaml` |
| GitHub Actions | `.github/workflows/*.yml` |
| Ansible | `playbook.yml`, `site.yml`, role `tasks/main.yml` |
| ARM Templates | `azuredeploy.json`, ARM JSON with `$schema` |

| Flag | Default | Description |
|------|---------|-------------|
| `<dir>` | `.` | Directory to scan |
| `--output` | `text` | Output format: `text`, `json`, `sarif` |
| `-o / --output-file` | — | Save results to a file |
| `--fail-on <SEVERITY>` | — | Exit 1 if any finding ≥ severity |
| `--only <TYPE>` | — | Restrict to one IaC type (repeatable). Values: `terraform`, `cloudformation`, `kubernetes`, `helm`, `dockerfile`, `compose`, `github-actions`, `ansible`, `arm` |
| `--timeout <SECS>` | `600` | Abort after this many seconds |

---

### `strobes ci dast` — Dynamic Application Security Testing

Actively probes a live URL via HTTP requests, browser navigation, and fuzzing.
No files are copied — the target must be reachable from this machine.

```bash
strobes ci dast http://localhost:5000
strobes ci dast https://staging.myapp.com --output sarif -o dast.sarif
strobes ci dast http://app.local --cookie "session=abc123" --fail-on high
strobes ci dast http://app.local --scope /api --scope /admin
strobes ci dast https://app.com --bearer "$TOKEN"
```

| Flag | Default | Description |
|------|---------|-------------|
| `<url>` | — | Target base URL to scan (required) |
| `--output` | `text` | Output format: `text`, `json`, `sarif` |
| `-o / --output-file` | — | Save results to a file |
| `--fail-on <SEVERITY>` | — | Exit 1 if any finding ≥ severity |
| `--cookie <COOKIE>` | — | Cookie header value for all requests (e.g. `session=abc; csrf=xyz`) |
| `--bearer <TOKEN>` | — | Bearer token for `Authorization` header |
| `--scope <PATH>` | — | Restrict crawl + testing to this path prefix (repeatable) |
| `--prompt <TEXT>` | — | Override the default DAST prompt |
| `--timeout <SECS>` | `900` | Abort if the scan has not finished |

---

### CI Integration

All scan types share a common set of CI-friendly features:

- **SARIF 2.1.0** output (`--output sarif`) — upload to GitHub Code Scanning,
  GitLab SAST, or any SARIF-aware platform
- **Exit-code gating** (`--fail-on <SEVERITY>`) — exit 1 when findings meet or
  exceed the threshold; exit 0 when clean
- **File output** (`-o <FILE>`) — write results to disk for artifact upload

```yaml
# .github/workflows/security.yml
name: Security scan
on: [push, pull_request]
jobs:
  sca:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install strobes
        run: curl -fsSL https://raw.githubusercontent.com/strobes-co/strobes-agents-cli/main/install.sh | bash
      - name: SCA — dependency scan
        env:
          STROBES_AI_BASE_URL:   ${{ secrets.STROBES_BASE_URL }}
          STROBES_AI_ORG_ID:     ${{ secrets.STROBES_ORG_ID }}
          STROBES_AI_MASTER_KEY: ${{ secrets.STROBES_MASTER_KEY }}
        run: strobes ci sca . --output sarif -o sca.sarif --fail-on critical
      - uses: github/codeql-action/upload-sarif@v3
        if: always()
        with:
          sarif_file: sca.sarif
  iac:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install strobes
        run: curl -fsSL https://raw.githubusercontent.com/strobes-co/strobes-agents-cli/main/install.sh | bash
      - name: IaC scan
        env:
          STROBES_AI_BASE_URL:   ${{ secrets.STROBES_BASE_URL }}
          STROBES_AI_ORG_ID:     ${{ secrets.STROBES_ORG_ID }}
          STROBES_AI_MASTER_KEY: ${{ secrets.STROBES_MASTER_KEY }}
        run: strobes ci iac . --output sarif -o iac.sarif --fail-on high
      - uses: github/codeql-action/upload-sarif@v3
        if: always()
        with:
          sarif_file: iac.sarif
```

---

## Interactive Chat (`strobes chat`)

Opens a full terminal UI that streams a remote agent run and executes
its tools (shell, code, browser) on your local machine.

```bash
strobes status                  # check connectivity
strobes workspaces              # list remote workspaces
strobes threads                 # list your threads
strobes chat                    # interactive chat (thread picker)
strobes chat --thread <UUID> --model 18    # resume a thread (Sonnet 4.6)
strobes bind --download         # pick + download a workspace locally
strobes pull --workspace <UUID> # download workspace files to a folder
strobes export -w <UUID>        # export thread transcripts (Markdown + index.md)
strobes export -w <UUID> --format json --dir out/   # raw event JSON
strobes update                  # self-update to the latest release
strobes --version               # print the version
```

### In-chat keys

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `/` | Slash-command autocomplete (Tab/Enter to complete) |
| `^W` | Workspaces browser → Enter binds, then pick a thread |
| `^O` | Threads browser (Enter switches) |
| `^F` / `^A` | Findings / approvals for the bound workspace |
| `^L` | List synced local workspace files |
| `^E` | Open the local workspace folder in Finder / Explorer |
| `^Y` | Copy the full transcript to the clipboard |
| `^T` / `^R` | Toggle thinking / markdown |
| `^C` | Cancel the running turn (or quit when idle) |
| `Esc` | Back (chat → threads → workspaces) · PgUp/PgDn / ↑↓ scroll |

Mouse isn't captured, so terminal native **click-drag selection + copy** still
works in the transcript. A spinner appears while a turn is running. The CLI
checks for newer releases on chat start and suggests the update one-liner.

**Browser setup:** `browser_*` tools need Google Chrome / Chromium — detected
automatically. Point at a non-standard install with `STROBES_AI_CHROME=/path/to/chrome`,
or set `STROBES_AI_BROWSER_AUTOINSTALL=1` to download Chrome for Testing on first use.

**Browser isolation:** parallel agents each get their own CDP tab within a
shared Chrome process per workspace.

**Model picker ids:** `4` Haiku 4.5 · `18` Sonnet 4.6 · `21` Opus 4.7
(Bedrock), or your org's BYOM id.

### Credits & tokens

The status bar shows **AI credits + token usage**: the current run's usage
while streaming (`◈ 0.036 cr · 1.8k tok`), and the session total (`◈ Σ …`)
once idle.

---

## Workflows (`strobes workflow`)

Workflows let you define multi-agent security tasks in a YAML file and execute
them offline — the CLI creates a dedicated workspace, spins up threads, and runs
everything in a live terminal TUI with a task tree and streamed output.

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
strobes workflow init --output myflow.yaml
strobes workflow run myflow.yaml

# Use a bundled security template
strobes workflow run workflows/bugbounty-recon.yaml \
  -v TARGET=example.com \
  -v PROGRAM="Example Bug Bounty"

# Headless (no TUI) — for CI
strobes workflow run myflow.yaml --no-tui -v TARGET=example.com
```

### Workflow YAML format

```yaml
name: "Web App Pentest"
description: "Automated security assessment"

variables:
  TARGET: "https://example.com"
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
      - name: vuln-scan
        depends_on: [port-scan, tech-stack]
        prompt: |
          Run a vulnerability scan on ${TARGET} using the recon results.

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

**DAG scheduling:** tasks without `depends_on` start immediately in parallel.
Tasks with `depends_on` block until all named tasks complete. A failed
dependency causes its dependents to be skipped.

### Workflow commands

| Command | Description |
|---------|-------------|
| `strobes workflow run <file> [-v KEY=VAL…] [--no-tui]` | Execute a workflow |
| `strobes workflow validate <file>` | Parse and validate without running |
| `strobes workflow list` | Find `.yaml` files with `phases:` in current dir |
| `strobes workflow init [--output file.yaml]` | Write a starter template |
| `strobes workflow history` | List past runs with status and progress |
| `strobes workflow resume <run-id>` | Continue an interrupted run |

### History and resume

```bash
strobes workflow history
# RUN ID                                  WORKFLOW                    STATUS      DONE
# ─────────────────────────────────────────────────────────────────────────────────────
# 20260624-143021-bug-bounty-recon        Bug Bounty Recon            partial     3/9
# 20260624-120015-webapp-pentest          WebApp Pentest              completed   8/8

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
  main.rs           clap commands + async entry point (ci scanning suite lives here)
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

assets/
  banner.svg        README header banner (hex logo + feature badges)
  ws-arch.svg       WebSocket architecture diagram
  ci-pipeline.svg   CI scanning pipeline diagram

workflows/
  bugbounty-recon.yaml       Subdomain enum, port scan, tech fingerprint, dork search
  webapp-pentest.yaml        Auth, injection, access control, XSS, SSRF, file upload
  full-bugbounty-hunt.yaml   End-to-end hunt: recon → exploit → PoC → report
```

## Development

```bash
cargo test                      # unit tests
cargo run -- chat
cargo run -- ci sca .           # SCA scan of this repo
cargo run -- workflow run workflows/bugbounty-recon.yaml --no-tui -v TARGET=example.com -v PROGRAM=test
```

## License

Proprietary — © Strobes Security.
