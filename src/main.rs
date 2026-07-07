//! `strobes` — a Ratatui terminal client for Strobes Agents AI.
//!
//! Reuses the same `~/.config/strobes-ai/config.json` as the Python CLI, so
//! `strobes-ai login` configures this client too. The `chat` subcommand opens
//! an interactive Ratatui session that streams a remote agent run and executes
//! its tools locally (the user's machine is the sandbox).

mod api;
mod app;
mod browser;
mod config;
mod local;
mod markdown;
mod picker;
mod pulse;
mod workflow;
mod remote_wf_tui;
mod workflow_runner;
mod workflow_state;
mod workflow_tui;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use app::App;
use config::Config;

/// Capture mouse events so trackpad/wheel scroll reaches the transcript.
/// (Native click-drag selection still works while holding Option/Shift.)
pub fn enable_mouse() {
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
}

pub fn disable_mouse() {
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
}

/// Alert the user that a response finished, by writing terminal escape codes the
/// emulator interprets — cross-platform (macOS/Linux/Windows terminals) and no
/// external processes. Most terminals only surface these when the window is
/// unfocused. Tunables:
///   STROBES_AI_NOTIFY=off            disable entirely
///   STROBES_AI_NOTIFY_MIN_SECS=<n>   only notify for runs ≥ n seconds (default 4)
fn notify_response_done(secs: u64) {
    let disabled = std::env::var("STROBES_AI_NOTIFY")
        .map(|v| v.eq_ignore_ascii_case("off") || v == "0")
        .unwrap_or(false);
    let min = std::env::var("STROBES_AI_NOTIFY_MIN_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(4);
    if let Some(bytes) = notify_done_bytes(secs, disabled, min) {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    }
}

/// The escape bytes for a done-notification, or None when suppressed (disabled,
/// or the run was shorter than `min` seconds). BEL = universal attention;
/// OSC 9 = desktop notification (iTerm2/WezTerm/Windows Terminal); OSC 777 =
/// the notify form used by urxvt and others. Terminals ignore what they can't grok.
fn notify_done_bytes(secs: u64, disabled: bool, min: u64) -> Option<Vec<u8>> {
    if disabled || secs < min {
        return None;
    }
    let msg = "Strobes Agents — response ready";
    Some(
        format!("\x07\x1b]9;{msg}\x07\x1b]777;notify;Strobes Agents;response ready\x07")
            .into_bytes(),
    )
}

#[derive(Parser)]
#[command(name = "strobes", version, about = "Ratatui client for Strobes Agents AI")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Tenant to use for this run (defaults to the configured default tenant).
    #[arg(long, visible_alias = "profile", global = true)]
    tenant: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive chat TUI (default).
    Chat {
        #[arg(long, short)]
        thread: Option<String>,
        #[arg(long, short)]
        workspace: Option<String>,
        #[arg(long, short)]
        model: Option<i64>,
        /// Force the thread picker / create a new thread instead of resuming.
        #[arg(long)]
        new: bool,
    },
    /// Configure credentials interactively (or pass flags to skip prompts).
    Login {
        /// Deployment base URL, e.g. https://app.strobes.co
        #[arg(long)]
        base_url: Option<String>,
        /// Organization UUID.
        #[arg(long)]
        org_id: Option<String>,
        /// 40-char MasterKey.
        #[arg(long)]
        master_key: Option<String>,
        /// Path style: blank/proxy = /api/v1, `direct` = bare /v1.
        #[arg(long)]
        deployment: Option<String>,
        /// Skip the post-save connectivity check.
        #[arg(long)]
        no_verify: bool,
    },
    /// Update the CLI to the latest release (downloads + replaces the binary).
    Update {
        /// Reinstall even if already on the latest version.
        #[arg(long)]
        force: bool,
    },
    /// List configured tenants (the default is marked with ★).
    Tenants,
    /// Show AI credit usage (org total + per-workspace breakdown).
    Credits {
        /// Scope to a single workspace.
        #[arg(long, short)]
        workspace: Option<String>,
        /// Scope to a single chat thread.
        #[arg(long, short)]
        thread: Option<String>,
    },
    /// Show the active profile and connectivity.
    Status,
    /// List remote workspaces.
    Workspaces,
    /// List your chat threads.
    Threads,
    /// Pick (or create) a workspace to bind, optionally downloading it locally.
    Bind {
        #[arg(long, short)]
        workspace: Option<String>,
        #[arg(long)]
        new: bool,
        #[arg(long, default_value = "CLI Workspace")]
        name: String,
        /// Also download the workspace files to a local folder.
        #[arg(long)]
        download: bool,
        #[arg(long)]
        dir: Option<String>,
    },
    /// Download a workspace's files to a local folder (binds the folder).
    Pull {
        #[arg(long, short)]
        workspace: Option<String>,
        #[arg(long)]
        dir: Option<String>,
    },
    /// Upload local file(s) to a workspace.
    Push {
        /// Local file paths to upload.
        files: Vec<String>,
        #[arg(long, short)]
        workspace: Option<String>,
        /// Destination dir/prefix inside the workspace (default: file name at root).
        #[arg(long)]
        dir: Option<String>,
    },
    /// Headless probe: connect, stream events to stdout, run local tools.
    /// Used to verify the WS + local-execution path without the TUI.
    Probe {
        #[arg(long, short)]
        thread: String,
        /// Optionally send a prompt on connect.
        #[arg(long)]
        send: Option<String>,
        /// Seconds to stay connected.
        #[arg(long, default_value = "200")]
        secs: u64,
        /// LLM model picker id (e.g. 4 = Haiku, 18 = Sonnet 4.6).
        #[arg(long, short)]
        model: Option<i64>,
    },
    /// Run YAML-based offline workflows (sequence, parallel, DAG).
    Workflow {
        #[command(subcommand)]
        sub: WorkflowCmd,
    },
    /// Create a thread, send one message, stream the response to stdout, then exit.
    /// Useful for scripting and for launching orchestrator prompts that spawn sub-agents.
    Send {
        /// Message to send (the full prompt text).
        message: String,
        /// Attach the thread to this existing workspace ID.
        #[arg(long, short)]
        workspace: Option<String>,
        /// Create a new workspace with this name and attach the thread to it.
        #[arg(long)]
        new_workspace: Option<String>,
        /// Thread title (defaults to first 60 chars of the message).
        #[arg(long, short)]
        title: Option<String>,
        /// LLM model picker id.
        #[arg(long, short)]
        model: Option<i64>,
        /// Output format: text (default) or json.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        output: String,
        /// Do not prompt for human input on interrupts; fail immediately instead (CI-safe).
        #[arg(long)]
        non_interactive: bool,
        /// Abort after this many seconds if the agent has not finished.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        /// Exit 1 if any findings at or above this severity exist after the run
        /// (critical, high, medium, low).
        #[arg(long, value_name = "SEVERITY")]
        fail_on_findings: Option<String>,
        /// Pre-populate the agent's local sandbox from this ID.
        /// The sandbox is created at ~/.strobes-ai/sandboxes/<ID>/
        /// Copy your codebase there before calling this command so the
        /// agent can cat / grep / run scripts against it with local tools.
        #[arg(long, value_name = "ID")]
        sandbox_id: Option<String>,
        /// Select a specific system agent for this thread (e.g. pr_security_review_agent).
        /// Corresponds to the agent_id registered on the server.
        #[arg(long, value_name = "AGENT_ID")]
        agent: Option<String>,
    },
    /// List or export workspace findings (JSON / SARIF for CI integration).
    Findings {
        /// Workspace to query (defaults to the bound workspace).
        #[arg(long, short)]
        workspace: Option<String>,
        /// Output format: text (default), json, or sarif.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        format: String,
        /// Exit 1 if any findings at or above this severity exist
        /// (critical, high, medium, low).
        #[arg(long, value_name = "SEVERITY")]
        fail_on: Option<String>,
    },
    /// CI security scanning — SAST, SCA, DAST, and container image scanning.
    Ci {
        #[command(subcommand)]
        sub: CiCmd,
    },
    /// Export thread transcripts from a workspace into a single folder.
    Export {
        /// Workspace to export (defaults to the bound workspace).
        #[arg(long, short)]
        workspace: Option<String>,
        /// Export only this thread instead of every thread in the workspace.
        #[arg(long, short)]
        thread: Option<String>,
        /// Destination folder (default: ./strobes-transcripts-<workspace>).
        #[arg(long)]
        dir: Option<String>,
        /// Output format: md (default) or json (raw persisted event stream).
        #[arg(long, default_value = "md", value_name = "FORMAT")]
        format: String,
        /// Markdown only: omit tool calls, tool output, thinking and task markers.
        #[arg(long)]
        messages_only: bool,
    },
}

#[derive(Subcommand)]
enum WorkflowCmd {
    /// Run a YAML workflow file.
    Run {
        /// Path to the workflow YAML file.
        file: String,
        /// Override a workflow variable (KEY=VALUE). Repeatable.
        #[arg(long, short = 'v', value_name = "KEY=VALUE")]
        var: Vec<String>,
        /// Print events to stdout instead of opening the TUI.
        #[arg(long)]
        no_tui: bool,
        /// Do not prompt for missing variables; fail if any are unset (CI-safe).
        #[arg(long)]
        non_interactive: bool,
        /// Load workflow variables from a JSON file ({"KEY": "VALUE"} map).
        #[arg(long, value_name = "FILE")]
        var_file: Option<String>,
        /// Abort after this many seconds if the workflow has not finished.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        /// Exit 1 if any findings at or above this severity exist after the
        /// workflow completes (critical, high, medium, low).
        #[arg(long, value_name = "SEVERITY")]
        fail_on_findings: Option<String>,
    },
    /// List workflow YAML files (.yaml/.yml with phases:) in the current directory.
    List,
    /// Write a starter workflow template (defaults to stdout).
    Init {
        /// Write to this file instead of stdout.
        #[arg(long, short)]
        output: Option<String>,
    },
    /// Show history of locally recorded workflow runs.
    History,
    /// Resume a previously interrupted workflow run.
    Resume {
        /// Run ID shown by `strobes workflow history`.
        id: String,
        /// Print events to stdout instead of opening the TUI.
        #[arg(long)]
        no_tui: bool,
    },
    /// Manage remote workflows via the Strobes GraphQL API.
    Remote {
        #[command(subcommand)]
        sub: RemoteWorkflowCmd,
    },
}

#[derive(Subcommand)]
enum RemoteWorkflowCmd {
    /// List available workflow templates (built-in and custom:).
    Templates,
    /// Show the workflow currently attached to a workspace.
    Status {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Attach a workflow template to a workspace and start it.
    Attach {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Template slug, e.g. "web-pentest" or "custom:my-template".
        /// Omit to pick interactively.
        #[arg(long, short)]
        template: Option<String>,
        /// Set a workflow variable (KEY=VALUE). Repeatable.
        #[arg(long, short = 'v', value_name = "KEY=VALUE")]
        var: Vec<String>,
    },
    /// Detach (cancel + remove) the workflow from a workspace.
    Detach {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Create a new remote workflow from a local YAML file.
    Create {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Local workflow YAML file to push.
        #[arg(long, short)]
        file: String,
        /// Override a workflow variable (KEY=VALUE). Repeatable.
        #[arg(long, short = 'v', value_name = "KEY=VALUE")]
        var: Vec<String>,
    },
    /// Edit the existing remote workflow from a local YAML file.
    /// Not allowed while the workflow is running.
    Edit {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Local workflow YAML file.
        #[arg(long, short)]
        file: String,
    },
    /// Smart sync: push a local YAML to remote — creates if none, edits if one exists.
    Sync {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Local workflow YAML file.
        #[arg(long, short)]
        file: String,
        /// Override a workflow variable (KEY=VALUE). Repeatable.
        #[arg(long, short = 'v', value_name = "KEY=VALUE")]
        var: Vec<String>,
    },
    /// Save the current remote workflow as a reusable custom template.
    Save {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Display name for the template (gets a custom: prefix automatically).
        #[arg(long, short)]
        name: String,
        /// Template description.
        #[arg(long, short = 'd')]
        description: Option<String>,
        /// Emoji icon shown next to the template.
        #[arg(long, short = 'i', default_value = "🔒")]
        icon: String,
    },
    /// Delete a custom workflow template (only custom: slugs can be deleted).
    DeleteTemplate {
        /// Template slug, e.g. "custom:my-template".
        slug: String,
    },
    /// Pause the running workflow.
    Pause {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Resume a paused workflow.
    Resume {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Cancel the running or paused workflow.
    Cancel {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Restart the workflow (from the beginning, or from a specific phase).
    Restart {
        #[arg(long, short)]
        workspace: Option<String>,
        /// Restart from this phase key rather than from the beginning.
        #[arg(long)]
        from_phase: Option<String>,
    },
    /// Advance past a manual-gate phase.
    Advance {
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// Open a live TUI showing workflow phase status with pause/resume/detach controls.
    Watch {
        #[arg(long, short)]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
enum CiCmd {
    /// Run a SAST scan on a local directory using Strobes AI.
    ///
    /// Copies the target directory into a local sandbox, sends a default
    /// SAST prompt to the agent, streams the response, and prints a
    /// structured findings summary. Results can also be saved to a file.
    ///
    /// Examples:
    ///   strobes ci sast .
    ///   strobes ci sast ~/myapp --output json --output-file results.json
    ///   strobes ci sast ./src --fail-on high --timeout 600
    Sast {
        /// Directory to scan (defaults to current directory).
        #[arg(default_value = ".")]
        dir: String,
        /// Output format for the summary: text (default), json, or sarif.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        output: String,
        /// Save scan output to this file (in the chosen format).
        #[arg(long, short = 'o', value_name = "FILE")]
        output_file: Option<String>,
        /// Override the default SAST prompt with a custom one.
        #[arg(long, value_name = "TEXT")]
        prompt: Option<String>,
        /// Attach to an existing workspace by ID (creates a new one if omitted).
        #[arg(long, short, value_name = "UUID")]
        workspace: Option<String>,
        /// LLM model picker id.
        #[arg(long, short)]
        model: Option<i64>,
        /// Abort after this many seconds if the scan has not finished (default: 600).
        #[arg(long, default_value = "600", value_name = "SECS")]
        timeout: u64,
        /// Exit 1 if any findings at or above this severity exist
        /// (critical, high, medium, low).
        #[arg(long, value_name = "SEVERITY")]
        fail_on: Option<String>,
        /// Glob patterns to exclude from the sandbox copy (e.g. "*.lock").
        /// Repeatable. node_modules, .git, and common binary types are always excluded.
        #[arg(long, value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Maximum total size of files to copy into the sandbox (MB, default 100).
        #[arg(long, default_value = "100", value_name = "MB")]
        max_mb: u64,
    },

    /// Scan dependencies for CVEs and use AI to determine true exploitability.
    ///
    /// Parses all supported manifest/lock files in the target directory,
    /// queries OSV.dev for known vulnerabilities (no API key required),
    /// builds the transitive dependency graph, then passes the results and
    /// the codebase to the agent for reachability analysis — so only CVEs
    /// that are genuinely reachable via your code are flagged as findings.
    ///
    /// Supported ecosystems: Python, Node.js, Go, Rust, Ruby, PHP, Java, .NET
    ///
    /// Examples:
    ///   strobes ci sca .
    ///   strobes ci sca ~/myapp --output json -o sca.json
    ///   strobes ci sca . --fail-on high --skip-ai
    ///   strobes ci sca . --min-severity medium
    Sca {
        /// Directory to scan (defaults to current directory).
        #[arg(default_value = ".")]
        dir: String,
        /// Output format: text (default), json, or sarif.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        output: String,
        /// Save output to this file (in the chosen format).
        #[arg(long, short = 'o', value_name = "FILE")]
        output_file: Option<String>,
        /// Attach to an existing workspace by ID.
        #[arg(long, short, value_name = "UUID")]
        workspace: Option<String>,
        /// LLM model picker id.
        #[arg(long, short)]
        model: Option<i64>,
        /// Abort AI analysis after this many seconds (default: 600).
        #[arg(long, default_value = "600", value_name = "SECS")]
        timeout: u64,
        /// Exit 1 if any findings at or above this severity exist.
        #[arg(long, value_name = "SEVERITY")]
        fail_on: Option<String>,
        /// Skip AI reachability analysis — report all CVEs directly.
        #[arg(long)]
        skip_ai: bool,
        /// Minimum CVE severity to include (default: low).
        #[arg(long, default_value = "low", value_name = "SEVERITY")]
        min_severity: String,
    },
    /// Scan a Docker image for OS-level and app-level CVEs using Strobes AI.
    ///
    /// Pulls the image, creates a temporary container to extract the package
    /// database (dpkg/apk/rpm) and any app-level manifests, queries OSV.dev
    /// for CVEs, then passes the results to the AI for reachability analysis.
    ///
    /// Requires Docker to be installed and running.
    ///
    /// Examples:
    ///   strobes ci container nginx:1.24
    ///   strobes ci container python:3.9-slim --skip-ai
    ///   strobes ci container myapp:latest --fail-on critical
    ///   strobes ci container ubuntu:20.04 --output json -o results.json
    Container {
        /// Docker image to scan (e.g. nginx:1.24, myapp:latest).
        image: String,
        /// Output format: text (default), json, or sarif.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        output: String,
        /// Save output to this file (in the chosen format).
        #[arg(long, short = 'o', value_name = "FILE")]
        output_file: Option<String>,
        /// Attach to an existing workspace by ID.
        #[arg(long, short, value_name = "UUID")]
        workspace: Option<String>,
        /// LLM model picker id.
        #[arg(long, short)]
        model: Option<i64>,
        /// Abort AI analysis after this many seconds (default: 600).
        #[arg(long, default_value = "600", value_name = "SECS")]
        timeout: u64,
        /// Exit 1 if any findings at or above this severity exist.
        #[arg(long, value_name = "SEVERITY")]
        fail_on: Option<String>,
        /// Skip AI reachability analysis — report all CVEs directly.
        #[arg(long)]
        skip_ai: bool,
        /// Minimum CVE severity to include (default: low).
        #[arg(long, default_value = "low", value_name = "SEVERITY")]
        min_severity: String,
        /// Target platform for multi-arch images (e.g. linux/amd64).
        #[arg(long, value_name = "PLATFORM")]
        platform: Option<String>,
    },

    /// Scan Infrastructure-as-Code files for security misconfigurations.
    ///
    /// Auto-detects Terraform, CloudFormation, Kubernetes, Helm, Dockerfile,
    /// Docker Compose, GitHub Actions, Ansible, and ARM templates. Copies only
    /// the IaC files into a sandbox and uses AI to find real misconfigurations.
    ///
    /// Examples:
    ///   strobes ci iac .
    ///   strobes ci iac ./infra --output json -o iac.json
    ///   strobes ci iac . --fail-on high
    ///   strobes ci iac . --only terraform --only kubernetes
    Iac {
        /// Directory to scan (defaults to current directory).
        #[arg(default_value = ".")]
        dir: String,
        /// Output format: text (default), json, or sarif.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        output: String,
        /// Save output to this file (in the chosen format).
        #[arg(long, short = 'o', value_name = "FILE")]
        output_file: Option<String>,
        /// Attach to an existing workspace by ID.
        #[arg(long, short, value_name = "UUID")]
        workspace: Option<String>,
        /// LLM model picker id.
        #[arg(long, short)]
        model: Option<i64>,
        /// Abort after this many seconds (default: 600).
        #[arg(long, default_value = "600", value_name = "SECS")]
        timeout: u64,
        /// Exit 1 if any findings at or above this severity exist.
        #[arg(long, value_name = "SEVERITY")]
        fail_on: Option<String>,
        /// Restrict scan to specific IaC types (repeatable).
        /// Values: terraform, cloudformation, kubernetes, helm,
        ///         dockerfile, compose, github-actions, ansible, arm
        #[arg(long, value_name = "TYPE")]
        only: Vec<String>,
    },

    /// Run a DAST scan against a live URL using Strobes AI.
    ///
    /// Sends the target URL to the agent which actively probes it via HTTP
    /// requests, browser navigation, and fuzzing. No files are copied —
    /// the target must be reachable from this machine.
    ///
    /// Examples:
    ///   strobes ci dast http://localhost:5000
    ///   strobes ci dast https://staging.myapp.com --output json -o dast.json
    ///   strobes ci dast http://localhost:3000 --cookie "session=abc123" --fail-on high
    ///   strobes ci dast http://app.local --scope /api --scope /admin
    Dast {
        /// Target base URL to scan (required).
        url: String,
        /// Output format: text (default), json, or sarif.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        output: String,
        /// Save scan output to this file (in the chosen format).
        #[arg(long, short = 'o', value_name = "FILE")]
        output_file: Option<String>,
        /// Override the default DAST prompt with a custom one.
        #[arg(long, value_name = "TEXT")]
        prompt: Option<String>,
        /// Cookie header value to include in all requests (e.g. "session=abc; csrf=xyz").
        #[arg(long, value_name = "COOKIE")]
        cookie: Option<String>,
        /// Bearer token for Authorization header.
        #[arg(long, value_name = "TOKEN")]
        bearer: Option<String>,
        /// Restrict crawl + testing to these path prefixes (e.g. /api). Repeatable.
        #[arg(long, value_name = "PATH")]
        scope: Vec<String>,
        /// Attach to an existing workspace by ID (creates a new one if omitted).
        #[arg(long, short, value_name = "UUID")]
        workspace: Option<String>,
        /// LLM model picker id.
        #[arg(long, short)]
        model: Option<i64>,
        /// Abort after this many seconds if the scan has not finished (default: 900).
        #[arg(long, default_value = "900", value_name = "SECS")]
        timeout: u64,
        /// Exit 1 if any findings at or above this severity exist
        /// (critical, high, medium, low).
        #[arg(long, value_name = "SEVERITY")]
        fail_on: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = Config::load();
    // Select the tenant for this run without mutating the stored default.
    let tenant = cli.tenant.clone().unwrap_or_else(|| cfg.current_profile.clone());
    let profile = cfg.profile_for(&tenant);

    match cli.cmd.unwrap_or(Cmd::Chat { thread: None, workspace: None, model: None, new: false }) {
        Cmd::Login { base_url, org_id, master_key, deployment, no_verify } => {
            cmd_login(&mut cfg, &tenant, base_url, org_id, master_key, deployment, no_verify).await
        }
        Cmd::Update { force } => cmd_update(force).await,
        Cmd::Tenants => cmd_tenants(&cfg),
        Cmd::Credits { workspace, thread } => cmd_credits(&profile, workspace, thread).await,
        Cmd::Status => cmd_status(&profile).await,
        Cmd::Workspaces => cmd_workspaces(&profile).await,
        Cmd::Threads => cmd_threads(&profile).await,
        Cmd::Bind { workspace, new, name, download, dir } => {
            cmd_bind(&mut cfg, &tenant, profile, workspace, new, name, download, dir).await
        }
        Cmd::Pull { workspace, dir } => cmd_pull(&mut cfg, &profile, workspace, dir).await,
        Cmd::Push { files, workspace, dir } => cmd_push(&cfg, &profile, files, workspace, dir).await,
        Cmd::Chat { thread, workspace, model, new } => {
            // Enter the alternate screen ONCE for the whole interactive flow
            // (pickers + chat) and restore ONCE, so switching between workspace,
            // thread, and chat never flashes the normal terminal.
            let mut terminal = ratatui::init();
            // Capture the mouse so trackpad/wheel scroll moves the transcript.
            // Native text selection/copy still works via Option/Shift-drag, and
            // ^Y copies the whole transcript.
            //
            // NOTE: we deliberately do NOT enable the kitty keyboard protocol
            // here. On terminals that support it, it changes ESC[-sequence
            // parsing and collides with SGR mouse reports — leaking raw scroll
            // sequences (e.g. `[<64;..M`) into the input. Newlines use Ctrl+J.
            enable_mouse();
            let r = chat_flow(&mut terminal, &mut cfg, &tenant, profile, thread, workspace, new, model).await;
            disable_mouse();
            ratatui::restore();
            r
        }
        Cmd::Probe { thread, send, secs, model } => cmd_probe(&profile, &thread, send, secs, model).await,
        Cmd::Workflow { sub } => cmd_workflow(profile, sub, &tenant).await,
        Cmd::Send { message, workspace, new_workspace, title, model, output, non_interactive, timeout, fail_on_findings, sandbox_id, agent } => {
            cmd_send(&profile, message, workspace, new_workspace, title, model, &output, non_interactive, timeout, fail_on_findings, sandbox_id, agent).await
        }
        Cmd::Ci { sub } => match sub {
            CiCmd::Sast { dir, output, output_file, prompt, workspace, model, timeout, fail_on, exclude, max_mb } => {
                cmd_scan_sast(&profile, dir, output, output_file, prompt, workspace, model, timeout, fail_on, exclude, max_mb).await
            }
            CiCmd::Sca { dir, output, output_file, workspace, model, timeout, fail_on, skip_ai, min_severity } => {
                cmd_scan_sca(&profile, dir, output, output_file, workspace, model, timeout, fail_on, skip_ai, min_severity).await
            }
            CiCmd::Container { image, output, output_file, workspace, model, timeout, fail_on, skip_ai, min_severity, platform } => {
                cmd_scan_container(&profile, image, output, output_file, workspace, model, timeout, fail_on, skip_ai, min_severity, platform).await
            }
            CiCmd::Iac { dir, output, output_file, workspace, model, timeout, fail_on, only } => {
                cmd_ci_iac(&profile, dir, output, output_file, workspace, model, timeout, fail_on, only).await
            }
            CiCmd::Dast { url, output, output_file, prompt, cookie, bearer, scope, workspace, model, timeout, fail_on } => {
                cmd_scan_dast(&profile, url, output, output_file, prompt, cookie, bearer, scope, workspace, model, timeout, fail_on).await
            }
        },
        Cmd::Findings { workspace, format, fail_on } => {
            cmd_findings(&profile, workspace, &format, fail_on).await
        }
        Cmd::Export { workspace, thread, dir, format, messages_only } => {
            cmd_export(&profile, workspace, thread, dir, &format, messages_only).await
        }
    }
}

/// The interactive chat flow: pick a workspace → thread (unless given), persist
/// the binding, then run the chat UI — all on one already-initialized terminal.
async fn chat_flow(
    terminal: &mut ratatui::DefaultTerminal,
    cfg: &mut Config,
    tenant: &str,
    mut profile: config::Profile,
    thread: Option<String>,
    workspace: Option<String>,
    new: bool,
    model: Option<i64>,
) -> Result<()> {
    if let Some(w) = &workspace {
        profile.workspace_id = Some(w.clone());
    }
    // Only an explicit --thread skips the pickers; a plain run always offers
    // workspace → thread selection (the stored thread_id is not auto-resumed).
    let explicit = if new { None } else { thread };
    // `initial_msg` is auto-sent on connect (used to seed a new workspace's
    // setup chat with the user's prompt).
    let mut initial_msg: Option<String> = None;
    let thread_id = match explicit {
        Some(t) => t,
        None => {
            // Esc on the thread picker steps back to the workspace picker; Esc on
            // the workspace picker (or ^C anywhere) quits cleanly.
            let ws_flag = workspace.is_some();
            loop {
                if !ws_flag {
                    match resolve_workspace_interactive(terminal, tenant, &profile, cfg).await? {
                        WsChoice::Pick(w) => {
                            // Count this open for "recent" ranking next time.
                            if let Some(id) = &w {
                                cfg.record_workspace_open(id);
                            }
                            profile.workspace_id = w;
                        }
                        WsChoice::Created { id, setup_thread, prompt } => {
                            cfg.record_workspace_open(&id);
                            profile.workspace_id = Some(id);
                            if let Some(t) = setup_thread {
                                // Jump straight into the setup chat with the prompt.
                                initial_msg = prompt;
                                break t;
                            }
                            // No setup thread → fall through to the thread picker.
                        }
                        WsChoice::Quit => return Ok(()),
                    }
                }
                match resolve_thread_interactive(terminal, tenant, &profile).await? {
                    ThreadChoice::Pick(t) => break t,
                    ThreadChoice::Quit => return Ok(()),
                    // No workspace picker to go back to when --workspace was given.
                    ThreadChoice::Back if ws_flag => return Ok(()),
                    ThreadChoice::Back => continue,
                }
            }
        }
    };
    // Persist the binding to the selected tenant's profile.
    {
        let p = cfg.profile_mut(tenant);
        p.thread_id = Some(thread_id.clone());
        if profile.workspace_id.is_some() {
            p.workspace_id = profile.workspace_id.clone();
        }
        let _ = cfg.save();
    }
    run_chat(terminal, tenant, profile, thread_id, model, initial_msg).await
}

/// Show a workspace picker over the available workspaces and return the chosen
/// workspace id. A leading "No workspace" entry (and an empty list) yields None,
/// leaving the chat unscoped. The currently-bound workspace is marked with ✓.
/// First-screen workspace choice. Esc/^C both quit the app cleanly (there's no
/// earlier screen to step back to).
enum WsChoice {
    Pick(Option<String>),
    /// A freshly-created workspace: bind it, open its setup thread, and send the
    /// optional setup prompt as the first message.
    Created {
        id: String,
        setup_thread: Option<String>,
        prompt: Option<String>,
    },
    Quit,
}

/// One-line "authenticated …" subtitle shown under the banner art.
fn auth_line(p: &config::Profile, tenant: &str) -> String {
    let org = if p.org_id.len() > 8 { &p.org_id[..8] } else { &p.org_id };
    format!("✔ authenticated · tenant: {tenant} · {} · org {org}", p.base_url)
}

async fn resolve_workspace_interactive(
    terminal: &mut ratatui::DefaultTerminal,
    tenant: &str,
    profile: &config::Profile,
    cfg: &Config,
) -> Result<WsChoice> {
    require_complete(profile)?;
    let auth = auth_line(profile, tenant);
    let client = api::ApiClient::new(profile.clone())?;
    let mut workspaces = client.list_workspaces().await.unwrap_or_default();
    // Surface frequently-opened workspaces first (most opens, then most recent).
    // Stable sort keeps never-opened ones in the server's order below.
    workspaces.sort_by(|a, b| {
        cfg.workspace_open_count(&b.id)
            .cmp(&cfg.workspace_open_count(&a.id))
            .then(cfg.workspace_last_opened(&b.id).cmp(&cfg.workspace_last_opened(&a.id)))
    });

    // Fetch workflow state for every workspace concurrently so we can badge rows.
    let wf_states: Vec<Option<api::WorkflowState>> = {
        let futs: Vec<_> = workspaces.iter().map(|w| client.workspace_workflow(&w.id)).collect();
        futures_util::future::join_all(futs)
            .await
            .into_iter()
            .map(|r| r.ok().flatten())
            .collect()
    };

    let cur = profile.workspace_id.as_deref();
    // Item 0 = create new, item 1 = no workspace, then existing workspaces (offset 2).
    let mut labels = vec![
        "➕  New workspace".to_string(),
        "↪  No workspace (all threads)".to_string(),
    ];
    for (w, wf) in workspaces.iter().zip(wf_states.iter()) {
        let name = if w.name.is_empty() { "(unnamed)" } else { &w.name };
        let mark = if Some(w.id.as_str()) == cur { "  ✓" } else { "" };
        let count = cfg.workspace_open_count(&w.id);
        let recent = if count > 0 { format!("  · ↻ ×{count}") } else { String::new() };
        let wf_badge = match wf.as_ref().map(|s| s.status.as_str()) {
            Some("running") => "  ⚙ running",
            Some("paused") => "  ⚙ paused",
            Some("pending") => "  ⚙ pending",
            Some("failed") => "  ⚙ failed",
            Some("completed") => "  ⚙ done",
            _ => "",
        };
        labels.push(format!("{name}   [{}]{mark}{recent}{wf_badge}", w.status));
    }

    // Add Tab hint as a context line below the auth banner.
    let auth_with_hint = format!("{auth}\n↹ Tab on a workspace: open live workflow TUI");

    loop {
        match picker::select_with(terminal, "Select a workspace", &labels, &auth_with_hint).await? {
            picker::Nav::Item(0) => {
                // Ask for a name and an optional setup prompt; Esc on the name
                // cancels back to the picker.
                let name = match picker::prompt_text(terminal, "New workspace — name", "", &auth).await? {
                    Some(n) => n.trim().to_string(),
                    None => continue,
                };
                let name = if name.is_empty() { "CLI Workspace".to_string() } else { name };
                let prompt = picker::prompt_text(
                    terminal,
                    "Setup prompt — what should this workspace do? (optional)",
                    "",
                    &auth,
                )
                .await?
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
                let (id, setup_thread) = client.create_workspace(&name).await?;
                return Ok(WsChoice::Created { id, setup_thread, prompt });
            }
            picker::Nav::Item(1) => return Ok(WsChoice::Pick(None)),
            picker::Nav::Item(i) => return Ok(WsChoice::Pick(Some(workspaces[i - 2].id.clone()))),
            // Tab on a workspace row → open the Watch TUI inline if a workflow exists.
            picker::Nav::Shortcut(i) if i >= 2 => {
                let ws = &workspaces[i - 2];
                if wf_states[i - 2].is_some() {
                    let _ = remote_wf_tui::run(terminal, &client, ws.id.clone(), profile.clone(), tenant.to_string()).await;
                    terminal.clear()?;
                }
                // Fall through to re-show the workspace picker.
            }
            picker::Nav::Shortcut(_) => {} // Tab on "New" / "No workspace" — ignore.
            // Esc on the first screen quits cleanly (no prior screen to return to).
            picker::Nav::Back | picker::Nav::Quit => return Ok(WsChoice::Quit),
        }
    }
}

/// Second-screen thread choice. Esc steps back to the workspace picker; ^C quits.
enum ThreadChoice {
    Pick(String),
    Back,
    Quit,
}

/// Show a thread picker (with a "new thread" option) and return the chosen
/// thread id, creating one via REST if requested.
async fn resolve_thread_interactive(
    terminal: &mut ratatui::DefaultTerminal,
    tenant: &str,
    profile: &config::Profile,
) -> Result<ThreadChoice> {
    require_complete(profile)?;
    let auth = auth_line(profile, tenant);
    let client = api::ApiClient::new(profile.clone())?;
    // Scope to the bound workspace if there is one.
    let threads = client
        .list_threads(profile.workspace_id.as_deref())
        .await
        .unwrap_or_default();

    // Context lines under the ASCII art: which workspace these threads belong
    // to, plus its headline counts (threads · credits · findings · files).
    let auth = match profile.workspace_id.as_deref() {
        Some(id) => {
            // Resolve the name and the stats counts concurrently.
            let (workspaces, (credits, findings, files)) =
                tokio::join!(client.list_workspaces(), fetch_workspace_stats(&client, id));
            let name = workspaces
                .ok()
                .and_then(|ws| ws.into_iter().find(|w| w.id.as_str() == id).map(|w| w.name))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("{}…", &id[..8.min(id.len())]));
            let stats = format!(
                "{} threads  ·  ◈ {credits:.2} credits  ·  ⚠ {findings} findings  ·  {files} files",
                threads.len()
            );
            format!("{auth}\n⊞ workspace: {name}\n{stats}")
        }
        None => format!("{auth}\n⊞ workspace: (none — all threads)"),
    };

    let mut labels = vec!["➕  New thread".to_string()];
    for t in &threads {
        let title = if t.title.is_empty() { "(untitled)".into() } else { t.title.clone() };
        let last = if t.last_message.is_empty() { String::new() } else { format!("  — {}", trunc(&t.last_message, 50)) };
        labels.push(format!("{title}   [{}]{last}", t.status));
    }

    match picker::select_with(terminal, "Select a thread", &labels, &auth).await? {
        picker::Nav::Item(0) => {
            let id = client
                .create_thread("CLI session", profile.workspace_id.as_deref(), None)
                .await?;
            Ok(ThreadChoice::Pick(id))
        }
        picker::Nav::Item(i) => Ok(ThreadChoice::Pick(threads[i - 1].id.clone())),
        picker::Nav::Back | picker::Nav::Shortcut(_) => Ok(ThreadChoice::Back),
        picker::Nav::Quit => Ok(ThreadChoice::Quit),
    }
}

async fn cmd_bind(
    cfg: &mut Config,
    tenant: &str,
    profile: config::Profile,
    workspace: Option<String>,
    new: bool,
    name: String,
    download: bool,
    dir: Option<String>,
) -> Result<()> {
    require_complete(&profile)?;
    let client = api::ApiClient::new(profile.clone())?;

    let ws_id = if new {
        let (id, _setup) = client.create_workspace(&name).await?;
        println!("✔ created workspace {id} ({name})");
        id
    } else if let Some(w) = workspace {
        w
    } else {
        let workspaces = client.list_workspaces().await?;
        if workspaces.is_empty() {
            return Err(anyhow!("no workspaces — use --new to create one"));
        }
        let labels: Vec<String> = workspaces
            .iter()
            .map(|w| format!("{}   [{}]", if w.name.is_empty() { "(unnamed)" } else { &w.name }, w.status))
            .collect();
        match picker::select("Select a workspace", &labels).await? {
            picker::Nav::Item(i) => workspaces[i].id.clone(),
            _ => return Ok(()),
        }
    };

    cfg.profile_mut(tenant).workspace_id = Some(ws_id.clone());
    cfg.save()?;
    println!("✔ bound workspace {ws_id}");

    if download {
        download_workspace(cfg, &profile, &ws_id, dir).await?;
    } else {
        println!("(run `strobes pull` to download its files locally)");
    }
    Ok(())
}

async fn cmd_pull(
    cfg: &mut Config,
    profile: &config::Profile,
    workspace: Option<String>,
    dir: Option<String>,
) -> Result<()> {
    require_complete(profile)?;
    let ws_id = workspace
        .or_else(|| profile.workspace_id.clone())
        .ok_or_else(|| anyhow!("no workspace — pass --workspace <UUID> or run `bind` first"))?;
    download_workspace(cfg, profile, &ws_id, dir).await
}

async fn cmd_push(
    cfg: &Config,
    profile: &config::Profile,
    files: Vec<String>,
    workspace: Option<String>,
    dir: Option<String>,
) -> Result<()> {
    require_complete(profile)?;
    if files.is_empty() {
        return Err(anyhow!("nothing to upload — usage: strobes push <file…> [--workspace <id>] [--dir <dest>]"));
    }
    let ws = workspace
        .or_else(|| profile.workspace_id.clone())
        .ok_or_else(|| anyhow!("no workspace — pass --workspace <UUID> or run `bind` first"))?;
    let client = api::ApiClient::new(profile.clone())?;
    let prefix = dir
        .map(|d| format!("{}/", d.trim_matches('/')))
        .filter(|d| d.len() > 1)
        .unwrap_or_default();
    let sync_roots = workspace_sync_roots(cfg, &ws);
    for f in &files {
        let dest = upload_one(&client, &ws, f, &prefix, &sync_roots).await?;
        println!("✔ {f} → {dest}");
    }
    for root in &sync_roots {
        println!("↕ mirrored into {}", root.display());
    }
    println!("done — {} file(s) → workspace {}", files.len(), &ws[..8.min(ws.len())]);
    Ok(())
}

/// Read a local file and upload it to `ws` under `prefix`. Returns the dest path.
///
/// After the remote upload, the file is mirrored into each of `sync_roots`
/// (the active workspace sandbox and/or a bound local folder) at `dest`, so the
/// local copy the agent sees stays in sync — unless the source already *is*
/// that file.
async fn upload_one(
    client: &api::ApiClient,
    ws: &str,
    local: &str,
    prefix: &str,
    sync_roots: &[std::path::PathBuf],
) -> Result<String> {
    let path = std::path::Path::new(local);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("bad file name: {local}"))?;
    let bytes = std::fs::read(path).map_err(|e| anyhow!("read {local}: {e}"))?;
    let dest = format!("{prefix}{name}");
    client.upload_workspace_file(ws, &dest, bytes.clone()).await?;
    for root in sync_roots {
        mirror_into_folder(root, &dest, path, &bytes);
    }
    Ok(dest)
}

/// Local folders that mirror a workspace: the per-workspace sandbox the chat
/// agent reads from (`config_dir()/workspaces/<ws>`) plus any explicitly bound
/// folder. Only dirs that already exist are returned, so a stray upload never
/// pre-creates the sandbox and tricks `spawn_workspace_sync` into skipping the
/// initial download. Deduplicated.
fn workspace_sync_roots(cfg: &Config, ws: &str) -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    let sandbox = config::config_dir().join("workspaces").join(ws);
    if sandbox.is_dir() {
        roots.push(sandbox);
    }
    if let Some(d) = cfg.workspace_dirs.get(ws) {
        let p = std::path::PathBuf::from(d);
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    roots
}

/// Write `bytes` into `root/dest`, creating parent dirs. No-op if the source
/// file already resolves to that destination (avoids copying onto itself).
fn mirror_into_folder(root: &std::path::Path, dest: &str, src: &std::path::Path, bytes: &[u8]) {
    let target = root.join(dest);
    if let (Ok(a), Ok(b)) = (src.canonicalize(), target.canonicalize()) {
        if a == b {
            return; // already in place
        }
    }
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&target, bytes);
}

/// Download a workspace zip and extract it to a local folder, recording the
/// folder binding in config.
/// Download the workspace zip and extract it into `target`. Returns file count.
async fn extract_workspace_zip(
    client: &api::ApiClient,
    ws_id: &str,
    target: &std::path::Path,
) -> Result<usize> {
    std::fs::create_dir_all(target)?;
    let url = client.workspace_download_url(ws_id).await?;
    let bytes = reqwest::Client::new().get(&url).send().await?.bytes().await?;
    let reader = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)?;
    let mut count = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let out = match entry.enclosed_name() {
            Some(p) => target.join(p),
            None => continue,
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut f)?;
            count += 1;
        }
    }
    Ok(count)
}

async fn download_workspace(
    cfg: &mut Config,
    profile: &config::Profile,
    ws_id: &str,
    dir: Option<String>,
) -> Result<()> {
    let client = api::ApiClient::new(profile.clone())?;
    let target = dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config::config_dir().join("workspaces").join(ws_id));
    println!("• downloading workspace {ws_id} → {}", target.display());
    let count = extract_workspace_zip(&client, ws_id, &target).await?;
    cfg.workspace_dirs.insert(ws_id.to_string(), target.to_string_lossy().to_string());
    cfg.save()?;
    println!("✔ {count} files extracted to {} (folder bound to workspace)", target.display());
    Ok(())
}

/// Sync the bound workspace's files into a local folder and point the agent's
/// local sandbox there, so its (locally-proxied) workspace_get_meta /
/// execute_command see the real workspace files. Mirrors the cloud's S3→sandbox
/// sync. Re-downloads only if the folder is missing/empty.
/// Extract zip bytes into `target`, returning the file count (blocking).
fn extract_zip_bytes(bytes: Vec<u8>, target: &std::path::Path) -> Result<usize> {
    std::fs::create_dir_all(target)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut count = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let out = match entry.enclosed_name() {
            Some(p) => target.join(p),
            None => continue,
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::io::copy(&mut entry, &mut std::fs::File::create(&out)?)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Point the agent's local sandbox at the workspace folder IMMEDIATELY (so the
/// next tool call uses it), then download + extract the files in the BACKGROUND
/// so the UI never freezes. Progress is reported as Notice events on `tx`.
/// Fetch a workspace's headline counts (credits, findings, files) concurrently.
/// Returned as `(credits, findings, files)`; failures degrade to 0.
async fn fetch_workspace_stats(client: &api::ApiClient, ws_id: &str) -> (f64, usize, usize) {
    let (credits, findings, files) = tokio::join!(
        client.get_credits(Some(ws_id), None),
        client.list_workspace_findings(ws_id),
        client.list_workspace_files(ws_id, true),
    );
    (
        credits.map(|c| c.credits).unwrap_or(0.0),
        findings.map(|f| f.len()).unwrap_or(0),
        files.map(|f| f.iter().filter(|x| !x.is_folder).count()).unwrap_or(0),
    )
}

fn spawn_workspace_sync(
    profile: config::Profile,
    ws_id: String,
    tx: mpsc::UnboundedSender<pulse::AppEvent>,
) {
    let dir = config::config_dir().join("workspaces").join(&ws_id);
    // Set the sandbox path right away (instant, non-blocking).
    std::env::set_var("STROBES_AI_SANDBOX", &dir);
    std::env::set_var("STROBES_AI_WORKSPACE_ID", &ws_id);

    let already = dir.is_dir()
        && std::fs::read_dir(&dir).map(|mut d| d.next().is_some()).unwrap_or(false);
    if already {
        let _ = tx.send(pulse::AppEvent::Notice(format!(
            "workspace files at {} (cached)", dir.display()
        )));
        return;
    }

    tokio::spawn(async move {
        let _ = tx.send(pulse::AppEvent::Notice("syncing workspace files locally…".into()));
        let client = match api::ApiClient::new(profile) {
            Ok(c) => c,
            Err(e) => { let _ = tx.send(pulse::AppEvent::Error(e.to_string())); return; }
        };
        match client.download_workspace_bytes(&ws_id).await {
            Ok(bytes) => {
                let d = dir.clone();
                let res = tokio::task::spawn_blocking(move || extract_zip_bytes(bytes, &d)).await;
                match res {
                    Ok(Ok(n)) => {
                        let _ = tx.send(pulse::AppEvent::Notice(format!(
                            "✔ synced {n} workspace files → {}", dir.display()
                        )));
                    }
                    Ok(Err(e)) => { let _ = tx.send(pulse::AppEvent::Error(format!("workspace extract failed: {e}"))); }
                    Err(e) => { let _ = tx.send(pulse::AppEvent::Error(format!("extract task failed: {e}"))); }
                }
            }
            Err(e) => {
                // An empty workspace (nothing to zip) is benign — show a notice,
                // not a red error. Detect the backend's "no files" case / 404.
                let msg = e.to_string();
                if msg.contains("No files found to download") || msg.contains("HTTP 404") {
                    let _ = tx.send(pulse::AppEvent::Notice(
                        "workspace has no files yet — nothing to sync".into(),
                    ));
                } else {
                    let _ = tx.send(pulse::AppEvent::Error(format!("workspace download failed: {e}")));
                }
            }
        }
    });
}

async fn cmd_probe(p: &config::Profile, thread_id: &str, send: Option<String>, secs: u64, model: Option<i64>) -> Result<()> {
    require_complete(p)?;
    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let handle = pulse::connect(p, thread_id, tx, model).await?;
    println!("[probe] connected to {}", p.pulse_ws_url(thread_id)?.split('?').next().unwrap_or(""));
    if let Some(text) = send {
        handle.send_user_message(&text);
        println!("[probe] sent: {text}");
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => { println!("[probe] timeout"); break; }
            maybe = rx.recv() => match maybe {
                None => break,
                Some(ev) => {
                    println!("[probe] {}", describe(&ev));
                    match ev {
                        pulse::AppEvent::RunFinished(_) => break,
                        pulse::AppEvent::Interrupt { id, fields, .. } => {
                            // Auto-answer every field with a canned value.
                            let mut resp = serde_json::Map::new();
                            for f in &fields {
                                resp.insert(f.key.clone(), serde_json::Value::from("STROBES_TEST_7788"));
                            }
                            handle.respond_interrupt(&id, serde_json::Value::Object(resp));
                            println!("[probe] auto-answered interrupt {id}");
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

/// Create a new thread, send one message, stream the response to stdout.
///
/// Tokens are printed inline, tool events are prefixed with ▶/◀/✗, and the
/// process exits with a non-zero code on agent errors or interrupt in
/// non-interactive mode. Pass `--output json` for machine-readable output.
#[allow(clippy::too_many_arguments)]
async fn cmd_send(
    p: &config::Profile,
    message: String,
    workspace: Option<String>,
    new_workspace: Option<String>,
    title: Option<String>,
    model: Option<i64>,
    output_fmt: &str,
    non_interactive: bool,
    timeout_secs: Option<u64>,
    fail_on_findings: Option<String>,
    sandbox_id: Option<String>,
    agent: Option<String>,
) -> Result<()> {
    require_complete(p)?;

    // If a sandbox_id was supplied, create the directory and point the
    // STROBES_AI_SANDBOX env var at it.  sandbox_dir_for() checks this var
    // first, so every local tool call (execute_command, execute_code, …) will
    // use that directory as its working dir without any other code changes.
    if let Some(ref sid) = sandbox_id {
        let safe: String = sid
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let sandbox_path = home.join(".strobes-ai").join("sandboxes").join(&safe);
        std::fs::create_dir_all(&sandbox_path)?;
        // Safety: we set this before any threads are spawned that read it.
        #[allow(deprecated)]
        std::env::set_var("STROBES_AI_SANDBOX", &sandbox_path);
        eprintln!("sandbox: {}", sandbox_path.display());
    }

    let client = api::ApiClient::new(p.clone())?;

    let workspace_id: Option<String> = match (workspace, new_workspace) {
        (Some(ws), _) => Some(ws),
        (None, Some(name)) => {
            let (id, _) = client.create_workspace(&name).await?;
            eprintln!("workspace: {id}");
            Some(id)
        }
        (None, None) => None,
    };

    let thread_title = title.unwrap_or_else(|| {
        let truncated: String = message.chars().take(60).collect();
        if message.chars().count() > 60 { format!("{truncated}…") } else { truncated }
    });

    let thread_id = client.create_thread(&thread_title, workspace_id.as_deref(), agent.as_deref()).await?;
    eprintln!("thread: {thread_id}");

    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let handle = pulse::connect(p, &thread_id, tx, model).await?;
    handle.send_user_message(&message);

    let json_mode = output_fmt == "json";
    let mut json_text = String::new();
    let mut json_tools: Vec<serde_json::Value> = Vec::new();
    let mut json_status = "success".to_string();
    let mut json_error: Option<String> = None;

    let deadline = timeout_secs
        .map(|s| tokio::time::Instant::now() + std::time::Duration::from_secs(s));

    let mut needs_newline = false;
    let run_result: Result<()> = loop {
        let ev_opt = if let Some(dl) = deadline {
            tokio::select! {
                _ = tokio::time::sleep_until(dl) => {
                    json_status = "timeout".into();
                    json_error = Some(format!("timed out after {}s", timeout_secs.unwrap_or(0)));
                    if !json_mode {
                        if needs_newline { println!(); }
                        eprintln!("error: timed out after {}s", timeout_secs.unwrap_or(0));
                    }
                    break Err(anyhow!("timed out after {}s", timeout_secs.unwrap_or(0)));
                }
                ev = rx.recv() => ev,
            }
        } else {
            rx.recv().await
        };

        match ev_opt {
            None => {
                if !json_mode && needs_newline { println!(); }
                break Ok(());
            }
            Some(ev) => match ev {
                pulse::AppEvent::RunFinished(_) => {
                    if !json_mode && needs_newline { println!(); }
                    break Ok(());
                }
                pulse::AppEvent::Stream(item) => match item.kind.as_str() {
                    "token" => {
                        if let Some(text) = &item.text {
                            if json_mode {
                                json_text.push_str(text);
                            } else {
                                print!("{text}");
                                let _ = std::io::Write::flush(&mut std::io::stdout());
                                needs_newline = !text.ends_with('\n');
                            }
                        }
                    }
                    "thinking" => {
                        if let Some(text) = &item.text {
                            if !json_mode {
                                if needs_newline { println!(); needs_newline = false; }
                                println!("💭 {text}");
                            }
                        }
                    }
                    "tool_start" => {
                        let name = item.tool_name.as_deref().unwrap_or("?").to_string();
                        let detail = item.detail.as_deref().unwrap_or("").to_string();
                        if json_mode {
                            json_tools.push(serde_json::json!({
                                "name": name, "status": "running", "detail": detail
                            }));
                        } else {
                            if needs_newline { println!(); needs_newline = false; }
                            if detail.is_empty() { println!("▶ {name}"); } else { println!("▶ {name}({detail})"); }
                        }
                    }
                    "tool_output" => {
                        let name = item.tool_name.as_deref().unwrap_or("?").to_string();
                        let detail = item.detail.as_deref().unwrap_or("").to_string();
                        if json_mode {
                            if let Some(last) = json_tools.iter_mut().rev().find(|t| t["name"] == name && t["status"] == "running") {
                                last["status"] = serde_json::Value::String("done".into());
                                last["output"] = serde_json::Value::String(detail);
                            }
                        } else {
                            if needs_newline { println!(); needs_newline = false; }
                            if !detail.is_empty() { println!("◀ {name}: {detail}"); }
                        }
                    }
                    "tool_failed" => {
                        let name = item.tool_name.as_deref().unwrap_or("?").to_string();
                        let err = item.detail.as_deref().unwrap_or("error").to_string();
                        if json_mode {
                            if let Some(last) = json_tools.iter_mut().rev().find(|t| t["name"] == name && t["status"] == "running") {
                                last["status"] = serde_json::Value::String("failed".into());
                                last["error"] = serde_json::Value::String(err);
                            }
                        } else {
                            if needs_newline { println!(); needs_newline = false; }
                            println!("✗ {name}: {err}");
                        }
                    }
                    "note" | "system" => {
                        if let Some(text) = &item.text {
                            if !json_mode {
                                if needs_newline { println!(); needs_newline = false; }
                                println!("ℹ {text}");
                            }
                        }
                    }
                    _ => {
                        if let Some(text) = &item.text {
                            if json_mode {
                                json_text.push_str(text);
                            } else {
                                print!("{text}");
                                let _ = std::io::Write::flush(&mut std::io::stdout());
                                needs_newline = !text.ends_with('\n');
                            }
                        }
                    }
                },
                pulse::AppEvent::Error(e) => {
                    json_status = "error".into();
                    json_error = Some(e.clone());
                    if !json_mode {
                        if needs_newline { println!(); needs_newline = false; }
                        eprintln!("error: {e}");
                    }
                    break Err(anyhow!("agent error: {e}"));
                }
                pulse::AppEvent::Interrupt { id, title, message: msg, fields } => {
                    if non_interactive {
                        json_status = "interrupted".into();
                        json_error = Some(format!("agent requested human input: {title}"));
                        if !json_mode {
                            if needs_newline { println!(); needs_newline = false; }
                            eprintln!("error: agent requested human input ({title}) — re-run interactively or suppress with --non-interactive");
                        }
                        break Err(anyhow!("agent requested human input in non-interactive mode: {title}"));
                    }
                    if !json_mode {
                        if needs_newline { println!(); needs_newline = false; }
                        println!("\n[interrupt] {title}");
                        if !msg.is_empty() { println!("{msg}"); }
                    }
                    let mut resp = serde_json::Map::new();
                    for f in &fields {
                        let secret = f.ftype == "password";
                        let val = prompt_line(&f.label, "", secret)?;
                        resp.insert(f.key.clone(), serde_json::Value::from(val));
                    }
                    handle.respond_interrupt(&id, serde_json::Value::Object(resp));
                }
                _ => {}
            },
        }
    };

    if json_mode {
        let obj = serde_json::json!({
            "thread_id": thread_id,
            "workspace_id": workspace_id,
            "status": json_status,
            "text": json_text,
            "tools": json_tools,
            "error": json_error,
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
    }

    // Propagate any run-level error before checking findings.
    run_result?;

    // --fail-on-findings: fetch findings and gate on severity threshold.
    if let (Some(threshold), Some(ws)) = (&fail_on_findings, &workspace_id) {
        let level = severity_level(threshold);
        let findings = client.list_workspace_findings(ws).await.unwrap_or_default();
        let matching: Vec<_> = findings.iter()
            .filter(|f| severity_level(&f.severity_label) >= level)
            .collect();
        if !matching.is_empty() {
            eprintln!("findings: {} finding(s) at or above '{threshold}' severity", matching.len());
            for f in &matching {
                eprintln!("  [{}] {}", f.severity_label, f.title);
            }
            return Err(anyhow!("{} finding(s) at or above '{}' severity — failing build", matching.len(), threshold));
        }
    }

    Ok(())
}

/// Map a Strobes severity label to a numeric level for threshold comparisons.
fn severity_level(label: &str) -> u8 {
    match label.to_lowercase().as_str() {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// strobes ci sast
// ─────────────────────────────────────────────────────────────────────────────

/// Directories and extensions that are never useful to send to an LLM.
const SAST_SKIP_DIRS: &[&str] = &[
    ".git", ".svn", ".hg", "node_modules", "__pycache__", ".pytest_cache",
    "target", "dist", "build", ".next", ".nuxt", "vendor", "venv", ".venv",
    "env", ".env", ".tox", "coverage", ".nyc_output", ".cache",
];
const SAST_SKIP_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "ico", "webp", "bmp",
    "pdf", "zip", "tar", "gz", "bz2", "xz", "7z", "rar",
    "exe", "dll", "so", "dylib", "a", "o", "wasm",
    "mp3", "mp4", "wav", "ogg", "flac", "avi", "mov",
    "ttf", "otf", "woff", "woff2", "eot",
    "lock",  // package lock files — too noisy
];

/// Recursively copy `src` into `dst`, skipping binary/noise paths.
/// Returns the number of files copied and total bytes.
fn copy_dir_to_sandbox(
    src: &std::path::Path,
    dst: &std::path::Path,
    extra_exclude: &[String],
    max_bytes: u64,
) -> Result<(u64, u64)> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    copy_dir_inner(src, src, dst, extra_exclude, max_bytes, &mut files, &mut bytes)?;
    Ok((files, bytes))
}

fn copy_dir_inner(
    root: &std::path::Path,
    src: &std::path::Path,
    dst: &std::path::Path,
    extra_exclude: &[String],
    max_bytes: u64,
    files: &mut u64,
    bytes: &mut u64,
) -> Result<()> {
    for entry in std::fs::read_dir(src)
        .map_err(|e| anyhow!("cannot read {}: {}", src.display(), e))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let path = entry.path();

        // Skip hidden dirs, noise dirs, and user exclusions.
        if SAST_SKIP_DIRS.iter().any(|d| *d == name_str)
            || extra_exclude.iter().any(|p| {
                glob::Pattern::new(p)
                    .map(|pat| pat.matches(&name_str))
                    .unwrap_or(false)
            })
        {
            continue;
        }

        if path.is_dir() {
            let sub = dst.join(&name);
            std::fs::create_dir_all(&sub)?;
            copy_dir_inner(root, &path, &sub, extra_exclude, max_bytes, files, bytes)?;
        } else if path.is_file() {
            // Skip by extension.
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if SAST_SKIP_EXTS.iter().any(|e| *e == ext) {
                continue;
            }
            let meta = std::fs::metadata(&path)?;
            let fsize = meta.len();
            // Skip files over 512 KB (single file) — likely generated/binary.
            if fsize > 512 * 1024 {
                continue;
            }
            if *bytes + fsize > max_bytes {
                return Err(anyhow!(
                    "sandbox size limit reached ({} MB) — use --max-mb to increase or --exclude to narrow scope",
                    max_bytes / 1_000_000
                ));
            }
            let dst_file = dst.join(&name);
            std::fs::copy(&path, &dst_file)?;
            *files += 1;
            *bytes += fsize;
        }
    }
    Ok(())
}

/// Build the default SAST prompt for a standalone scan.
fn build_sast_prompt(dir: &std::path::Path) -> String {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("codebase");
    format!(
        r#"You are a security engineer performing a comprehensive SAST (Static Application Security Testing) scan.

The codebase "{name}" is available in your local sandbox. Use your tools to enumerate and read the source files.

## Scan scope
Analyse all source files for security vulnerabilities. Focus on:
- Injection flaws: SQL, command, LDAP, XPath, template injection
- Broken authentication and session management
- Sensitive data exposure: hardcoded secrets, keys, tokens, passwords
- Insecure direct object references (IDOR)
- Security misconfigurations: debug flags, permissive CORS, open redirects
- Cross-site scripting (XSS) and cross-site request forgery (CSRF)
- Insecure deserialization
- Path traversal and local/remote file inclusion
- Vulnerable or outdated dependencies (check requirements.txt, package.json, etc.)
- SSRF and XXE
- Missing input validation and unsafe type coercions
- Weak cryptography or use of broken algorithms
- Business logic flaws that could be exploited

## Output format
For EVERY finding, provide:
1. A short title
2. Severity: critical / high / medium / low
3. File path and line number (exact)
4. A concise description of the vulnerability
5. A recommended fix

After your analysis, output a JSON block EXACTLY like this (no trailing text after it):

```json
{{
  "scan_target": "{name}",
  "findings": [
    {{
      "severity": "critical",
      "title": "SQL Injection in login handler",
      "file": "app.py",
      "line": 42,
      "description": "User input is interpolated directly into a SQL query without parameterisation.",
      "fix": "Use parameterised queries: cursor.execute('SELECT * FROM users WHERE id = %s', (user_id,))"
    }}
  ]
}}
```

If no findings are found, output `{{"scan_target": "{name}", "findings": []}}`.
Begin the scan now."#
    )
}

/// A finding parsed from the agent's JSON output block.
#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
struct SastFinding {
    severity: String,
    title: String,
    file: String,
    #[serde(default)]
    line: Option<u64>,
    description: String,
    #[serde(default)]
    fix: String,
}

/// Extract the first ```json … ``` block from the agent's text and parse findings.
fn parse_sast_output(text: &str) -> Vec<SastFinding> {
    // Try to find a ```json ... ``` code block.
    let json_str = if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        if let Some(end) = after.find("```") {
            after[..end].trim()
        } else {
            after.trim()
        }
    } else if let Some(start) = text.find('{') {
        // Fallback: grab the last occurrence of a top-level JSON object.
        let end = text.rfind('}').map(|e| e + 1).unwrap_or(text.len());
        &text[start..end]
    } else {
        return vec![];
    };

    #[derive(serde::Deserialize)]
    struct Wrapper {
        findings: Vec<SastFinding>,
    }
    serde_json::from_str::<Wrapper>(json_str)
        .map(|w| w.findings)
        .unwrap_or_default()
}

/// Severity ordering for sorting and threshold checks.
fn sast_severity_level(s: &str) -> u8 {
    match s.to_lowercase().as_str() {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

fn sast_severity_color(s: &str) -> &'static str {
    match s.to_lowercase().as_str() {
        "critical" => "\x1b[1;31m",  // bold red
        "high"     => "\x1b[31m",    // red
        "medium"   => "\x1b[33m",    // yellow
        "low"      => "\x1b[36m",    // cyan
        _          => "\x1b[0m",
    }
}

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";

/// Print the findings summary table to stdout.
fn print_sast_summary(findings: &[SastFinding], target: &str, elapsed: std::time::Duration) {
    let total = findings.len();
    let secs = elapsed.as_secs();

    println!();
    println!("{BOLD}┌─ Strobes SAST Scan Results ─────────────────────────────────┐{RESET}");
    println!("{BOLD}│{RESET}  Target : {target}");
    println!("{BOLD}│{RESET}  Time   : {secs}s");
    println!("{BOLD}│{RESET}  Total  : {BOLD}{total}{RESET} finding(s)");
    println!("{BOLD}├──────────────────────────────────────────────────────────────┤{RESET}");

    // Count by severity.
    let counts: Vec<(&str, usize)> = ["critical", "high", "medium", "low"]
        .iter()
        .map(|&sev| {
            let n = findings.iter().filter(|f| f.severity.to_lowercase() == sev).count();
            (sev, n)
        })
        .collect();

    for (sev, n) in &counts {
        if *n > 0 {
            let col = sast_severity_color(sev);
            println!("{BOLD}│{RESET}  {col}{}{RESET:>12}  {BOLD}{n}{RESET}", sev.to_uppercase());
        }
    }

    if total == 0 {
        println!("{BOLD}│{RESET}  {GREEN}No security findings — clean scan.{RESET}");
        println!("{BOLD}└──────────────────────────────────────────────────────────────┘{RESET}");
        return;
    }

    println!("{BOLD}├──────────────────────────────────────────────────────────────┤{RESET}");

    // Print each finding.
    let mut sorted = findings.to_vec();
    sorted.sort_by(|a, b| {
        sast_severity_level(&b.severity).cmp(&sast_severity_level(&a.severity))
    });

    for (i, f) in sorted.iter().enumerate() {
        let col = sast_severity_color(&f.severity);
        let sev_upper = f.severity.to_uppercase();
        let loc = match f.line {
            Some(l) => format!("{}:{}", f.file, l),
            None => f.file.clone(),
        };
        println!("{BOLD}│{RESET}");
        println!("{BOLD}│{RESET}  {BOLD}#{}{RESET}  {col}[{sev_upper}]{RESET}  {BOLD}{}{RESET}", i + 1, f.title);
        println!("{BOLD}│{RESET}      {DIM}{loc}{RESET}");
        // Word-wrap description at ~70 chars.
        for line in wrap_text(&f.description, 68) {
            println!("{BOLD}│{RESET}      {line}");
        }
        if !f.fix.is_empty() {
            println!("{BOLD}│{RESET}      {DIM}Fix: {}{RESET}", f.fix);
        }
    }

    println!("{BOLD}└──────────────────────────────────────────────────────────────┘{RESET}");
}

fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let mut lines = vec![];
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.len() + 1 + word.len() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(cur.clone());
            cur = word.to_string();
        }
    }
    if !cur.is_empty() { lines.push(cur); }
    lines
}

/// Convert parsed SAST findings to SARIF 2.1.0 JSON.
fn sast_to_sarif(findings: &[SastFinding], target: &str) -> serde_json::Value {
    let rules: Vec<serde_json::Value> = findings.iter().enumerate().map(|(i, f)| {
        serde_json::json!({
            "id": format!("STROBES-SAST-{:03}", i + 1),
            "shortDescription": { "text": f.title },
            "properties": { "severity": f.severity },
        })
    }).collect();

    let results: Vec<serde_json::Value> = findings.iter().enumerate().map(|(i, f)| {
        let level = match f.severity.to_lowercase().as_str() {
            "critical" | "high" => "error",
            "medium" => "warning",
            "low" => "note",
            _ => "none",
        };
        let mut loc = serde_json::json!({
            "physicalLocation": {
                "artifactLocation": { "uri": f.file, "uriBaseId": "%SRCROOT%" }
            }
        });
        if let Some(line) = f.line {
            loc["physicalLocation"]["region"] = serde_json::json!({ "startLine": line });
        }
        serde_json::json!({
            "ruleId": format!("STROBES-SAST-{:03}", i + 1),
            "level": level,
            "message": { "text": format!("{}\n\n{}", f.title, f.description) },
            "locations": [loc],
        })
    }).collect();

    serde_json::json!({
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "Strobes SAST",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": "https://strobes.co",
                    "rules": rules,
                }
            },
            "results": results,
            "automationDetails": { "id": format!("strobes/sast/{target}") },
        }]
    })
}

#[allow(clippy::too_many_arguments)]
// ── Live scan display ─────────────────────────────────────────────────────────

const SAST_LOGO: &[&str] = &[
    " ███████╗████████╗██████╗  ██████╗ ██████╗ ███████╗███████╗",
    " ██╔════╝╚══██╔══╝██╔══██╗██╔═══██╗██╔══██╗██╔════╝██╔════╝",
    " ███████╗   ██║   ██████╔╝██║   ██║██████╔╝█████╗  ███████╗",
    " ╚════██║   ██║   ██╔══██╗██║   ██║██╔══██╗██╔══╝  ╚════██║",
    " ███████║   ██║   ██║  ██║╚██████╔╝██████╔╝███████╗███████║",
    " ╚══════╝   ╚═╝   ╚═╝  ╚═╝ ╚═════╝ ╚═════╝ ╚══════╝╚══════╝",
];
const SAST_SPINNER: &[char] = &['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];
// Logo(6) + subtitle(1) + blank(1) + stats(1) + blank(1) + bar(1) + blank(1) + msgs(3) = 15
const SAST_DISPLAY_HEIGHT: usize = 15;
const SAST_MSG_ROWS: usize = 3;

struct ScanDisplay {
    target: String,
    file_count: u64,
    byte_count: u64,
    timeout_secs: u64,
    messages: std::collections::VecDeque<String>,
    spinner_idx: usize,
    first_render: bool,
}

impl ScanDisplay {
    fn new(target: &str, file_count: u64, byte_count: u64, timeout_secs: u64) -> Self {
        Self {
            target: target.to_string(),
            file_count,
            byte_count,
            timeout_secs,
            messages: std::collections::VecDeque::new(),
            spinner_idx: 0,
            first_render: true,
        }
    }

    fn push(&mut self, msg: impl Into<String>) {
        self.messages.push_back(msg.into());
        while self.messages.len() > SAST_MSG_ROWS {
            self.messages.pop_front();
        }
    }

    fn render(&mut self, elapsed: std::time::Duration) {
        use std::io::Write as _;
        let mut buf = String::with_capacity(2048);

        // Erase previous block.
        if !self.first_render {
            for _ in 0..SAST_DISPLAY_HEIGHT {
                buf.push_str("\x1b[A\x1b[2K");
            }
        }
        self.first_render = false;

        // Logo — green.
        for (i, line) in SAST_LOGO.iter().enumerate() {
            if i == SAST_LOGO.len() - 1 {
                buf.push_str(&format!("\x1b[1;32m{line}\x1b[0m  \x1b[2mAI Security Scanner\x1b[0m\n"));
            } else {
                buf.push_str(&format!("\x1b[1;32m{line}\x1b[0m\n"));
            }
        }

        // Blank.
        buf.push('\n');

        // Stats row.
        let s = elapsed.as_secs();
        buf.push_str(&format!(
            "  \x1b[2mtarget\x1b[0m  \x1b[1m{:<18}\x1b[0m  \
             \x1b[2melapsed\x1b[0m  \x1b[1m{:02}:{:02}\x1b[0m  \
             \x1b[2mfiles\x1b[0m  {} \x1b[2m({:.1} KB)\x1b[0m\n",
            self.target,
            s / 60, s % 60,
            self.file_count, self.byte_count as f64 / 1024.0,
        ));

        // Blank.
        buf.push('\n');

        // Progress bar (elapsed / timeout, indeterminate feel with bouncing fill).
        let pct = (s as f64 / self.timeout_secs as f64).min(1.0);
        let bar_w = 38usize;
        let filled = (pct * bar_w as f64) as usize;
        let empty = bar_w - filled;
        let spin = SAST_SPINNER[self.spinner_idx % SAST_SPINNER.len()];
        self.spinner_idx += 1;
        buf.push_str(&format!(
            "  \x1b[32m{}\x1b[2m{}\x1b[0m  {} \x1b[2m{:>3.0}%  scanning…\x1b[0m\n",
            "▓".repeat(filled),
            "░".repeat(empty),
            spin,
            pct * 100.0,
        ));

        // Blank.
        buf.push('\n');

        // Last N message lines — always print exactly SAST_MSG_ROWS lines.
        let msgs: Vec<&str> = self.messages.iter().map(|s| s.as_str()).collect();
        for i in 0..SAST_MSG_ROWS {
            if let Some(m) = msgs.get(i) {
                // Trim to ~76 chars so lines never wrap.
                let display: String = m.chars().take(76).collect();
                let pad = if m.chars().count() > 76 { "…" } else { "" };
                buf.push_str(&format!("  \x1b[2m{display}{pad}\x1b[0m\n"));
            } else {
                buf.push('\n');
            }
        }

        let _ = std::io::stderr().write_all(buf.as_bytes());
        let _ = std::io::stderr().flush();
    }

    /// Erase the display block from the terminal.
    fn erase(&self) {
        use std::io::Write as _;
        let mut buf = String::new();
        for _ in 0..SAST_DISPLAY_HEIGHT {
            buf.push_str("\x1b[A\x1b[2K");
        }
        let _ = std::io::stderr().write_all(buf.as_bytes());
        let _ = std::io::stderr().flush();
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn cmd_scan_sast(
    p: &config::Profile,
    dir: String,
    output_fmt: String,
    output_file: Option<String>,
    custom_prompt: Option<String>,
    workspace: Option<String>,
    model: Option<i64>,
    timeout: u64,
    fail_on: Option<String>,
    exclude: Vec<String>,
    max_mb: u64,
) -> Result<()> {
    require_complete(p)?;

    // 1. Resolve source directory.
    let src = std::path::Path::new(&dir)
        .canonicalize()
        .map_err(|e| anyhow!("cannot access '{}': {}", dir, e))?;
    if !src.is_dir() {
        return Err(anyhow!("'{}' is not a directory", dir));
    }
    let target_name = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("scan")
        .to_string();

    // 2. Create a deterministic sandbox ID so re-runs reuse the same slot.
    let sandbox_id = format!(
        "sast-{}",
        target_name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
            .collect::<String>()
    );
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let sandbox_path = home.join(".strobes-ai").join("sandboxes").join(&sandbox_id);

    // Wipe + recreate so we always scan fresh files.
    if sandbox_path.exists() {
        std::fs::remove_dir_all(&sandbox_path)?;
    }
    std::fs::create_dir_all(&sandbox_path)?;

    // 3. Copy files into sandbox.
    let max_bytes = max_mb * 1_000_000;
    eprintln!("copying {}  →  sandbox…", src.display());
    let (file_count, byte_count) = copy_dir_to_sandbox(&src, &sandbox_path, &exclude, max_bytes)
        .map_err(|e| anyhow!("{e}"))?;

    // 4. Point the sandbox env var at our directory.
    #[allow(deprecated)]
    std::env::set_var("STROBES_AI_SANDBOX", &sandbox_path);

    // 5. Build the prompt.
    let prompt = custom_prompt.unwrap_or_else(|| build_sast_prompt(&src));

    // 6. Create workspace + thread, then run the scan.
    let client = api::ApiClient::new(p.clone())?;
    let workspace_id: Option<String> = match workspace {
        Some(ws) => Some(ws),
        None => {
            let (id, _) = client.create_workspace(&format!("SAST: {target_name}")).await?;
            eprintln!("workspace: {id}");
            Some(id)
        }
    };

    let thread_title = format!("SAST scan: {target_name}");
    let thread_id = client
        .create_thread(&thread_title, workspace_id.as_deref(), None)
        .await?;
    eprintln!("thread: {thread_id}");

    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let handle = pulse::connect(p, &thread_id, tx, model).await?;
    handle.send_user_message(&prompt);

    // 7. Live display.
    let mut display = ScanDisplay::new(&target_name, file_count, byte_count, timeout);
    let start = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut render_tick = tokio::time::interval(std::time::Duration::from_millis(120));
    let mut full_text = String::new();

    // Initial render so the block is present before the first tick.
    display.render(start.elapsed());

    let run_result: Result<()> = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                display.erase();
                eprintln!("error: scan timed out after {timeout}s — use --timeout to extend");
                break Err(anyhow!("timed out after {timeout}s"));
            }
            _ = render_tick.tick() => {
                display.render(start.elapsed());
            }
            ev_opt = rx.recv() => {
                match ev_opt {
                    None => {
                        display.erase();
                        break Ok(());
                    }
                    Some(ev) => match ev {
                        pulse::AppEvent::RunFinished(_) => {
                            display.erase();
                            break Ok(());
                        }
                        pulse::AppEvent::Stream(item) => match item.kind.as_str() {
                            "token" => {
                                if let Some(text) = &item.text {
                                    full_text.push_str(text);
                                }
                            }
                            "tool_start" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let detail = item.detail.as_deref().unwrap_or("");
                                let msg = if detail.is_empty() {
                                    format!("▶ {name}")
                                } else {
                                    // Trim detail so line stays short.
                                    let d: String = detail.chars().take(48).collect();
                                    let ellipsis = if detail.len() > 48 { "…" } else { "" };
                                    format!("▶ {name}({d}{ellipsis})")
                                };
                                display.push(msg);
                                display.render(start.elapsed());
                            }
                            "tool_output" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let detail = item.detail.as_deref().unwrap_or("");
                                if !detail.is_empty() {
                                    let d: String = detail.chars().take(52).collect();
                                    let ellipsis = if detail.len() > 52 { "…" } else { "" };
                                    display.push(format!("◀ {name}: {d}{ellipsis}"));
                                    display.render(start.elapsed());
                                }
                            }
                            "tool_failed" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let err = item.detail.as_deref().unwrap_or("error");
                                display.push(format!("✗ {name}: {err}"));
                                display.render(start.elapsed());
                            }
                            _ => {
                                if let Some(text) = &item.text {
                                    full_text.push_str(text);
                                }
                            }
                        },
                        pulse::AppEvent::Error(e) => {
                            display.erase();
                            eprintln!("error: {e}");
                            break Err(anyhow!("agent error: {e}"));
                        }
                        pulse::AppEvent::Interrupt { .. } => {
                            display.erase();
                            eprintln!("error: agent requested input — run interactively for complex scans");
                            break Err(anyhow!("agent requested human input during non-interactive scan"));
                        }
                        _ => {}
                    },
                }
            }
        }
    };

    run_result?;
    let elapsed = start.elapsed();

    // 7. Parse findings from agent output.
    let mut findings = parse_sast_output(&full_text);
    findings.sort_by(|a, b| {
        sast_severity_level(&b.severity).cmp(&sast_severity_level(&a.severity))
    });

    // 8. Print summary to stderr / stdout.
    print_sast_summary(&findings, &target_name, elapsed);

    // 9. Produce structured output.
    let output_content = match output_fmt.as_str() {
        "json" => {
            serde_json::to_string_pretty(&serde_json::json!({
                "scan_target": target_name,
                "workspace_id": workspace_id,
                "thread_id": thread_id,
                "elapsed_secs": elapsed.as_secs(),
                "findings": findings,
            }))?
        }
        "sarif" => {
            serde_json::to_string_pretty(&sast_to_sarif(&findings, &target_name))?
        }
        _ => String::new(), // text summary already printed above
    };

    if !output_content.is_empty() {
        if let Some(ref path) = output_file {
            std::fs::write(path, &output_content)?;
            eprintln!("results saved → {path}");
        } else {
            println!("{output_content}");
        }
    } else if let Some(ref path) = output_file {
        // text format + output file: save the raw agent text.
        std::fs::write(path, &full_text)?;
        eprintln!("raw output saved → {path}");
    }

    // 10. Gate on severity threshold.
    if let Some(threshold) = &fail_on {
        let level = sast_severity_level(threshold);
        let blocking: Vec<_> = findings
            .iter()
            .filter(|f| sast_severity_level(&f.severity) >= level)
            .collect();
        if !blocking.is_empty() {
            return Err(anyhow!(
                "{} finding(s) at or above '{}' severity",
                blocking.len(),
                threshold
            ));
        }
    }

    Ok(())
}

// ── strobes ci sca ──────────────────────────────────────────────────────────

/// A resolved dependency with ecosystem, version, and dependency chain.
#[derive(Debug, Clone, serde::Serialize)]
struct Dep {
    name: String,
    version: String,
    ecosystem: String,
    is_direct: bool,
    /// chain of names that pull this in, innermost first
    via: Vec<String>,
    manifest: String,
}

/// A vulnerability returned by OSV.dev for a single package.
#[derive(Debug, Clone, serde::Serialize)]
struct OsvVuln {
    id: String,
    aliases: Vec<String>,
    summary: String,
    severity: String,        // critical / high / medium / low / unknown
    cvss_score: Option<f64>,
    fix_versions: Vec<String>,
    details: String,
    /// the package this vuln was found on
    pkg_name: String,
    pkg_version: String,
    pkg_ecosystem: String,
    is_direct: bool,
    via: Vec<String>,
    manifest: String,
}

/// Final finding after AI reachability analysis.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ScaFinding {
    severity: String,
    title: String,
    cve_id: String,
    package: String,
    version: String,
    is_direct: bool,
    via: Vec<String>,
    fix_version: String,
    reachable: bool,
    reachability_confidence: String,  // confirmed / likely / unlikely / unknown
    description: String,
    ai_reasoning: String,
}

// ── Language parsers ───────────────────────────────────────────────────────

fn parse_all_deps(dir: &std::path::Path) -> Vec<Dep> {
    let mut deps = vec![];
    deps.extend(parse_python_deps(dir));
    deps.extend(parse_nodejs_deps(dir));
    deps.extend(parse_go_deps(dir));
    deps.extend(parse_rust_deps(dir));
    deps.extend(parse_ruby_deps(dir));
    deps.extend(parse_php_deps(dir));
    deps.extend(parse_java_deps(dir));
    deps.extend(parse_dotnet_deps(dir));
    // Deduplicate by (ecosystem, name, version) — keep first occurrence.
    let mut seen = std::collections::HashSet::new();
    deps.retain(|d| seen.insert((d.ecosystem.clone(), d.name.to_lowercase(), d.version.clone())));
    deps
}

/// Parse `name==version` style lines; handles `>=`, `~=`, `!=`, extras.
fn parse_python_deps(dir: &std::path::Path) -> Vec<Dep> {
    let mut deps = vec![];

    // requirements*.txt
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with("requirements") && name.ends_with(".txt") {
            let content = std::fs::read_to_string(&p).unwrap_or_default();
            for line in content.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if line.is_empty() || line.starts_with('-') { continue; }
                // Strip extras: requests[security]==2.28.0  ->  requests==2.28.0
                let line = line.split('[').next().unwrap_or(line);
                if let Some((pkg, ver)) = split_pep440(line) {
                    deps.push(Dep {
                        name: pkg,
                        version: ver,
                        ecosystem: "PyPI".into(),
                        is_direct: true,
                        via: vec![],
                        manifest: name.to_string(),
                    });
                }
            }
        }
    }

    // poetry.lock
    let poetry = dir.join("poetry.lock");
    if let Ok(content) = std::fs::read_to_string(&poetry) {
        let mut in_pkg = false;
        let mut cur_name = String::new();
        let mut cur_ver = String::new();
        for line in content.lines() {
            if line == "[[package]]" {
                if !cur_name.is_empty() && !cur_ver.is_empty() {
                    deps.push(Dep { name: cur_name.clone(), version: cur_ver.clone(),
                        ecosystem: "PyPI".into(), is_direct: false, via: vec![], manifest: "poetry.lock".into() });
                }
                in_pkg = true; cur_name.clear(); cur_ver.clear();
            } else if in_pkg {
                if let Some(v) = toml_str_field(line, "name") { cur_name = v; }
                if let Some(v) = toml_str_field(line, "version") { cur_ver = v; }
            }
        }
        if !cur_name.is_empty() && !cur_ver.is_empty() {
            deps.push(Dep { name: cur_name, version: cur_ver, ecosystem: "PyPI".into(),
                is_direct: false, via: vec![], manifest: "poetry.lock".into() });
        }
    }

    // Pipfile.lock — JSON {"default": {"requests": {"version": "==2.28.0"}}}
    let pipfile = dir.join("Pipfile.lock");
    if let Ok(content) = std::fs::read_to_string(&pipfile) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
            for section in &["default", "develop"] {
                if let Some(pkgs) = v.get(section).and_then(|s| s.as_object()) {
                    for (pkg, meta) in pkgs {
                        if let Some(ver) = meta.get("version").and_then(|v| v.as_str()) {
                            let ver = ver.trim_start_matches("==").to_string();
                            deps.push(Dep { name: pkg.clone(), version: ver,
                                ecosystem: "PyPI".into(), is_direct: true, via: vec![],
                                manifest: "Pipfile.lock".into() });
                        }
                    }
                }
            }
        }
    }

    deps
}

fn split_pep440(spec: &str) -> Option<(String, String)> {
    // e.g. "requests==2.28.0", "django>=4.0", "flask~=3.0.0"
    for op in &["===", "~=", "==", ">=", "<=", "!=", ">", "<"] {
        if let Some(idx) = spec.find(op) {
            let name = spec[..idx].trim().to_string();
            let ver_part = spec[idx + op.len()..].trim();
            // Take only the first constraint if comma-separated.
            let ver = ver_part.split(',').next().unwrap_or(ver_part).trim().to_string();
            if !name.is_empty() && !ver.is_empty() {
                return Some((name, ver));
            }
        }
    }
    None
}

fn parse_nodejs_deps(dir: &std::path::Path) -> Vec<Dep> {
    let mut deps = vec![];

    // package-lock.json v2/v3 — "packages" map is the full tree
    let lockfile = dir.join("package-lock.json");
    if let Ok(content) = std::fs::read_to_string(&lockfile) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
            // Direct deps listed in package.json
            let direct: std::collections::HashSet<String> = v
                .get("packages").and_then(|p| p.get(""))
                .and_then(|root| root.get("dependencies")).and_then(|d| d.as_object())
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();

            if let Some(pkgs) = v.get("packages").and_then(|p| p.as_object()) {
                for (path, meta) in pkgs {
                    if path.is_empty() { continue; }
                    let name = path.trim_start_matches("node_modules/").to_string();
                    // Handle scoped: node_modules/@scope/pkg -> @scope/pkg
                    if let Some(ver) = meta.get("version").and_then(|v| v.as_str()) {
                        let is_direct = direct.contains(&name);
                        deps.push(Dep { name, version: ver.to_string(),
                            ecosystem: "npm".into(), is_direct, via: vec![],
                            manifest: "package-lock.json".into() });
                    }
                }
            }
        }
    }

    // yarn.lock fallback — "name@version:\n  version \"x.y.z\""
    if deps.is_empty() {
        let yarn = dir.join("yarn.lock");
        if let Ok(content) = std::fs::read_to_string(&yarn) {
            let mut cur_spec = String::new();
            for line in content.lines() {
                if line.is_empty() || line.starts_with('#') { cur_spec.clear(); continue; }
                if !line.starts_with(' ') && line.ends_with(':') {
                    cur_spec = line.trim_end_matches(':').split('@').next().unwrap_or("").trim_matches('"').to_string();
                } else if line.trim().starts_with("version") && !cur_spec.is_empty() {
                    let ver = line.trim().trim_start_matches("version").trim().trim_matches('"').to_string();
                    if !ver.is_empty() {
                        deps.push(Dep { name: cur_spec.clone(), version: ver,
                            ecosystem: "npm".into(), is_direct: false, via: vec![],
                            manifest: "yarn.lock".into() });
                        cur_spec.clear();
                    }
                }
            }
        }
    }

    // package.json fallback for unpinned deps (name only, no version — skip OSV query)
    // Already covered by package-lock.json above; skip to avoid duplicates.
    deps
}

fn parse_go_deps(dir: &std::path::Path) -> Vec<Dep> {
    let mut deps = vec![];
    // go.mod — direct deps
    let gomod = dir.join("go.mod");
    if let Ok(content) = std::fs::read_to_string(&gomod) {
        let mut in_require = false;
        for line in content.lines() {
            let t = line.trim();
            if t == "require (" { in_require = true; continue; }
            if t == ")" { in_require = false; continue; }
            let parts: Vec<&str> = if in_require {
                t.split_whitespace().collect()
            } else if t.starts_with("require ") {
                t.split_whitespace().skip(1).collect()
            } else {
                continue;
            };
            if parts.len() >= 2 && !parts[0].starts_with("//") {
                let ver = parts[1].trim_start_matches('v').to_string();
                deps.push(Dep { name: parts[0].to_string(), version: ver,
                    ecosystem: "Go".into(), is_direct: true, via: vec![],
                    manifest: "go.mod".into() });
            }
        }
    }
    // go.sum — transitive (module path + version, one per line, space-separated)
    let gosum = dir.join("go.sum");
    if let Ok(content) = std::fs::read_to_string(&gosum) {
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let modpath = parts[0];
                let ver_raw = parts[1];
                // skip go.mod hash entries (end with /go.mod)
                if ver_raw.contains("/go.mod") { continue; }
                let ver = ver_raw.split('/').next().unwrap_or(ver_raw)
                    .trim_start_matches('v').to_string();
                let already_direct = deps.iter().any(|d| d.name == modpath);
                deps.push(Dep { name: modpath.to_string(), version: ver,
                    ecosystem: "Go".into(), is_direct: already_direct, via: vec![],
                    manifest: "go.sum".into() });
            }
        }
    }
    deps
}

fn parse_rust_deps(dir: &std::path::Path) -> Vec<Dep> {
    let mut deps = vec![];
    let lock = dir.join("Cargo.lock");
    if let Ok(content) = std::fs::read_to_string(&lock) {
        // Read direct deps from Cargo.toml for is_direct flag.
        let direct: std::collections::HashSet<String> = {
            let toml = dir.join("Cargo.toml");
            std::fs::read_to_string(&toml).unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    let t = l.trim();
                    if t.starts_with('[') { return None; }
                    t.split('=').next().map(|n| n.trim().trim_matches('"').to_string())
                })
                .collect()
        };
        let mut cur_name = String::new();
        let mut cur_ver = String::new();
        for line in content.lines() {
            if line == "[[package]]" {
                if !cur_name.is_empty() && !cur_ver.is_empty() {
                    let is_direct = direct.contains(&cur_name);
                    deps.push(Dep { name: cur_name.clone(), version: cur_ver.clone(),
                        ecosystem: "crates.io".into(), is_direct, via: vec![],
                        manifest: "Cargo.lock".into() });
                }
                cur_name.clear(); cur_ver.clear();
            } else {
                if let Some(v) = toml_str_field(line, "name") { cur_name = v; }
                if let Some(v) = toml_str_field(line, "version") { cur_ver = v; }
            }
        }
        if !cur_name.is_empty() && !cur_ver.is_empty() {
            deps.push(Dep { name: cur_name, version: cur_ver, ecosystem: "crates.io".into(),
                is_direct: false, via: vec![], manifest: "Cargo.lock".into() });
        }
    }
    deps
}

fn parse_ruby_deps(dir: &std::path::Path) -> Vec<Dep> {
    let mut deps = vec![];
    let lock = dir.join("Gemfile.lock");
    if let Ok(content) = std::fs::read_to_string(&lock) {
        let mut in_gems = false;
        for line in content.lines() {
            let t = line.trim();
            if t == "GEM" || t == "GIT" || t == "PATH" { in_gems = false; }
            if t == "specs:" { in_gems = true; continue; }
            if in_gems && !t.is_empty() {
                // "    rails (7.1.2)"
                if let Some(idx) = t.find(" (") {
                    let name = t[..idx].trim().to_string();
                    let ver = t[idx+2..].trim_end_matches(')').to_string();
                    deps.push(Dep { name, version: ver, ecosystem: "RubyGems".into(),
                        is_direct: false, via: vec![], manifest: "Gemfile.lock".into() });
                }
            }
        }
    }
    deps
}

fn parse_php_deps(dir: &std::path::Path) -> Vec<Dep> {
    let mut deps = vec![];
    let lock = dir.join("composer.lock");
    if let Ok(content) = std::fs::read_to_string(&lock) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
            for section in &["packages", "packages-dev"] {
                if let Some(pkgs) = v.get(section).and_then(|p| p.as_array()) {
                    for pkg in pkgs {
                        let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        let ver = pkg.get("version").and_then(|v| v.as_str())
                            .unwrap_or("").trim_start_matches('v').to_string();
                        if !name.is_empty() && !ver.is_empty() {
                            deps.push(Dep { name, version: ver, ecosystem: "Packagist".into(),
                                is_direct: true, via: vec![], manifest: "composer.lock".into() });
                        }
                    }
                }
            }
        }
    }
    deps
}

fn parse_java_deps(dir: &std::path::Path) -> Vec<Dep> {
    let mut deps = vec![];
    let pom = dir.join("pom.xml");
    if let Ok(content) = std::fs::read_to_string(&pom) {
        // Rough XML parse: find <dependency> blocks and extract groupId/artifactId/version.
        for block in content.split("<dependency>").skip(1) {
            let end = block.find("</dependency>").unwrap_or(block.len());
            let block = &block[..end];
            let group = xml_tag(block, "groupId").unwrap_or_default();
            let artifact = xml_tag(block, "artifactId").unwrap_or_default();
            let ver = xml_tag(block, "version").unwrap_or_default();
            if !group.is_empty() && !artifact.is_empty() && !ver.is_empty() {
                // Skip version properties like ${spring.version} — can't resolve.
                if ver.starts_with('$') { continue; }
                let name = format!("{}:{}", group, artifact);
                deps.push(Dep { name, version: ver, ecosystem: "Maven".into(),
                    is_direct: true, via: vec![], manifest: "pom.xml".into() });
            }
        }
    }
    // build.gradle — best-effort regex for implementation/api 'group:artifact:version'
    let gradle = dir.join("build.gradle");
    if let Ok(content) = std::fs::read_to_string(&gradle) {
        for line in content.lines() {
            let t = line.trim();
            for kw in &["implementation", "api", "compile", "testImplementation"] {
                if t.starts_with(kw) {
                    // Extract quoted string like 'com.example:lib:1.2.3'
                    for quote in &['\'', '"'] {
                        if let Some(start) = t.find(*quote) {
                            if let Some(end) = t[start+1..].find(*quote) {
                                let dep_str = &t[start+1..start+1+end];
                                let parts: Vec<&str> = dep_str.split(':').collect();
                                if parts.len() == 3 {
                                    let name = format!("{}:{}", parts[0], parts[1]);
                                    deps.push(Dep { name, version: parts[2].to_string(),
                                        ecosystem: "Maven".into(), is_direct: true, via: vec![],
                                        manifest: "build.gradle".into() });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    deps
}

fn parse_dotnet_deps(dir: &std::path::Path) -> Vec<Dep> {
    let mut deps = vec![];
    // packages.lock.json (NuGet lock file)
    let lock = dir.join("packages.lock.json");
    if let Ok(content) = std::fs::read_to_string(&lock) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(frameworks) = v.get("dependencies").and_then(|d| d.as_object()) {
                for (_framework, pkgs) in frameworks {
                    if let Some(pkgs) = pkgs.as_object() {
                        for (name, meta) in pkgs {
                            let ver = meta.get("resolved").and_then(|v| v.as_str())
                                .unwrap_or("").to_string();
                            if !ver.is_empty() {
                                let is_direct = meta.get("type").and_then(|t| t.as_str()) == Some("Direct");
                                deps.push(Dep { name: name.clone(), version: ver,
                                    ecosystem: "NuGet".into(), is_direct, via: vec![],
                                    manifest: "packages.lock.json".into() });
                            }
                        }
                    }
                }
            }
        }
    }
    // .csproj PackageReference fallback
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("csproj") {
            let content = std::fs::read_to_string(&p).unwrap_or_default();
            let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            for block in content.split("<PackageReference").skip(1) {
                let name = xml_attr(block, "Include").unwrap_or_default();
                let ver = xml_attr(block, "Version").unwrap_or_default();
                if !name.is_empty() && !ver.is_empty() {
                    deps.push(Dep { name, version: ver, ecosystem: "NuGet".into(),
                        is_direct: true, via: vec![], manifest: fname.clone() });
                }
            }
        }
    }
    deps
}

// ── Mini XML/TOML helpers ──────────────────────────────────────────────────

fn xml_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)?;
    Some(s[start..start + end].trim().to_string())
}

fn xml_attr(s: &str, attr: &str) -> Option<String> {
    let needle = format!("{}=\"", attr);
    let start = s.find(&needle)? + needle.len();
    let end = s[start..].find('"')?;
    Some(s[start..start + end].to_string())
}

fn toml_str_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("{} = ", key);
    if line.trim().starts_with(&needle) || line.trim() == key {
        let after = line.trim().trim_start_matches(&needle);
        Some(after.trim().trim_matches('"').to_string())
    } else {
        None
    }
}

// ── OSV.dev batch query ────────────────────────────────────────────────────

async fn query_osv(deps: &[Dep]) -> anyhow::Result<Vec<OsvVuln>> {
    if deps.is_empty() { return Ok(vec![]); }
    let client = std::sync::Arc::new(
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?
    );

    // Pass 1 — querybatch: returns only (id, modified) per vuln.
    // Collect (dep_index, vuln_id) pairs across all chunks.
    let mut dep_vuln_ids: Vec<(usize, String)> = vec![];   // (dep index, osv id)
    for chunk in deps.chunks(1000) {
        let queries: Vec<serde_json::Value> = chunk.iter().map(|d| {
            serde_json::json!({
                "package": { "name": d.name, "ecosystem": d.ecosystem },
                "version": d.version,
            })
        }).collect();
        let chunk_start = deps.iter().position(|d| {
            d.name == chunk[0].name && d.ecosystem == chunk[0].ecosystem
        }).unwrap_or(0);

        let body = serde_json::json!({ "queries": queries });
        let resp = client
            .post("https://api.osv.dev/v1/querybatch")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let code = resp.status();
            eprintln!("OSV.dev returned {code} — skipping CVE query");
            continue;
        }

        let data: serde_json::Value = resp.json().await?;
        let results = data.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        for (i, result) in results.iter().enumerate() {
            // OSV querybatch returns abbreviated entries (id + modified only).
            // Collect the IDs; full details fetched in pass 2.
            let vuln_arr = result.get("vulns").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            for vuln in vuln_arr {
                if let Some(id) = vuln.get("id").and_then(|v| v.as_str()) {
                    dep_vuln_ids.push((chunk_start + i, id.to_string()));
                }
            }
        }
    }

    if dep_vuln_ids.is_empty() { return Ok(vec![]); }

    // Pass 2 — fetch full vuln details for each unique ID concurrently.
    let unique_ids: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        dep_vuln_ids.iter().filter_map(|(_, id)| {
            if seen.insert(id.clone()) { Some(id.clone()) } else { None }
        }).collect()
    };

    // Fetch in batches of 20 concurrent requests to avoid hammering OSV.
    let mut vuln_details: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
    for id_batch in unique_ids.chunks(20) {
        let futures: Vec<_> = id_batch.iter().map(|id| {
            let client = client.clone();
            let id = id.clone();
            async move {
                let url = format!("https://api.osv.dev/v1/vulns/{}", id);
                let resp = client.get(&url).send().await.ok()?;
                if resp.status().is_success() {
                    let v: serde_json::Value = resp.json().await.ok()?;
                    Some((id, v))
                } else {
                    None
                }
            }
        }).collect();
        let results = futures_util::future::join_all(futures).await;
        for item in results.into_iter().flatten() {
            vuln_details.insert(item.0, item.1);
        }
    }

    // Pass 3 — assemble OsvVuln entries, mapping dep back by index.
    let mut all_vulns = vec![];
    for (dep_idx, osv_id) in &dep_vuln_ids {
        let dep = &deps[*dep_idx];
        let Some(vuln) = vuln_details.get(osv_id) else { continue; };

        let id = vuln.get("id").and_then(|v| v.as_str()).unwrap_or(osv_id).to_string();
        // "aliases" for most ecosystems; Debian uses "upstream" for CVE IDs.
        let mut aliases: Vec<String> = vuln.get("aliases")
            .and_then(|a| a.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if let Some(upstream) = vuln.get("upstream").and_then(|u| u.as_array()) {
            for v in upstream {
                if let Some(s) = v.as_str() {
                    if !aliases.contains(&s.to_string()) { aliases.push(s.to_string()); }
                }
            }
        }
        let details = vuln.get("details").and_then(|v| v.as_str()).unwrap_or("").to_string();
        // Debian OSV entries have no "summary" — use first sentence of details.
        let summary = vuln.get("summary").and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                details.split(". ").next().unwrap_or(&details).chars().take(120).collect()
            });
        let (severity, cvss_score) = extract_osv_severity(vuln);
        let fix_versions = extract_fix_versions(vuln);

        all_vulns.push(OsvVuln {
            id, aliases, summary, severity, cvss_score,
            fix_versions, details,
            pkg_name: dep.name.clone(),
            pkg_version: dep.version.clone(),
            pkg_ecosystem: dep.ecosystem.clone(),
            is_direct: dep.is_direct,
            via: dep.via.clone(),
            manifest: dep.manifest.clone(),
        });
    }
    Ok(all_vulns)
}

fn extract_osv_severity(vuln: &serde_json::Value) -> (String, Option<f64>) {
    // 1. Try severity[].type == CVSS_V3 — score may be a number or a vector string.
    if let Some(sevs) = vuln.get("severity").and_then(|s| s.as_array()) {
        for sev in sevs {
            let t = sev.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if t == "CVSS_V3" || t == "CVSS_V4" {
                if let Some(score_str) = sev.get("score").and_then(|s| s.as_str()) {
                    // Numeric score (e.g. "7.5")
                    if let Ok(n) = score_str.parse::<f64>() {
                        return (cvss_label(n), Some(n));
                    }
                    // CVSS vector string (e.g. "CVSS:3.1/AV:N/AC:L/...")
                    if score_str.starts_with("CVSS:") {
                        let sev = cvss_vector_severity(score_str);
                        return (sev, None);
                    }
                }
            }
        }
    }
    // 2. database_specific.severity (GitHub, OSS-Fuzz, etc.)
    if let Some(sev) = vuln.get("database_specific")
        .and_then(|d| d.get("severity"))
        .and_then(|s| s.as_str())
    {
        return (normalise_severity(sev), None);
    }
    // 3. ecosystem_specific.urgency (Debian/Ubuntu)
    if let Some(affected) = vuln.get("affected").and_then(|a| a.as_array()) {
        for aff in affected {
            if let Some(urgency) = aff.get("ecosystem_specific")
                .and_then(|e| e.get("urgency"))
                .and_then(|u| u.as_str())
            {
                match urgency.to_lowercase().as_str() {
                    "unimportant" | "not yet assigned" => continue,
                    "low" | "minor" => return ("low".to_string(), None),
                    "medium" | "moderate" => return ("medium".to_string(), None),
                    "high" | "important" | "critical" | "grave" => return ("high".to_string(), None),
                    _ => continue,
                };
            }
        }
    }
    ("medium".to_string(), None)  // safe default — better to over-report than miss
}

fn normalise_severity(s: &str) -> String {
    match s.to_lowercase().as_str() {
        "critical" => "critical".to_string(),
        "high" | "important" => "high".to_string(),
        "moderate" | "medium" => "medium".to_string(),
        "low" | "minor" => "low".to_string(),
        _ => "medium".to_string(),
    }
}

/// Parse a CVSS v3/v4 vector string to a rough severity label.
/// E.g. "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" → "critical"
fn cvss_vector_severity(vector: &str) -> String {
    let av_network = vector.contains("/AV:N");
    let scope_changed = vector.contains("/S:C");
    let c_high = vector.contains("/C:H");
    let i_high = vector.contains("/I:H");
    let a_high = vector.contains("/A:H");
    let c_low  = vector.contains("/C:L");
    let i_low  = vector.contains("/I:L");

    if (c_high && i_high) || (scope_changed && (c_high || i_high)) {
        if av_network { return "critical".to_string(); }
        return "high".to_string();
    }
    if c_high || i_high || a_high {
        return "high".to_string();
    }
    if c_low || i_low || vector.contains("/A:L") {
        return "medium".to_string();
    }
    "low".to_string()
}

fn cvss_label(score: f64) -> String {
    match score as u32 {
        0 => "low".to_string(),
        1..=3 => "low".to_string(),
        4..=6 => "medium".to_string(),
        7..=8 => "high".to_string(),
        _ => "critical".to_string(),
    }
}

fn extract_fix_versions(vuln: &serde_json::Value) -> Vec<String> {
    let mut fixes = vec![];
    if let Some(affected) = vuln.get("affected").and_then(|a| a.as_array()) {
        for aff in affected {
            if let Some(ranges) = aff.get("ranges").and_then(|r| r.as_array()) {
                for range in ranges {
                    if let Some(events) = range.get("events").and_then(|e| e.as_array()) {
                        for ev in events {
                            if let Some(fixed) = ev.get("fixed").and_then(|f| f.as_str()) {
                                fixes.push(fixed.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    fixes.dedup();
    fixes
}

// ── AI reachability prompt ─────────────────────────────────────────────────

fn build_sca_prompt(target: &str, vulns: &[OsvVuln]) -> String {
    let vuln_list = vulns.iter().enumerate().map(|(i, v)| {
        let cve = v.aliases.iter().find(|a| a.starts_with("CVE-")).cloned()
            .unwrap_or_else(|| v.id.clone());
        let fix = if v.fix_versions.is_empty() { "no fix yet".to_string() }
                  else { v.fix_versions.join(", ") };
        let via_str = if v.via.is_empty() { "direct dependency".to_string() }
                      else { format!("transitive via: {}", v.via.join(" -> ")) };
        format!(
            "  [{idx}] {cve} | {sev} | {pkg}@{ver} | {via}\n       Summary: {summary}\n       Fix: {fix}",
            idx = i + 1, cve = cve, sev = v.severity.to_uppercase(),
            pkg = v.pkg_name, ver = v.pkg_version, via = via_str,
            summary = v.summary, fix = fix
        )
    }).collect::<Vec<_>>().join("\n\n");

    format!(
        r#"You are a security engineer performing a Software Composition Analysis (SCA) reachability audit.

Target codebase: {target}
The source code is available in your local sandbox — use your tools to read and analyse it.

## CVEs Found in Dependencies

{vuln_list}

## Your Task

For EACH CVE above, determine:

1. **Reachable** — Does the application actually call the vulnerable component?
   - Read the package's import/usage in the codebase
   - Trace the call path from application code → vulnerable function
   - Consider all import styles (direct import, transitive re-export, dynamic require)

2. **Exploitable in context** — Even if reachable, is the vulnerable code path triggered by attacker-controlled input?
   - Does untrusted data (HTTP request params, user input, external files) flow into the vulnerable function?
   - Are there compensating controls (input validation, WAF, auth gates) that block exploitation?

3. **Verdict** — For each CVE:
   - `confirmed_reachable` — called with untrusted input, no effective mitigation
   - `likely_reachable` — called but can't fully trace input source
   - `not_reachable` — imported but the vulnerable function/path is never called
   - `unknown` — can't determine from source alone (e.g. runtime-only, config-driven)

## Output Format

After your analysis, output a JSON block EXACTLY like this:

```json
{{
  "scan_target": "{target}",
  "findings": [
    {{
      "severity": "critical",
      "title": "SQL Injection via requests CVE-2024-XXXX — confirmed reachable",
      "cve_id": "CVE-2024-XXXX",
      "package": "requests",
      "version": "2.28.0",
      "is_direct": true,
      "via": [],
      "fix_version": "2.31.0",
      "reachable": true,
      "reachability_confidence": "confirmed_reachable",
      "description": "The vulnerable session handling in requests is called from api/client.py line 42 with user-supplied URLs. No validation is applied before the call.",
      "ai_reasoning": "Found import of requests in api/client.py. fetch_user_data() on line 42 passes request.args.get('url') directly to requests.get(). This is attacker-controlled input reaching the vulnerable function."
    }}
  ]
}}
```

Only include CVEs that are `confirmed_reachable` or `likely_reachable` in the JSON.
Briefly note `not_reachable` ones in your analysis text but exclude them from the findings JSON.
Begin."#
    )
}

// ── Output formatters ──────────────────────────────────────────────────────

fn print_sca_summary(vulns: &[OsvVuln], findings: &[ScaFinding], target: &str, elapsed: std::time::Duration, skip_ai: bool) {
    let total_vulns = vulns.len();
    let reachable = findings.iter().filter(|f| f.reachable).count();
    println!();
    println!("{BOLD}┌─ Strobes SCA Results ────────────────────────────────────────┐{RESET}");
    println!("{BOLD}│{RESET}  Target  : {target}");
    println!("{BOLD}│{RESET}  Time    : {}s", elapsed.as_secs());
    println!("{BOLD}│{RESET}  CVEs    : {BOLD}{total_vulns}{RESET} found in dependencies");
    if !skip_ai {
        println!("{BOLD}│{RESET}  Impact  : {BOLD}{reachable}{RESET} confirmed/likely reachable");
    }
    println!("{BOLD}├──────────────────────────────────────────────────────────────┤{RESET}");

    let findings_to_show: Vec<_> = if skip_ai {
        // Show all vulns sorted by severity.
        let mut all: Vec<ScaFinding> = vulns.iter().map(|v| {
            let cve = v.aliases.iter().find(|a| a.starts_with("CVE-")).cloned()
                .unwrap_or_else(|| v.id.clone());
            let fix = v.fix_versions.first().cloned().unwrap_or_default();
            ScaFinding {
                severity: v.severity.clone(),
                title: v.summary.clone(),
                cve_id: cve,
                package: v.pkg_name.clone(),
                version: v.pkg_version.clone(),
                is_direct: v.is_direct,
                via: v.via.clone(),
                fix_version: fix,
                reachable: true,
                reachability_confidence: "not_analyzed".into(),
                description: v.details.chars().take(300).collect(),
                ai_reasoning: String::new(),
            }
        }).collect();
        all.sort_by(|a, b| sast_severity_level(&b.severity).cmp(&sast_severity_level(&a.severity)));
        all
    } else {
        let mut f = findings.to_vec();
        f.sort_by(|a, b| sast_severity_level(&b.severity).cmp(&sast_severity_level(&a.severity)));
        f
    };

    for sev in &["critical", "high", "medium", "low"] {
        let n = findings_to_show.iter().filter(|f| f.severity.to_lowercase() == *sev).count();
        if n > 0 {
            let col = sast_severity_color(sev);
            println!("{BOLD}│{RESET}  {col}{:<10}{RESET}  {BOLD}{n}{RESET}", sev.to_uppercase());
        }
    }

    if findings_to_show.is_empty() {
        println!("{BOLD}│{RESET}  {GREEN}No impactful vulnerabilities found.{RESET}");
        println!("{BOLD}└──────────────────────────────────────────────────────────────┘{RESET}");
        return;
    }

    println!("{BOLD}├──────────────────────────────────────────────────────────────┤{RESET}");

    for (i, f) in findings_to_show.iter().enumerate() {
        let col = sast_severity_color(&f.severity);
        let confidence_badge = match f.reachability_confidence.as_str() {
            "confirmed_reachable" => format!("{GREEN}[CONFIRMED REACHABLE]{RESET}"),
            "likely_reachable"    => format!("\x1b[33m[LIKELY REACHABLE]{RESET}"),
            "not_analyzed"        => format!("{DIM}[NOT ANALYZED]{RESET}"),
            _                     => format!("{DIM}[UNKNOWN]{RESET}"),
        };
        let via_str = if f.via.is_empty() { "direct".to_string() }
                      else { format!("via {}", f.via.join(" -> ")) };
        println!("{BOLD}│{RESET}");
        println!("{BOLD}│{RESET}  {BOLD}#{}{RESET}  {col}[{}]{RESET}  {BOLD}{}{RESET}", i + 1, f.severity.to_uppercase(), f.cve_id);
        println!("{BOLD}│{RESET}      {BOLD}{}{RESET}", f.title);
        println!("{BOLD}│{RESET}      {DIM}{}@{}  {}  {}{RESET}", f.package, f.version, via_str, confidence_badge);
        if !f.fix_version.is_empty() {
            println!("{BOLD}│{RESET}      {DIM}Fix: upgrade to {}{RESET}", f.fix_version);
        }
        for line in wrap_text(&f.description, 66) {
            println!("{BOLD}│{RESET}      {line}");
        }
        if !f.ai_reasoning.is_empty() {
            println!("{BOLD}│{RESET}      {DIM}AI: {}{RESET}", &f.ai_reasoning.chars().take(160).collect::<String>());
        }
    }
    println!("{BOLD}└──────────────────────────────────────────────────────────────┘{RESET}");
}

fn sca_to_sarif(findings: &[ScaFinding], target: &str) -> serde_json::Value {
    let rules: Vec<serde_json::Value> = findings.iter().enumerate().map(|(i, f)| {
        serde_json::json!({
            "id": format!("STROBES-SCA-{:03}", i + 1),
            "shortDescription": { "text": format!("{} in {}", f.cve_id, f.package) },
            "properties": { "severity": f.severity, "cve": f.cve_id },
        })
    }).collect();
    let results: Vec<serde_json::Value> = findings.iter().enumerate().map(|(i, f)| {
        let level = match f.severity.to_lowercase().as_str() {
            "critical" | "high" => "error",
            "medium" => "warning",
            _ => "note",
        };
        serde_json::json!({
            "ruleId": format!("STROBES-SCA-{:03}", i + 1),
            "level": level,
            "message": { "text": format!("{}: {}\n\n{}", f.cve_id, f.title, f.description) },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": format!("{}@{}", f.package, f.version) }
                }
            }],
            "properties": {
                "severity": f.severity, "package": f.package, "version": f.version,
                "fix_version": f.fix_version, "reachable": f.reachable,
                "reachability": f.reachability_confidence,
            }
        })
    }).collect();
    serde_json::json!({
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{ "tool": { "driver": {
            "name": "Strobes SCA", "version": env!("CARGO_PKG_VERSION"),
            "informationUri": "https://strobes.co", "rules": rules,
        }}, "results": results,
        "automationDetails": { "id": format!("strobes/sca/{}", target) } }]
    })
}

// ── cmd_scan_sca ───────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn cmd_scan_sca(
    p: &config::Profile,
    dir: String,
    output_fmt: String,
    output_file: Option<String>,
    workspace: Option<String>,
    model: Option<i64>,
    timeout: u64,
    fail_on: Option<String>,
    skip_ai: bool,
    min_severity: String,
) -> Result<()> {
    require_complete(p)?;

    // 1. Resolve directory.
    let src = std::path::Path::new(&dir)
        .canonicalize()
        .map_err(|e| anyhow!("cannot access '{}': {}", dir, e))?;
    if !src.is_dir() { return Err(anyhow!("'{}' is not a directory", dir)); }
    let target_name = src.file_name().and_then(|n| n.to_str()).unwrap_or("scan").to_string();

    // 2. Parse all manifests.
    eprintln!("scanning manifests in {}…", src.display());
    let deps = parse_all_deps(&src);
    if deps.is_empty() {
        eprintln!("no supported dependency manifests found");
        eprintln!("supported: requirements.txt, poetry.lock, Pipfile.lock, package-lock.json,");
        eprintln!("           yarn.lock, go.mod+go.sum, Cargo.lock, Gemfile.lock,");
        eprintln!("           composer.lock, pom.xml, build.gradle, packages.lock.json, *.csproj");
        return Ok(());
    }

    // Group by ecosystem for display.
    let mut eco_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for d in &deps { *eco_counts.entry(d.ecosystem.as_str()).or_default() += 1; }
    for (eco, n) in &eco_counts {
        eprintln!("  {eco}: {n} package(s)");
    }
    eprintln!("  total: {} unique package(s)", deps.len());

    // 3. Query OSV.dev.
    eprintln!("\nquerying OSV.dev for CVEs…");
    let mut all_vulns = query_osv(&deps).await?;

    // Filter by min_severity.
    let min_level = sast_severity_level(&min_severity);
    all_vulns.retain(|v| sast_severity_level(&v.severity) >= min_level);

    eprintln!("  {} CVE(s) found (severity >= {})", all_vulns.len(), min_severity);

    if all_vulns.is_empty() {
        print_sca_summary(&[], &[], &target_name, std::time::Duration::from_secs(0), skip_ai);
        return Ok(());
    }

    // 4. If --skip-ai, report all CVEs directly.
    if skip_ai {
        let elapsed = std::time::Duration::from_secs(0);
        print_sca_summary(&all_vulns, &[], &target_name, elapsed, true);
        let output_content = format_sca_output(&output_fmt, &[], &all_vulns, &target_name, "", elapsed, true)?;
        write_sca_output(output_content, &output_file, &output_fmt)?;
        return check_sca_gate(&fail_on, &all_vulns.iter().map(|v| {
            ScaFinding {
                severity: v.severity.clone(), title: v.summary.clone(),
                cve_id: v.aliases.iter().find(|a| a.starts_with("CVE-")).cloned().unwrap_or_else(|| v.id.clone()),
                package: v.pkg_name.clone(), version: v.pkg_version.clone(),
                is_direct: v.is_direct, via: v.via.clone(),
                fix_version: v.fix_versions.first().cloned().unwrap_or_default(),
                reachable: true, reachability_confidence: "not_analyzed".into(),
                description: String::new(), ai_reasoning: String::new(),
            }
        }).collect::<Vec<_>>());
    }

    // 5. Copy codebase into sandbox for AI reachability.
    let sandbox_id = format!("sca-{}", target_name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>());
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let sandbox_path = home.join(".strobes-ai").join("sandboxes").join(&sandbox_id);
    if sandbox_path.exists() { std::fs::remove_dir_all(&sandbox_path)?; }
    std::fs::create_dir_all(&sandbox_path)?;
    let (file_count, byte_count) = copy_dir_to_sandbox(&src, &sandbox_path, &[], 100_000_000)?;
    eprintln!("  {} file(s) copied for reachability analysis ({:.1} KB)", file_count, byte_count as f64 / 1024.0);
    #[allow(deprecated)]
    std::env::set_var("STROBES_AI_SANDBOX", &sandbox_path);

    // 6. Build AI prompt and launch scan.
    let prompt = build_sca_prompt(&target_name, &all_vulns);
    let client = api::ApiClient::new(p.clone())?;
    let workspace_id: Option<String> = match workspace {
        Some(ws) => Some(ws),
        None => {
            let (id, _) = client.create_workspace(&format!("SCA: {target_name}")).await?;
            eprintln!("workspace: {id}");
            Some(id)
        }
    };
    let thread_id = client.create_thread(
        &format!("SCA reachability: {target_name}"), workspace_id.as_deref(), None).await?;
    eprintln!("thread: {thread_id}\n");

    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let handle = pulse::connect(p, &thread_id, tx, model).await?;
    handle.send_user_message(&prompt);

    let mut display = ScanDisplay::new(&target_name, file_count, byte_count, timeout);
    let start = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut render_tick = tokio::time::interval(std::time::Duration::from_millis(120));
    let mut full_text = String::new();
    display.push(format!("analysing {} CVE(s) for reachability…", all_vulns.len()));
    display.render(start.elapsed());

    let run_result: Result<()> = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                display.erase();
                eprintln!("error: timed out after {timeout}s");
                break Err(anyhow!("timed out after {timeout}s"));
            }
            _ = render_tick.tick() => { display.render(start.elapsed()); }
            ev_opt = rx.recv() => {
                match ev_opt {
                    None => { display.erase(); break Ok(()); }
                    Some(ev) => match ev {
                        pulse::AppEvent::RunFinished(_) => { display.erase(); break Ok(()); }
                        pulse::AppEvent::Stream(item) => match item.kind.as_str() {
                            "token" => { if let Some(t) = &item.text { full_text.push_str(t); } }
                            "tool_start" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let d = item.detail.as_deref().unwrap_or("");
                                let d: String = d.chars().take(48).collect();
                                display.push(format!("▶ {name}({d})"));
                                display.render(start.elapsed());
                            }
                            "tool_output" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let d = item.detail.as_deref().unwrap_or("");
                                if !d.is_empty() {
                                    let d: String = d.chars().take(52).collect();
                                    display.push(format!("◀ {name}: {d}"));
                                    display.render(start.elapsed());
                                }
                            }
                            _ => { if let Some(t) = &item.text { full_text.push_str(t); } }
                        },
                        pulse::AppEvent::Error(e) => { display.erase(); break Err(anyhow!("{e}")); }
                        pulse::AppEvent::Interrupt { .. } => {
                            display.erase();
                            break Err(anyhow!("agent requested human input during non-interactive scan"));
                        }
                        _ => {}
                    },
                }
            }
        }
    };

    run_result?;
    let elapsed = start.elapsed();

    // 7. Parse and display findings.
    let mut findings = parse_sast_output(&full_text).into_iter().map(|f| {
        ScaFinding {
            severity: f.severity, title: f.title, cve_id: String::new(),
            package: String::new(), version: String::new(),
            is_direct: true, via: vec![], fix_version: String::new(),
            reachable: true, reachability_confidence: "confirmed_reachable".into(),
            description: f.description, ai_reasoning: String::new(),
        }
    }).collect::<Vec<_>>();
    // Prefer the dedicated ScaFinding parse from the JSON block.
    if let Some(start_idx) = full_text.find("```json") {
        let after = &full_text[start_idx + 7..];
        let json_str = if let Some(end) = after.find("```") { &after[..end] } else { after };
        #[derive(serde::Deserialize)]
        struct Wrapper { findings: Vec<ScaFinding> }
        if let Ok(w) = serde_json::from_str::<Wrapper>(json_str.trim()) {
            findings = w.findings;
        }
    }
    findings.sort_by(|a, b| sast_severity_level(&b.severity).cmp(&sast_severity_level(&a.severity)));

    print_sca_summary(&all_vulns, &findings, &target_name, elapsed, false);
    let output_content = format_sca_output(&output_fmt, &findings, &all_vulns, &target_name, &thread_id, elapsed, false)?;
    write_sca_output(output_content, &output_file, &output_fmt)?;
    check_sca_gate(&fail_on, &findings)
}

fn format_sca_output(
    fmt: &str,
    findings: &[ScaFinding],
    vulns: &[OsvVuln],
    target: &str,
    thread_id: &str,
    elapsed: std::time::Duration,
    skip_ai: bool,
) -> Result<String> {
    Ok(match fmt {
        "json" => serde_json::to_string_pretty(&serde_json::json!({
            "scan_target": target,
            "thread_id": thread_id,
            "elapsed_secs": elapsed.as_secs(),
            "total_cves": vulns.len(),
            "skip_ai": skip_ai,
            "findings": findings,
        }))?,
        "sarif" => serde_json::to_string_pretty(&sca_to_sarif(findings, target))?,
        _ => String::new(),
    })
}

fn write_sca_output(content: String, path: &Option<String>, fmt: &str) -> Result<()> {
    if content.is_empty() { return Ok(()); }
    if let Some(p) = path {
        std::fs::write(p, &content)?;
        eprintln!("results saved → {p}");
    } else if fmt != "text" {
        println!("{content}");
    }
    Ok(())
}

fn check_sca_gate(fail_on: &Option<String>, findings: &[ScaFinding]) -> Result<()> {
    if let Some(threshold) = fail_on {
        let level = sast_severity_level(threshold);
        let blocking: Vec<_> = findings.iter()
            .filter(|f| sast_severity_level(&f.severity) >= level)
            .collect();
        if !blocking.is_empty() {
            return Err(anyhow!("{} SCA finding(s) at or above '{}' severity", blocking.len(), threshold));
        }
    }
    Ok(())
}

// ── strobes ci container ────────────────────────────────────────────────────

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

struct ExtractedContainer {
    deps: Vec<Dep>,
    os_label: String,
    extract_dir: std::path::PathBuf,
}

/// RAII guard — removes the container on drop.
struct ContainerGuard(String);
impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.0])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
    }
}

fn docker_cp(container: &str, src: &str, dest: &std::path::Path) -> bool {
    std::process::Command::new("docker")
        .args(["cp", &format!("{container}:{src}"), dest.to_str().unwrap_or(".")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn extract_container(image: &str, platform: Option<&str>) -> Result<ExtractedContainer> {
    let uid = uuid::Uuid::new_v4();
    let short = uid.to_string().split('-').next().unwrap_or("tmp").to_string();
    let container_name = format!("strobes-scan-{short}");

    let extract_dir = std::env::temp_dir().join(format!("strobes-container-{short}"));
    std::fs::create_dir_all(&extract_dir)?;

    // Create container without starting it.
    let mut cmd = std::process::Command::new("docker");
    cmd.args(["create", "--name", &container_name]);
    if let Some(p) = platform { cmd.args(["--platform", p]); }
    cmd.arg(image);
    let out = cmd.output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("docker create failed: {}", msg.trim()));
    }
    let _guard = ContainerGuard(container_name.clone());

    // Detect OS. /etc/os-release is often a symlink; try both paths.
    let os_release_path = extract_dir.join("os-release");
    if !docker_cp(&container_name, "/etc/os-release", &os_release_path)
        || std::fs::metadata(&os_release_path).map(|m| m.len() == 0).unwrap_or(true)
    {
        docker_cp(&container_name, "/usr/lib/os-release", &os_release_path);
    }
    let (os_name, os_ver) = parse_os_release(&os_release_path);
    let ecosystem = os_to_ecosystem(&os_name);
    let os_label = if os_ver.is_empty() {
        os_name.clone()
    } else {
        format!("{} {}", os_name, os_ver)
    };

    // Extract OS package databases.
    let dpkg_path = extract_dir.join("dpkg-status");
    let apk_path  = extract_dir.join("apk-installed");
    docker_cp(&container_name, "/var/lib/dpkg/status", &dpkg_path);
    docker_cp(&container_name, "/lib/apk/db/installed", &apk_path);

    // Extract app directories into a sub-dir so parsers can find manifests.
    let app_dir = extract_dir.join("app");
    std::fs::create_dir_all(&app_dir)?;
    for dir in &["/app", "/usr/src/app", "/srv/app", "/home/app", "/opt/app", "/workspace", "/code"] {
        if docker_cp(&container_name, dir, &app_dir) { break; }
    }

    // Parse packages.
    let mut deps: Vec<Dep> = vec![];

    if dpkg_path.exists() {
        deps.extend(parse_dpkg_status(&dpkg_path, ecosystem));
    }
    if apk_path.exists() {
        deps.extend(parse_apk_installed(&apk_path));
    }

    // App-level deps (Python/Node/Go/etc.) from the extracted app dir.
    if app_dir.exists() {
        deps.extend(parse_all_deps(&app_dir));
    }

    // Deduplicate.
    let mut seen = std::collections::HashSet::new();
    deps.retain(|d| seen.insert((d.ecosystem.clone(), d.name.to_lowercase(), d.version.clone())));

    Ok(ExtractedContainer { deps, os_label, extract_dir })
}

fn parse_os_release(path: &std::path::Path) -> (String, String) {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut name = String::new();
    let mut version = String::new();
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("NAME=") {
            name = v.trim().trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            version = v.trim().trim_matches('"').to_string();
        }
    }
    (if name.is_empty() { "Linux".into() } else { name }, version)
}

fn os_to_ecosystem(os_name: &str) -> &'static str {
    let n = os_name.to_lowercase();
    if n.contains("ubuntu")    { return "Ubuntu"; }
    if n.contains("debian")    { return "Debian"; }
    if n.contains("alpine")    { return "Alpine"; }
    if n.contains("rhel") || n.contains("red hat") { return "Red Hat"; }
    if n.contains("centos")    { return "Red Hat"; }
    if n.contains("fedora")    { return "Red Hat"; }
    if n.contains("rocky")     { return "Rocky Linux"; }
    if n.contains("alma")      { return "AlmaLinux"; }
    "Debian"  // safe fallback for most distros
}

fn parse_dpkg_status(path: &std::path::Path, ecosystem: &str) -> Vec<Dep> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut deps = vec![];
    let mut name = String::new();
    let mut ver  = String::new();
    let mut status = String::new();
    for line in content.lines() {
        if line.is_empty() {
            if !name.is_empty() && !ver.is_empty() && status.contains("installed") {
                deps.push(Dep {
                    name: name.clone(), version: ver.clone(),
                    ecosystem: ecosystem.to_string(),
                    is_direct: true, via: vec![], manifest: "dpkg/status".into(),
                });
            }
            name.clear(); ver.clear(); status.clear();
        } else if let Some(v) = line.strip_prefix("Package: ") { name = v.trim().into(); }
        else if let Some(v) = line.strip_prefix("Version: ")   { ver  = v.trim().into(); }
        else if let Some(v) = line.strip_prefix("Status: ")    { status = v.trim().into(); }
    }
    // Last stanza (file may not end with blank line).
    if !name.is_empty() && !ver.is_empty() && status.contains("installed") {
        deps.push(Dep { name, version: ver, ecosystem: ecosystem.to_string(),
            is_direct: true, via: vec![], manifest: "dpkg/status".into() });
    }
    deps
}

fn parse_apk_installed(path: &std::path::Path) -> Vec<Dep> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut deps = vec![];
    let mut name = String::new();
    let mut ver  = String::new();
    for line in content.lines() {
        if line.is_empty() {
            if !name.is_empty() && !ver.is_empty() {
                deps.push(Dep { name: name.clone(), version: ver.clone(),
                    ecosystem: "Alpine".into(), is_direct: true, via: vec![],
                    manifest: "apk/db/installed".into() });
            }
            name.clear(); ver.clear();
        } else if let Some(v) = line.strip_prefix("P:") { name = v.trim().into(); }
        else if let Some(v) = line.strip_prefix("V:") { ver  = v.trim().into(); }
    }
    if !name.is_empty() && !ver.is_empty() {
        deps.push(Dep { name, version: ver, ecosystem: "Alpine".into(),
            is_direct: true, via: vec![], manifest: "apk/db/installed".into() });
    }
    deps
}

fn build_container_prompt(image: &str, os_label: &str, vulns: &[OsvVuln]) -> String {
    let vuln_list = vulns.iter().enumerate().map(|(i, v)| {
        let cve = v.aliases.iter().find(|a| a.starts_with("CVE-")).cloned()
            .unwrap_or_else(|| v.id.clone());
        let fix = if v.fix_versions.is_empty() { "no fix yet".to_string() }
                  else { v.fix_versions.join(", ") };
        format!(
            "  [{idx}] {cve} | {sev} | {pkg}@{ver} ({eco})\n       Summary: {summary}\n       Fix: {fix}",
            idx = i + 1, cve = cve, sev = v.severity.to_uppercase(),
            pkg = v.pkg_name, ver = v.pkg_version, eco = v.pkg_ecosystem,
            summary = v.summary, fix = fix
        )
    }).collect::<Vec<_>>().join("\n\n");

    format!(
        r#"You are a security engineer performing a container image security audit.

Image: {image}
Base OS: {os_label}
The extracted container filesystem is available in your sandbox — use your tools to read it.

## CVEs Found in Container Packages

{vuln_list}

## Your Task

For EACH CVE above, determine:

1. **Exposed** — Is the vulnerable package actually present and used?
   - OS packages: check if the binary/library is present and callable
   - App packages: trace import/usage in the application code under /app/

2. **Reachable from outside** — Is the vulnerable code path triggerable by an external actor?
   - Consider the container's exposed services (HTTP, gRPC, etc.)
   - Check if attacker-controlled input reaches the vulnerable function
   - Look for compensating controls (auth, input validation, network isolation)

3. **Verdict** — For each CVE:
   - `confirmed_reachable` — binary/library used AND reachable by external input
   - `likely_reachable` — used but can't fully trace external exposure
   - `not_reachable` — package present but the vulnerable path is never exercised
   - `unknown` — insufficient info to determine

## Output Format

After your analysis, output a JSON block EXACTLY like this:

```json
{{
  "scan_target": "{image}",
  "findings": [
    {{
      "severity": "critical",
      "title": "Log4Shell in log4j-core — confirmed reachable",
      "cve_id": "CVE-2021-44228",
      "package": "log4j-core",
      "version": "2.14.1",
      "is_direct": true,
      "via": [],
      "fix_version": "2.17.0",
      "reachable": true,
      "reachability_confidence": "confirmed_reachable",
      "description": "log4j-core 2.14.1 is present and the application logs user-controlled HTTP headers at /app/src/Server.java:82, directly triggering the JNDI lookup.",
      "ai_reasoning": "Found log4j-core JAR in /app/lib/. Application code at Server.java:82 calls logger.info(request.getHeader(\"X-Api-Version\")), passing the attacker-controlled header to log4j."
    }}
  ]
}}
```

Only include `confirmed_reachable` or `likely_reachable` findings in the JSON.
Note `not_reachable` ones in your analysis text but exclude them from the JSON.
Begin."#
    )
}

#[allow(clippy::too_many_arguments)]
async fn cmd_scan_container(
    p: &config::Profile,
    image: String,
    output_fmt: String,
    output_file: Option<String>,
    workspace: Option<String>,
    model: Option<i64>,
    timeout: u64,
    fail_on: Option<String>,
    skip_ai: bool,
    min_severity: String,
    platform: Option<String>,
) -> Result<()> {
    require_complete(p)?;

    if !docker_available() {
        return Err(anyhow!(
            "docker is not available — install Docker and ensure the daemon is running"
        ));
    }

    // 1. Pull image.
    eprint!("pulling {} … ", image);
    let mut pull = std::process::Command::new("docker");
    pull.args(["pull", &image]);
    if let Some(ref plt) = platform { pull.args(["--platform", plt]); }
    let pull_out = pull.output()?;
    if pull_out.status.success() {
        eprintln!("done");
    } else {
        eprintln!("(not pulled — using local image if available)");
    }

    // 2. Extract container FS.
    eprintln!("extracting packages from container …");
    let scan = extract_container(&image, platform.as_deref())?;

    let image_label = image.split('/').last().unwrap_or(&image)
        .split(':').next().unwrap_or(&image);
    eprintln!("  OS: {}", scan.os_label);

    if scan.deps.is_empty() {
        eprintln!("no packages found — unsupported base image or empty container");
        return Ok(());
    }

    let mut eco_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for d in &scan.deps { *eco_counts.entry(d.ecosystem.as_str()).or_default() += 1; }
    let mut eco_list: Vec<(&str, usize)> = eco_counts.into_iter().collect();
    eco_list.sort_by_key(|e| e.0);
    for (eco, n) in &eco_list { eprintln!("  {eco}: {n} package(s)"); }
    eprintln!("  total: {} package(s)", scan.deps.len());

    // 3. Query OSV.dev.
    eprintln!("\nquerying OSV.dev for CVEs …");
    let mut all_vulns = query_osv(&scan.deps).await?;
    let min_level = sast_severity_level(&min_severity);
    all_vulns.retain(|v| sast_severity_level(&v.severity) >= min_level);
    eprintln!("  {} CVE(s) found (severity >= {})", all_vulns.len(), min_severity);

    if all_vulns.is_empty() {
        print_sca_summary(&[], &[], image_label, std::time::Duration::from_secs(0), skip_ai);
        return Ok(());
    }

    // 4. --skip-ai: report all CVEs directly.
    if skip_ai {
        let findings: Vec<ScaFinding> = all_vulns.iter().map(|v| ScaFinding {
            severity: v.severity.clone(), title: v.summary.clone(),
            cve_id: v.aliases.iter().find(|a| a.starts_with("CVE-")).cloned()
                .unwrap_or_else(|| v.id.clone()),
            package: v.pkg_name.clone(), version: v.pkg_version.clone(),
            is_direct: v.is_direct, via: v.via.clone(),
            fix_version: v.fix_versions.first().cloned().unwrap_or_default(),
            reachable: true, reachability_confidence: "not_analyzed".into(),
            description: v.details.chars().take(300).collect(),
            ai_reasoning: String::new(),
        }).collect();
        print_sca_summary(&all_vulns, &[], image_label, std::time::Duration::from_secs(0), true);
        let content = format_sca_output(&output_fmt, &findings, &all_vulns, image_label, "", std::time::Duration::from_secs(0), true)?;
        write_sca_output(content, &output_file, &output_fmt)?;
        return check_sca_gate(&fail_on, &findings);
    }

    // 5. Copy extracted dir to AI sandbox.
    let sandbox_id = format!("container-{}", image_label.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>());
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let sandbox_path = home.join(".strobes-ai").join("sandboxes").join(&sandbox_id);
    if sandbox_path.exists() { std::fs::remove_dir_all(&sandbox_path)?; }
    let (file_count, byte_count) = copy_dir_to_sandbox(&scan.extract_dir, &sandbox_path, &[], 100_000_000)?;
    #[allow(deprecated)]
    std::env::set_var("STROBES_AI_SANDBOX", &sandbox_path);

    // 6. Launch AI reachability.
    let prompt = build_container_prompt(&image, &scan.os_label, &all_vulns);
    let client = api::ApiClient::new(p.clone())?;
    let workspace_id: Option<String> = match workspace {
        Some(ws) => Some(ws),
        None => {
            let (id, _) = client.create_workspace(&format!("Container: {image}")).await?;
            eprintln!("workspace: {id}");
            Some(id)
        }
    };
    let thread_id = client.create_thread(
        &format!("Container scan: {image}"), workspace_id.as_deref(), None).await?;
    eprintln!("thread: {thread_id}\n");

    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let handle = pulse::connect(p, &thread_id, tx, model).await?;
    handle.send_user_message(&prompt);

    let mut display = ScanDisplay::new(&image, file_count, byte_count, timeout);
    let start    = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut render_tick = tokio::time::interval(std::time::Duration::from_millis(120));
    let mut full_text = String::new();
    display.push(format!("analysing {} CVE(s) in {} …", all_vulns.len(), scan.os_label));
    display.render(start.elapsed());

    let run_result: Result<()> = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                display.erase();
                break Err(anyhow!("timed out after {timeout}s"));
            }
            _ = render_tick.tick() => { display.render(start.elapsed()); }
            ev_opt = rx.recv() => {
                match ev_opt {
                    None => { display.erase(); break Ok(()); }
                    Some(ev) => match ev {
                        pulse::AppEvent::RunFinished(_) => { display.erase(); break Ok(()); }
                        pulse::AppEvent::Stream(item) => match item.kind.as_str() {
                            "token" => { if let Some(t) = &item.text { full_text.push_str(t); } }
                            "tool_start" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let d: String = item.detail.as_deref().unwrap_or("").chars().take(48).collect();
                                display.push(format!("▶ {name}({d})"));
                                display.render(start.elapsed());
                            }
                            "tool_output" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let d: String = item.detail.as_deref().unwrap_or("").chars().take(52).collect();
                                if !d.is_empty() { display.push(format!("◀ {name}: {d}")); display.render(start.elapsed()); }
                            }
                            _ => { if let Some(t) = &item.text { full_text.push_str(t); } }
                        },
                        pulse::AppEvent::Error(e) => { display.erase(); break Err(anyhow!("{e}")); }
                        pulse::AppEvent::Interrupt { .. } => {
                            display.erase();
                            break Err(anyhow!("agent requested human input during non-interactive scan"));
                        }
                        _ => {}
                    }
                }
            }
        }
    };

    run_result?;
    let elapsed = start.elapsed();

    // 7. Parse findings JSON from AI output.
    let mut findings: Vec<ScaFinding> = vec![];
    if let Some(start_idx) = full_text.find("```json") {
        let after = &full_text[start_idx + 7..];
        let json_str = if let Some(end) = after.find("```") { &after[..end] } else { after };
        #[derive(serde::Deserialize)]
        struct Wrapper { findings: Vec<ScaFinding> }
        if let Ok(w) = serde_json::from_str::<Wrapper>(json_str.trim()) {
            findings = w.findings;
        }
    }
    findings.sort_by(|a, b| sast_severity_level(&b.severity).cmp(&sast_severity_level(&a.severity)));

    print_sca_summary(&all_vulns, &findings, image_label, elapsed, false);
    let content = format_sca_output(&output_fmt, &findings, &all_vulns, image_label, &thread_id, elapsed, false)?;
    write_sca_output(content, &output_file, &output_fmt)?;
    check_sca_gate(&fail_on, &findings)
}

// ── strobes ci iac ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum IacType {
    Terraform,
    CloudFormation,
    Kubernetes,
    Helm,
    Dockerfile,
    DockerCompose,
    GitHubActions,
    Ansible,
    ArmTemplate,
}

impl IacType {
    fn label(&self) -> &'static str {
        match self {
            IacType::Terraform     => "Terraform",
            IacType::CloudFormation => "CloudFormation",
            IacType::Kubernetes    => "Kubernetes",
            IacType::Helm          => "Helm",
            IacType::Dockerfile    => "Dockerfile",
            IacType::DockerCompose => "Docker Compose",
            IacType::GitHubActions => "GitHub Actions",
            IacType::Ansible       => "Ansible",
            IacType::ArmTemplate   => "ARM Template",
        }
    }

    fn slug(&self) -> &'static str {
        match self {
            IacType::Terraform     => "terraform",
            IacType::CloudFormation => "cloudformation",
            IacType::Kubernetes    => "kubernetes",
            IacType::Helm          => "helm",
            IacType::Dockerfile    => "dockerfile",
            IacType::DockerCompose => "compose",
            IacType::GitHubActions => "github-actions",
            IacType::Ansible       => "ansible",
            IacType::ArmTemplate   => "arm",
        }
    }
}

struct IacFile {
    abs_path: std::path::PathBuf,
    rel_path: String,
    iac_type: IacType,
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
struct IacFinding {
    severity: String,
    title: String,
    file: String,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    resource: String,
    #[serde(default)]
    iac_type: String,
    description: String,
    #[serde(default)]
    fix: String,
}

// ── IaC file detection ─────────────────────────────────────────────────────

const IAC_SKIP_DIRS: &[&str] = &[
    ".git", ".svn", "node_modules", "__pycache__", "target", "dist",
    "build", ".next", "vendor", "venv", ".venv", ".tox", ".cache",
    ".terraform", ".terragrunt-cache",
];

fn detect_iac_files(root: &std::path::Path, filter: &[String]) -> Vec<IacFile> {
    let mut files = vec![];
    walk_for_iac(root, root, &mut files);
    if !filter.is_empty() {
        files.retain(|f| filter.iter().any(|slug| slug == f.iac_type.slug()));
    }
    files.sort_by(|a, b| {
        a.iac_type.label().cmp(b.iac_type.label()).then(a.rel_path.cmp(&b.rel_path))
    });
    files
}

fn walk_for_iac(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<IacFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return; };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if IAC_SKIP_DIRS.contains(&name) { continue; }
            walk_for_iac(root, &path, out);
            continue;
        }
        if let Some(iac_type) = classify_iac_file(&path) {
            let rel_path = path.strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| name.to_string());
            out.push(IacFile { abs_path: path, rel_path, iac_type });
        }
    }
}

fn classify_iac_file(path: &std::path::Path) -> Option<IacType> {
    let name = path.file_name()?.to_str()?;
    let ext  = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    // ── Exact filename matches ──────────────────────────────────────────────
    if name == "Dockerfile" || name.starts_with("Dockerfile.") { return Some(IacType::Dockerfile); }
    if name == "docker-compose.yml" || name == "docker-compose.yaml"
        || name == "compose.yml"    || name == "compose.yaml"     { return Some(IacType::DockerCompose); }
    if name == "Chart.yaml" || name == "Chart.yml"                 { return Some(IacType::Helm); }
    if name == "Pulumi.yaml" || name == "Pulumi.yml"               { return None; } // skip Pulumi for now

    // ── Extension-based matches ─────────────────────────────────────────────
    if ext == "tf" || ext == "tfvars" { return Some(IacType::Terraform); }

    // YAML/JSON require content sniffing.
    if matches!(ext, "yaml" | "yml") {
        let head = read_head(path, 4096);
        return sniff_yaml(&head, name);
    }
    if ext == "json" {
        let head = read_head(path, 2048);
        return sniff_json(&head);
    }

    None
}

fn read_head(path: &std::path::Path, bytes: usize) -> String {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return String::new(); };
    let mut buf = vec![0u8; bytes];
    let n = f.read(&mut buf).unwrap_or(0);
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn sniff_yaml(content: &str, name: &str) -> Option<IacType> {
    // GitHub Actions — workflow files
    if name.ends_with(".yml") || name.ends_with(".yaml") {
        // In .github/workflows/ directory or has workflow-like structure
        let has_on = content.starts_with("on:") || content.contains("\non:")
            || content.starts_with("\"on\"") || content.contains("\n\"on\":");
        if has_on && content.contains("jobs:") { return Some(IacType::GitHubActions); }
    }
    // CloudFormation
    if content.contains("AWSTemplateFormatVersion") { return Some(IacType::CloudFormation); }
    if content.contains("Type: AWS::") || content.contains("Type: \"AWS::") {
        return Some(IacType::CloudFormation);
    }
    // Kubernetes — apiVersion + kind, not a Helm template
    if content.contains("apiVersion:") && content.contains("kind:") {
        // Helm templates use {{ }} — skip them (they're captured via Chart.yaml)
        if content.contains("{{") { return None; }
        return Some(IacType::Kubernetes);
    }
    // Docker Compose
    if content.contains("services:") && (content.contains("image:") || content.contains("build:")) {
        return Some(IacType::DockerCompose);
    }
    // Ansible playbook — list of plays with hosts + tasks
    if content.trim_start().starts_with("- ") && content.contains("hosts:") && content.contains("tasks:") {
        return Some(IacType::Ansible);
    }
    None
}

fn sniff_json(content: &str) -> Option<IacType> {
    // Azure ARM template
    if content.contains("\"$schema\"") && content.contains("deploymentTemplate") {
        return Some(IacType::ArmTemplate);
    }
    // CloudFormation JSON
    if content.contains("AWSTemplateFormatVersion") { return Some(IacType::CloudFormation); }
    None
}

// ── Prompt builder ─────────────────────────────────────────────────────────

fn build_iac_prompt(target: &str, files: &[IacFile]) -> String {
    // Group by type for the summary section.
    let mut by_type: std::collections::BTreeMap<String, Vec<&str>> = std::collections::BTreeMap::new();
    for f in files {
        by_type.entry(f.iac_type.label().to_string()).or_default().push(&f.rel_path);
    }
    let type_summary = by_type.iter().map(|(t, paths)| {
        format!("  {t} ({} file(s)):\n{}", paths.len(),
            paths.iter().map(|p| format!("    - {p}")).collect::<Vec<_>>().join("\n"))
    }).collect::<Vec<_>>().join("\n");

    format!(
        r#"You are a cloud security engineer performing an Infrastructure-as-Code (IaC) security audit.

Target: {target}
All IaC files listed below are available in your sandbox — use your file-reading tools to inspect each one.

## Detected IaC Files

{type_summary}

## What to Check

For each file type, look for the following classes of misconfiguration:

**Terraform**
- S3/GCS/Azure Blob buckets with public access or missing encryption
- Security groups / NSGs with 0.0.0.0/0 ingress on sensitive ports (22, 3306, 5432, 6379, 27017)
- IAM roles/policies with wildcard actions ("*") or overly broad resources
- Hardcoded secrets, passwords, or access keys in variables or resource attributes
- Unencrypted EBS/RDS/ElastiCache/Secrets
- RDS instances with `publicly_accessible = true`
- Missing deletion protection on critical resources (RDS, ALB)
- S3 buckets without versioning or logging
- VPCs with flow logs disabled
- Kubernetes clusters with overly permissive RBAC or public endpoint

**Kubernetes / Helm**
- Containers running as root (`runAsUser: 0` or missing `runAsNonRoot: true`)
- `securityContext.privileged: true`
- Missing CPU/memory `limits`
- `hostNetwork: true`, `hostPID: true`, `hostIPC: true`
- Secrets exposed as plain-text environment variables
- Overly permissive RBAC (ClusterAdmin, wildcard verbs/resources)
- `automountServiceAccountToken: true` on pods that don't need it
- Images using `latest` tag
- Missing `readOnlyRootFilesystem: true`
- Services of type LoadBalancer exposing unnecessary ports

**Dockerfile**
- No `USER` instruction (running as root)
- Using `latest` tag for base image
- Secrets or tokens in `ENV` or `ARG`
- `RUN curl | sh` or arbitrary internet downloads
- Excessive base image (use slim/distroless)
- Unnecessary `EXPOSE` of sensitive ports

**Docker Compose**
- `privileged: true`
- Ports bound to `0.0.0.0` unnecessarily
- Volumes mounting sensitive host directories (`/`, `/etc`, `/var/run/docker.sock`)
- Hardcoded secrets in `environment`
- Missing `restart: unless-stopped` or resource limits

**GitHub Actions**
- `pull_request_target` with checkout + code execution (script injection)
- Secrets or tokens echoed to logs
- Actions pinned to branch/tag instead of SHA (`uses: actions/checkout@main`)
- Overly permissive top-level `permissions: write-all`
- User-controlled input injected into `run:` shell commands via `${{{{ github.event.* }}}}`
- `GITHUB_TOKEN` with excessive permissions

**CloudFormation / ARM**
- Public S3 buckets (`PublicAccessBlockConfiguration` missing/disabled)
- Unencrypted RDS, S3, or EBS
- Hardcoded passwords in `Parameters` defaults or `Properties`
- Over-permissive IAM policies
- Missing MFA enforcement
- Security groups open to 0.0.0.0/0

**Ansible**
- Tasks using `shell` or `command` with user-supplied variables unquoted
- Hardcoded credentials or `vars` with secrets
- `no_log: false` on tasks handling secrets
- Missing privilege escalation guards

## Output Format

After reading and analysing every file, output a JSON block EXACTLY like this (no other JSON):

```json
{{
  "scan_target": "{target}",
  "findings": [
    {{
      "severity": "high",
      "title": "S3 bucket allows public read access",
      "file": "terraform/s3.tf",
      "line": 14,
      "resource": "aws_s3_bucket.assets",
      "iac_type": "terraform",
      "description": "The bucket ACL is set to 'public-read', exposing all objects. An attacker can enumerate and download any stored file without authentication.",
      "fix": "Remove the acl attribute or set it to 'private'. Add an aws_s3_bucket_public_access_block resource with all block flags set to true."
    }}
  ]
}}
```

Severity levels: critical, high, medium, low.
Include ALL confirmed misconfigurations — do not skip minor ones.
If a file has no issues, do not include it in the findings.
Begin."#
    )
}

// ── Output helpers ─────────────────────────────────────────────────────────

fn print_iac_summary(findings: &[IacFinding], target: &str, elapsed: std::time::Duration, file_count: usize) {
    println!();
    println!("{BOLD}┌─ Strobes IaC Scan Results ───────────────────────────────────┐{RESET}");
    println!("{BOLD}│{RESET}  Target  : {target}");
    println!("{BOLD}│{RESET}  Time    : {}s", elapsed.as_secs());
    println!("{BOLD}│{RESET}  Files   : {BOLD}{file_count}{RESET} IaC file(s) scanned");
    println!("{BOLD}├──────────────────────────────────────────────────────────────┤{RESET}");

    if findings.is_empty() {
        println!("{BOLD}│{RESET}  {GREEN}No misconfigurations found.{RESET}");
        println!("{BOLD}└──────────────────────────────────────────────────────────────┘{RESET}");
        return;
    }

    for sev in &["critical", "high", "medium", "low"] {
        let n = findings.iter().filter(|f| f.severity.to_lowercase() == *sev).count();
        if n > 0 {
            let col = sast_severity_color(sev);
            println!("{BOLD}│{RESET}  {col}{:<10}{RESET}  {BOLD}{n}{RESET}", sev.to_uppercase());
        }
    }
    println!("{BOLD}├──────────────────────────────────────────────────────────────┤{RESET}");

    for (i, f) in findings.iter().enumerate() {
        let col = sast_severity_color(&f.severity);
        let loc = if let Some(ln) = f.line {
            format!("{}:{}", f.file, ln)
        } else {
            f.file.clone()
        };
        println!("{BOLD}│{RESET}");
        println!("{BOLD}│{RESET}  {BOLD}#{}{RESET}  {col}[{}]{RESET}  {BOLD}{}{RESET}", i + 1, f.severity.to_uppercase(), f.title);
        if !f.resource.is_empty() {
            println!("{BOLD}│{RESET}      {DIM}resource: {}  |  {}{RESET}", f.resource, loc);
        } else {
            println!("{BOLD}│{RESET}      {DIM}{}{RESET}", loc);
        }
        for line in wrap_text(&f.description, 66) {
            println!("{BOLD}│{RESET}      {line}");
        }
        if !f.fix.is_empty() {
            println!("{BOLD}│{RESET}      {DIM}Fix: {}{RESET}", &f.fix.chars().take(140).collect::<String>());
        }
    }
    println!("{BOLD}└──────────────────────────────────────────────────────────────┘{RESET}");
}

fn iac_to_sarif(findings: &[IacFinding], target: &str) -> serde_json::Value {
    let rules: Vec<serde_json::Value> = findings.iter().enumerate().map(|(i, f)| {
        serde_json::json!({
            "id": format!("STROBES-IAC-{:03}", i + 1),
            "shortDescription": { "text": f.title.clone() },
            "properties": { "severity": f.severity, "iac_type": f.iac_type },
        })
    }).collect();
    let results: Vec<serde_json::Value> = findings.iter().enumerate().map(|(i, f)| {
        let level = match f.severity.to_lowercase().as_str() {
            "critical" | "high" => "error",
            "medium" => "warning",
            _ => "note",
        };
        serde_json::json!({
            "ruleId": format!("STROBES-IAC-{:03}", i + 1),
            "level": level,
            "message": { "text": format!("{}\n\n{}", f.description, f.fix) },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": f.file },
                    "region": { "startLine": f.line.unwrap_or(1) }
                }
            }],
            "properties": { "resource": f.resource, "iac_type": f.iac_type }
        })
    }).collect();
    serde_json::json!({
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{ "tool": { "driver": {
            "name": "Strobes IaC", "version": env!("CARGO_PKG_VERSION"),
            "informationUri": "https://strobes.co", "rules": rules,
        }}, "results": results,
        "automationDetails": { "id": format!("strobes/iac/{}", target) } }]
    })
}

// ── cmd_ci_iac ─────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn cmd_ci_iac(
    p: &config::Profile,
    dir: String,
    output_fmt: String,
    output_file: Option<String>,
    workspace: Option<String>,
    model: Option<i64>,
    timeout: u64,
    fail_on: Option<String>,
    only: Vec<String>,
) -> Result<()> {
    require_complete(p)?;

    let src = std::path::Path::new(&dir)
        .canonicalize()
        .map_err(|e| anyhow!("cannot access '{}': {}", dir, e))?;
    if !src.is_dir() { return Err(anyhow!("'{}' is not a directory", dir)); }
    let target_name = src.file_name().and_then(|n| n.to_str()).unwrap_or("iac-scan").to_string();

    // 1. Detect IaC files.
    eprintln!("detecting IaC files in {} …", src.display());
    let iac_files = detect_iac_files(&src, &only);

    if iac_files.is_empty() {
        eprintln!("no IaC files found");
        eprintln!("supported: Terraform (.tf), CloudFormation, Kubernetes, Helm,");
        eprintln!("           Dockerfile, Docker Compose, GitHub Actions, Ansible, ARM");
        return Ok(());
    }

    // Show breakdown by type.
    let mut by_type: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for f in &iac_files { *by_type.entry(f.iac_type.label().to_string()).or_default() += 1; }
    for (t, n) in &by_type { eprintln!("  {t}: {n} file(s)"); }
    eprintln!("  total: {} IaC file(s)", iac_files.len());

    // 2. Copy IaC files to sandbox preserving directory structure.
    let sandbox_id = format!("iac-{}", target_name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>());
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let sandbox_path = home.join(".strobes-ai").join("sandboxes").join(&sandbox_id);
    if sandbox_path.exists() { std::fs::remove_dir_all(&sandbox_path)?; }
    std::fs::create_dir_all(&sandbox_path)?;

    let mut total_bytes: u64 = 0;
    for f in &iac_files {
        let dest = sandbox_path.join(&f.rel_path);
        if let Some(parent) = dest.parent() { std::fs::create_dir_all(parent)?; }
        if let Ok(meta) = std::fs::metadata(&f.abs_path) {
            total_bytes += meta.len();
        }
        std::fs::copy(&f.abs_path, &dest)?;
    }
    eprintln!("  copied {:.1} KB to sandbox", total_bytes as f64 / 1024.0);
    #[allow(deprecated)]
    std::env::set_var("STROBES_AI_SANDBOX", &sandbox_path);

    // 3. Build prompt and launch scan.
    let prompt = build_iac_prompt(&target_name, &iac_files);
    let client = api::ApiClient::new(p.clone())?;
    let workspace_id: Option<String> = match workspace {
        Some(ws) => Some(ws),
        None => {
            let (id, _) = client.create_workspace(&format!("IaC: {target_name}")).await?;
            eprintln!("workspace: {id}");
            Some(id)
        }
    };
    let thread_id = client.create_thread(
        &format!("IaC scan: {target_name}"), workspace_id.as_deref(), None).await?;
    eprintln!("thread: {thread_id}\n");

    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let handle = pulse::connect(p, &thread_id, tx, model).await?;
    handle.send_user_message(&prompt);

    let mut display = ScanDisplay::new(&target_name, iac_files.len() as u64, total_bytes, timeout);
    let start    = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut render_tick = tokio::time::interval(std::time::Duration::from_millis(120));
    let mut full_text   = String::new();
    let type_list: Vec<_> = by_type.keys().cloned().collect();
    display.push(format!("scanning {} …", type_list.join(", ")));
    display.render(start.elapsed());

    let run_result: Result<()> = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                display.erase();
                break Err(anyhow!("timed out after {timeout}s"));
            }
            _ = render_tick.tick() => { display.render(start.elapsed()); }
            ev_opt = rx.recv() => {
                match ev_opt {
                    None => { display.erase(); break Ok(()); }
                    Some(ev) => match ev {
                        pulse::AppEvent::RunFinished(_) => { display.erase(); break Ok(()); }
                        pulse::AppEvent::Stream(item) => match item.kind.as_str() {
                            "token" => { if let Some(t) = &item.text { full_text.push_str(t); } }
                            "tool_start" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let d: String = item.detail.as_deref().unwrap_or("").chars().take(48).collect();
                                display.push(format!("▶ {name}({d})"));
                                display.render(start.elapsed());
                            }
                            "tool_output" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let d: String = item.detail.as_deref().unwrap_or("").chars().take(52).collect();
                                if !d.is_empty() { display.push(format!("◀ {name}: {d}")); display.render(start.elapsed()); }
                            }
                            _ => { if let Some(t) = &item.text { full_text.push_str(t); } }
                        },
                        pulse::AppEvent::Error(e) => { display.erase(); break Err(anyhow!("{e}")); }
                        pulse::AppEvent::Interrupt { .. } => {
                            display.erase();
                            break Err(anyhow!("agent requested human input during non-interactive scan"));
                        }
                        _ => {}
                    }
                }
            }
        }
    };

    run_result?;
    let elapsed = start.elapsed();

    // 4. Parse findings.
    let mut findings: Vec<IacFinding> = vec![];
    if let Some(start_idx) = full_text.find("```json") {
        let after = &full_text[start_idx + 7..];
        let json_str = if let Some(end) = after.find("```") { &after[..end] } else { after };
        #[derive(serde::Deserialize)]
        struct Wrapper { findings: Vec<IacFinding> }
        if let Ok(w) = serde_json::from_str::<Wrapper>(json_str.trim()) {
            findings = w.findings;
        }
    }
    findings.sort_by(|a, b| sast_severity_level(&b.severity).cmp(&sast_severity_level(&a.severity)));

    print_iac_summary(&findings, &target_name, elapsed, iac_files.len());

    // 5. Output.
    let content = match output_fmt.as_str() {
        "json" => serde_json::to_string_pretty(&serde_json::json!({
            "scan_target": target_name,
            "thread_id": thread_id,
            "elapsed_secs": elapsed.as_secs(),
            "iac_files": iac_files.len(),
            "findings": findings,
        }))?,
        "sarif" => serde_json::to_string_pretty(&iac_to_sarif(&findings, &target_name))?,
        _ => String::new(),
    };
    if !content.is_empty() {
        if let Some(ref p) = output_file {
            std::fs::write(p, &content)?;
            eprintln!("results saved → {p}");
        } else {
            println!("{content}");
        }
    }

    // 6. Fail gate.
    if let Some(threshold) = fail_on {
        let level = sast_severity_level(&threshold);
        let blocking: Vec<_> = findings.iter()
            .filter(|f| sast_severity_level(&f.severity) >= level)
            .collect();
        if !blocking.is_empty() {
            return Err(anyhow!("{} IaC finding(s) at or above '{}' severity", blocking.len(), threshold));
        }
    }
    Ok(())
}

// ── strobes ci dast ─────────────────────────────────────────────────────────

/// Build the default DAST prompt for a live-URL scan.
fn build_dast_prompt(url: &str, cookie: Option<&str>, bearer: Option<&str>, scope: &[String]) -> String {
    let auth_block = match (cookie, bearer) {
        (Some(c), _) => format!(
            "\n## Authentication\nInclude this cookie on all requests:\n  Cookie: {c}\n"
        ),
        (None, Some(t)) => format!(
            "\n## Authentication\nInclude this header on all requests:\n  Authorization: Bearer {t}\n"
        ),
        _ => String::new(),
    };

    let scope_block = if scope.is_empty() {
        String::new()
    } else {
        let paths = scope.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n");
        format!("\n## Scope restriction\nOnly test paths under these prefixes:\n{paths}\n")
    };

    format!(
        r#"You are a security engineer performing a comprehensive DAST (Dynamic Application Security Testing) scan.

Target URL: {url}
{auth_block}{scope_block}
## Methodology

Work through these phases in order:

### 1. Reconnaissance
- Fetch the root URL and map all links, forms, and API endpoints
- Identify the technology stack (server headers, cookies, error pages)
- Enumerate paths: try /robots.txt, /sitemap.xml, /.well-known/, /api, /swagger, /openapi.json, /graphql
- Check for exposed admin panels, debug endpoints, and developer tools

### 2. Crawl & Inventory
- Follow all internal links up to 2 levels deep
- Record every form (action, method, fields) and every API endpoint found
- Note all input parameters (query string, body fields, path params, headers)

### 3. Active Testing
Test every endpoint and parameter for:

**Injection**
- SQL injection: try `'`, `' OR '1'='1`, `1; DROP TABLE users--`
- Command injection: try `; id`, `| whoami`, `$(id)`, `` `id` ``
- SSTI: try `{{7*7}}`, `<%= 7*7 %>`, `${{7*7}}`
- Path traversal: try `../../../etc/passwd`, `....//....//etc/passwd`

**Authentication & Session**
- Try accessing authenticated endpoints without credentials
- Check for default credentials on admin panels
- Test for JWT algorithm confusion (alg: none, RS256→HS256)
- Check cookie flags: Secure, HttpOnly, SameSite

**Injection via headers**
- Host header injection
- X-Forwarded-For / X-Real-IP bypass
- SSRF via Referer, redirect params, webhook URLs

**Business logic**
- Negative quantities, zero-price purchases
- Mass assignment: send extra JSON fields not in the form
- IDOR: change numeric IDs in URLs to access other users' data
- Privilege escalation: try low-privilege actions that should require admin

**Information disclosure**
- Verbose error messages with stack traces
- Directory listing
- Exposed .git, .env, .DS_Store, backup files (.bak, ~, .old)
- Sensitive data in response bodies (tokens, keys, PII)

**Security headers**
- Missing: Content-Security-Policy, X-Frame-Options, HSTS, X-Content-Type-Options
- CORS misconfiguration: test with Origin: https://evil.com

### 4. Output

For EVERY confirmed or highly-probable finding, provide:
1. Title
2. Severity: critical / high / medium / low
3. URL and parameter affected
4. HTTP request that triggers it (method, path, body)
5. HTTP response evidence (status code, key response text)
6. Description
7. Recommended fix

After your analysis output a JSON block EXACTLY like this:

```json
{{
  "scan_target": "{url}",
  "findings": [
    {{
      "severity": "critical",
      "title": "SQL Injection in /api/users",
      "url": "{url}/api/users?id=1'",
      "parameter": "id",
      "method": "GET",
      "request": "GET /api/users?id=1' HTTP/1.1\\nHost: ...",
      "evidence": "500 Internal Server Error: syntax error near '1''",
      "description": "The id parameter is interpolated into a SQL query without sanitisation.",
      "fix": "Use parameterised queries."
    }}
  ]
}}
```

If no findings are found, output `{{"scan_target": "{url}", "findings": []}}`.

Begin the scan now."#
    )
}

/// A DAST finding parsed from the agent's output.
#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
struct DastFinding {
    severity: String,
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    parameter: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    request: String,
    #[serde(default)]
    evidence: String,
    description: String,
    #[serde(default)]
    fix: String,
}

fn parse_dast_output(text: &str) -> Vec<DastFinding> {
    let json_str = if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        if let Some(end) = after.find("```") { after[..end].trim() } else { after.trim() }
    } else if let Some(start) = text.find('{') {
        let end = text.rfind('}').map(|e| e + 1).unwrap_or(text.len());
        &text[start..end]
    } else {
        return vec![];
    };

    #[derive(serde::Deserialize)]
    struct Wrapper { findings: Vec<DastFinding> }
    serde_json::from_str::<Wrapper>(json_str)
        .map(|w| w.findings)
        .unwrap_or_default()
}

fn print_dast_summary(findings: &[DastFinding], target: &str, elapsed: std::time::Duration) {
    let total = findings.len();
    let secs = elapsed.as_secs();

    println!();
    println!("{BOLD}┌─ Strobes DAST Scan Results ─────────────────────────────────┐{RESET}");
    println!("{BOLD}│{RESET}  Target  : {target}");
    println!("{BOLD}│{RESET}  Time    : {secs}s");
    println!("{BOLD}│{RESET}  Total   : {BOLD}{total}{RESET} finding(s)");
    println!("{BOLD}├──────────────────────────────────────────────────────────────┤{RESET}");

    for sev in &["critical", "high", "medium", "low"] {
        let n = findings.iter().filter(|f| f.severity.to_lowercase() == *sev).count();
        if n > 0 {
            let col = sast_severity_color(sev);
            println!("{BOLD}│{RESET}  {col}{:<10}{RESET}  {BOLD}{n}{RESET}", sev.to_uppercase());
        }
    }

    if total == 0 {
        println!("{BOLD}│{RESET}  {GREEN}No findings — clean scan.{RESET}");
        println!("{BOLD}└──────────────────────────────────────────────────────────────┘{RESET}");
        return;
    }

    println!("{BOLD}├──────────────────────────────────────────────────────────────┤{RESET}");

    let mut sorted = findings.to_vec();
    sorted.sort_by(|a, b| sast_severity_level(&b.severity).cmp(&sast_severity_level(&a.severity)));

    for (i, f) in sorted.iter().enumerate() {
        let col = sast_severity_color(&f.severity);
        let sev_upper = f.severity.to_uppercase();
        println!("{BOLD}│{RESET}");
        println!("{BOLD}│{RESET}  {BOLD}#{}{RESET}  {col}[{sev_upper}]{RESET}  {BOLD}{}{RESET}", i + 1, f.title);
        if !f.url.is_empty() {
            println!("{BOLD}│{RESET}      {DIM}{} {}{RESET}", f.method, f.url);
        }
        if !f.parameter.is_empty() {
            println!("{BOLD}│{RESET}      {DIM}param: {}{RESET}", f.parameter);
        }
        for line in wrap_text(&f.description, 68) {
            println!("{BOLD}│{RESET}      {line}");
        }
        if !f.evidence.is_empty() {
            let ev: String = f.evidence.chars().take(72).collect();
            println!("{BOLD}│{RESET}      {DIM}evidence: {ev}{RESET}");
        }
        if !f.fix.is_empty() {
            println!("{BOLD}│{RESET}      {DIM}Fix: {}{RESET}", f.fix);
        }
    }
    println!("{BOLD}└──────────────────────────────────────────────────────────────┘{RESET}");
}

fn dast_to_sarif(findings: &[DastFinding], target: &str) -> serde_json::Value {
    let rules: Vec<serde_json::Value> = findings.iter().enumerate().map(|(i, f)| {
        serde_json::json!({
            "id": format!("STROBES-DAST-{:03}", i + 1),
            "shortDescription": { "text": f.title },
            "properties": { "severity": f.severity },
        })
    }).collect();

    let results: Vec<serde_json::Value> = findings.iter().enumerate().map(|(i, f)| {
        let level = match f.severity.to_lowercase().as_str() {
            "critical" | "high" => "error",
            "medium" => "warning",
            _ => "note",
        };
        serde_json::json!({
            "ruleId": format!("STROBES-DAST-{:03}", i + 1),
            "level": level,
            "message": { "text": format!("{}\n\n{}", f.title, f.description) },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": f.url }
                }
            }],
            "properties": {
                "severity": f.severity,
                "parameter": f.parameter,
                "method": f.method,
                "evidence": f.evidence,
            }
        })
    }).collect();

    serde_json::json!({
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "Strobes DAST",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": "https://strobes.co",
                    "rules": rules,
                }
            },
            "results": results,
            "automationDetails": { "id": format!("strobes/dast/{target}") },
        }]
    })
}

#[allow(clippy::too_many_arguments)]
async fn cmd_scan_dast(
    p: &config::Profile,
    url: String,
    output_fmt: String,
    output_file: Option<String>,
    custom_prompt: Option<String>,
    cookie: Option<String>,
    bearer: Option<String>,
    scope: Vec<String>,
    workspace: Option<String>,
    model: Option<i64>,
    timeout: u64,
    fail_on: Option<String>,
) -> Result<()> {
    require_complete(p)?;

    // Normalise URL — strip trailing slash for display, keep as-is for prompt.
    let target_url = url.trim_end_matches('/').to_string();
    let target_label = target_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string();

    // 1. Build prompt.
    let prompt = custom_prompt.unwrap_or_else(|| {
        build_dast_prompt(
            &target_url,
            cookie.as_deref(),
            bearer.as_deref(),
            &scope,
        )
    });

    // 2. Create workspace + thread.
    let client = api::ApiClient::new(p.clone())?;
    let workspace_id: Option<String> = match workspace {
        Some(ws) => Some(ws),
        None => {
            let (id, _) = client.create_workspace(&format!("DAST: {target_label}")).await?;
            eprintln!("workspace: {id}");
            Some(id)
        }
    };

    let thread_title = format!("DAST scan: {target_label}");
    let thread_id = client
        .create_thread(&thread_title, workspace_id.as_deref(), None)
        .await?;
    eprintln!("thread: {thread_id}");

    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let handle = pulse::connect(p, &thread_id, tx, model).await?;
    handle.send_user_message(&prompt);

    // 3. Live display — reuse ScanDisplay with "0 files" (no copy step for DAST).
    let mut display = ScanDisplay::new(&target_label, 0, 0, timeout);
    let start = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut render_tick = tokio::time::interval(std::time::Duration::from_millis(120));
    let mut full_text = String::new();

    display.render(start.elapsed());

    let run_result: Result<()> = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                display.erase();
                eprintln!("error: scan timed out after {timeout}s — use --timeout to extend");
                break Err(anyhow!("timed out after {timeout}s"));
            }
            _ = render_tick.tick() => {
                display.render(start.elapsed());
            }
            ev_opt = rx.recv() => {
                match ev_opt {
                    None => { display.erase(); break Ok(()); }
                    Some(ev) => match ev {
                        pulse::AppEvent::RunFinished(_) => { display.erase(); break Ok(()); }
                        pulse::AppEvent::Stream(item) => match item.kind.as_str() {
                            "token" => {
                                if let Some(text) = &item.text {
                                    full_text.push_str(text);
                                }
                            }
                            "tool_start" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let detail = item.detail.as_deref().unwrap_or("");
                                let msg = if detail.is_empty() {
                                    format!("▶ {name}")
                                } else {
                                    let d: String = detail.chars().take(48).collect();
                                    let ell = if detail.len() > 48 { "…" } else { "" };
                                    format!("▶ {name}({d}{ell})")
                                };
                                display.push(msg);
                                display.render(start.elapsed());
                            }
                            "tool_output" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let detail = item.detail.as_deref().unwrap_or("");
                                if !detail.is_empty() {
                                    let d: String = detail.chars().take(52).collect();
                                    let ell = if detail.len() > 52 { "…" } else { "" };
                                    display.push(format!("◀ {name}: {d}{ell}"));
                                    display.render(start.elapsed());
                                }
                            }
                            "tool_failed" => {
                                let name = item.tool_name.as_deref().unwrap_or("?");
                                let err = item.detail.as_deref().unwrap_or("error");
                                display.push(format!("✗ {name}: {err}"));
                                display.render(start.elapsed());
                            }
                            _ => {
                                if let Some(text) = &item.text {
                                    full_text.push_str(text);
                                }
                            }
                        },
                        pulse::AppEvent::Error(e) => {
                            display.erase();
                            eprintln!("error: {e}");
                            break Err(anyhow!("agent error: {e}"));
                        }
                        pulse::AppEvent::Interrupt { .. } => {
                            display.erase();
                            eprintln!("error: agent requested input — run interactively for complex scans");
                            break Err(anyhow!("agent requested human input during non-interactive scan"));
                        }
                        _ => {}
                    },
                }
            }
        }
    };

    run_result?;
    let elapsed = start.elapsed();

    // 4. Parse findings.
    let mut findings = parse_dast_output(&full_text);
    findings.sort_by(|a, b| sast_severity_level(&b.severity).cmp(&sast_severity_level(&a.severity)));

    // 5. Print summary.
    print_dast_summary(&findings, &target_url, elapsed);

    // 6. Structured output.
    let output_content = match output_fmt.as_str() {
        "json" => serde_json::to_string_pretty(&serde_json::json!({
            "scan_target": target_url,
            "workspace_id": workspace_id,
            "thread_id": thread_id,
            "elapsed_secs": elapsed.as_secs(),
            "findings": findings,
        }))?,
        "sarif" => serde_json::to_string_pretty(&dast_to_sarif(&findings, &target_label))?,
        _ => String::new(),
    };

    if !output_content.is_empty() {
        if let Some(ref path) = output_file {
            std::fs::write(path, &output_content)?;
            eprintln!("results saved → {path}");
        } else {
            println!("{output_content}");
        }
    } else if let Some(ref path) = output_file {
        std::fs::write(path, &full_text)?;
        eprintln!("raw output saved → {path}");
    }

    // 7. Severity gate.
    if let Some(threshold) = &fail_on {
        let level = sast_severity_level(threshold);
        let blocking: Vec<_> = findings
            .iter()
            .filter(|f| sast_severity_level(&f.severity) >= level)
            .collect();
        if !blocking.is_empty() {
            return Err(anyhow!(
                "{} finding(s) at or above '{}' severity",
                blocking.len(),
                threshold
            ));
        }
    }

    Ok(())
}

/// Emit findings as a SARIF 2.1.0 JSON string for GitHub Code Scanning.
fn findings_to_sarif(findings: &[api::Finding], workspace_id: &str) -> Result<String> {
    let rules: Vec<serde_json::Value> = findings.iter().map(|f| {
        serde_json::json!({
            "id": format!("STROBES-{}", f.id),
            "shortDescription": { "text": f.title },
            "properties": { "severity": f.severity_label },
        })
    }).collect();

    let results: Vec<serde_json::Value> = findings.iter().map(|f| {
        let level = match f.severity_label.to_lowercase().as_str() {
            "critical" | "high" => "error",
            "medium" => "warning",
            "low" => "note",
            _ => "none",
        };
        let body = if f.description.is_empty() {
            f.title.clone()
        } else {
            format!("{}\n\n{}", f.title, f.description)
        };
        serde_json::json!({
            "ruleId": format!("STROBES-{}", f.id),
            "level": level,
            "message": { "text": body },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": {
                        "uri": f.asset.as_deref().unwrap_or("unknown"),
                        "uriBaseId": "%SRCROOT%",
                    }
                }
            }],
            "properties": {
                "severity": f.severity_label,
                "state": f.state_label,
                "cvss": f.cvss,
            }
        })
    }).collect();

    let sarif = serde_json::json!({
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "Strobes Security Assessment",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": "https://strobes.co",
                    "rules": rules,
                }
            },
            "results": results,
            "automationDetails": {
                "id": format!("strobes/workspace/{workspace_id}"),
            }
        }]
    });

    Ok(serde_json::to_string_pretty(&sarif)?)
}

/// List or export workspace findings (text / JSON / SARIF).
async fn cmd_findings(
    p: &config::Profile,
    workspace: Option<String>,
    format: &str,
    fail_on: Option<String>,
) -> Result<()> {
    require_complete(p)?;
    let ws = workspace
        .or_else(|| p.workspace_id.clone())
        .ok_or_else(|| anyhow!("no workspace — pass --workspace <UUID> or run `strobes bind` first"))?;
    let client = api::ApiClient::new(p.clone())?;
    let findings = client.list_workspace_findings(&ws).await?;

    match format {
        "json" => {
            let arr: Vec<serde_json::Value> = findings.iter().map(|f| serde_json::json!({
                "id": f.id,
                "title": f.title,
                "severity": f.severity_label,
                "state": f.state_label,
                "cvss": f.cvss,
                "asset": f.asset,
                "description": f.description,
                "mitigation": f.mitigation,
            })).collect();
            println!("{}", serde_json::to_string_pretty(&serde_json::Value::Array(arr))?);
        }
        "sarif" => {
            println!("{}", findings_to_sarif(&findings, &ws)?);
        }
        _ => {
            if findings.is_empty() {
                println!("(no findings in workspace {}…)", &ws[..8.min(ws.len())]);
            } else {
                println!("{} finding(s) in workspace {}…:", findings.len(), &ws[..8.min(ws.len())]);
                for f in &findings {
                    let cvss = f.cvss.map(|c| format!(" CVSS:{c:.1}")).unwrap_or_default();
                    println!("  [{}] {}{} · {}", f.severity_label, f.title, cvss, f.state_label);
                }
            }
        }
    }

    if let Some(threshold) = &fail_on {
        let level = severity_level(threshold);
        let count = findings.iter().filter(|f| severity_level(&f.severity_label) >= level).count();
        if count > 0 {
            return Err(anyhow!("{count} finding(s) at or above '{threshold}' severity"));
        }
    }

    Ok(())
}

/// Export every thread transcript in a workspace to one local folder, as
/// standalone Markdown files (or raw persisted event JSON) plus an index.md.
async fn cmd_export(
    p: &config::Profile,
    workspace: Option<String>,
    thread: Option<String>,
    dir: Option<String>,
    format: &str,
    messages_only: bool,
) -> Result<()> {
    require_complete(p)?;
    if !matches!(format, "md" | "json") {
        return Err(anyhow!("unknown format '{format}' — use md or json"));
    }
    let ws = workspace
        .or_else(|| p.workspace_id.clone())
        .ok_or_else(|| anyhow!("no workspace — pass --workspace <UUID> or run `strobes bind` first"))?;
    let client = api::ApiClient::new(p.clone())?;

    let (threads_res, ws_res) = tokio::join!(client.list_threads(Some(&ws)), client.list_workspaces());
    let mut threads = threads_res?;
    if let Some(tid) = &thread {
        threads.retain(|t| &t.id == tid);
        if threads.is_empty() {
            return Err(anyhow!("thread {tid} not found in workspace {ws}"));
        }
    }
    if threads.is_empty() {
        println!("(no threads in workspace {}…)", &ws[..8.min(ws.len())]);
        return Ok(());
    }
    let ws_name = ws_res
        .unwrap_or_default()
        .into_iter()
        .find(|w| w.id == ws)
        .map(|w| w.name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("{}…", &ws[..8.min(ws.len())]));

    let out = std::path::PathBuf::from(
        dir.unwrap_or_else(|| format!("strobes-transcripts-{}", &ws[..8.min(ws.len())])),
    );
    std::fs::create_dir_all(&out)?;
    println!("exporting {} thread(s) from workspace {ws_name} → {}", threads.len(), out.display());

    let mut exported: Vec<(String, api::Thread, usize)> = Vec::new();
    let mut skipped = 0usize;
    for (i, t) in threads.iter().enumerate() {
        let events = fetch_all_thread_events(&client, &t.id).await?;
        // Prefer the full-fidelity event stream; fall back to the plain
        // message history for threads persisted before events existed.
        let (body, n) = if !events.is_empty() {
            let n = events.len();
            let body = match format {
                "json" => serde_json::to_string_pretty(&events)?,
                _ => transcript_md_from_events(t, &ws, &events, messages_only),
            };
            (body, n)
        } else {
            let hist = client.get_thread_history(&t.id, 1000).await.unwrap_or_default();
            if hist.messages.is_empty() {
                skipped += 1;
                println!("  – {} (empty, skipped)", thread_label(t));
                continue;
            }
            let n = hist.messages.len();
            let body = match format {
                "json" => serde_json::to_string_pretty(
                    &hist
                        .messages
                        .iter()
                        .map(|m| serde_json::json!({ "author": m.author, "text": m.text }))
                        .collect::<Vec<_>>(),
                )?,
                _ => transcript_md_from_messages(t, &ws, &hist.messages),
            };
            (body, n)
        };
        let file = format!(
            "{:03}-{}-{}.{format}",
            i + 1,
            slugify(&t.title),
            &t.id[..8.min(t.id.len())]
        );
        std::fs::write(out.join(&file), body)?;
        println!("  ✔ {file} ({n} events)");
        exported.push((file, t.clone(), n));
    }

    if !exported.is_empty() {
        let mut idx = format!("# Transcripts — {ws_name}\n\n- **Workspace:** `{ws}`\n\n");
        idx.push_str("| # | Thread | Status | Events | File |\n|--:|--------|--------|-------:|------|\n");
        for (i, (file, t, n)) in exported.iter().enumerate() {
            idx.push_str(&format!(
                "| {} | {} | {} | {n} | [`{file}`]({file}) |\n",
                i + 1,
                thread_label(t).replace('|', "\\|"),
                t.status,
            ));
        }
        std::fs::write(out.join("index.md"), idx)?;
    }

    let skipped_note = if skipped > 0 { format!(" ({skipped} empty skipped)") } else { String::new() };
    println!("✔ exported {} transcript(s) to {}{skipped_note}", exported.len(), out.display());
    Ok(())
}

/// Fetch a thread's complete persisted event stream, paging by `seq` until a
/// short page (or a page with no usable seq) signals the end.
async fn fetch_all_thread_events(
    client: &api::ApiClient,
    thread_id: &str,
) -> Result<Vec<serde_json::Value>> {
    const PAGE: u32 = 1000;
    let mut all: Vec<serde_json::Value> = Vec::new();
    let mut after = 0i64;
    loop {
        let batch = client.get_thread_events(thread_id, after, PAGE).await?;
        let full_page = batch.len() as u32 == PAGE;
        let last_seq = batch.iter().rev().find_map(|e| e.get("seq").and_then(|s| s.as_i64()));
        all.extend(batch);
        match last_seq {
            Some(seq) if full_page && seq > after => after = seq,
            _ => break,
        }
    }
    Ok(all)
}

/// "Title (id8…)" display label for a thread.
fn thread_label(t: &api::Thread) -> String {
    let title = if t.title.is_empty() { "(untitled)" } else { &t.title };
    format!("{title} ({}…)", &t.id[..8.min(t.id.len())])
}

/// Filesystem-safe slug from a thread title: lowercase alphanumerics joined by
/// single dashes, capped so full filenames stay comfortably short.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            pending_dash = false;
        } else {
            pending_dash = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    if out.is_empty() { "untitled".into() } else { out }
}

/// Shared `# title` + metadata header for exported transcripts.
fn transcript_md_header(t: &api::Thread, ws: &str) -> String {
    let title = if t.title.is_empty() { "(untitled)" } else { &t.title };
    let mut md = format!("# {title}\n\n");
    md.push_str(&format!("- **Thread:** `{}`\n", t.id));
    md.push_str(&format!("- **Workspace:** `{ws}`\n"));
    if !t.status.is_empty() {
        md.push_str(&format!("- **Status:** {}\n", t.status));
    }
    if let Some(c) = t.created_at.as_deref().filter(|c| !c.is_empty()) {
        md.push_str(&format!("- **Created:** {c}\n"));
    }
    md.push_str("\n---\n\n");
    md
}

/// Render the full-fidelity event stream as a standalone Markdown transcript:
/// user/agent messages as sections; thinking as blockquotes; tool calls, tool
/// output and task markers as indented code lines (mirrors the TUI transcript).
fn transcript_md_from_events(
    t: &api::Thread,
    ws: &str,
    events: &[serde_json::Value],
    messages_only: bool,
) -> String {
    let mut md = transcript_md_header(t, ws);
    // Indented tool/task lines form one Markdown code block per run; a blank
    // line is required before the first line of each run.
    let mut in_tools = false;
    fn tool_line(md: &mut String, in_tools: &mut bool, line: String) {
        if !*in_tools {
            md.push('\n');
            *in_tools = true;
        }
        md.push_str(&format!("    {line}\n"));
    }
    for e in events {
        let etype = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let p = e.get("payload").cloned().unwrap_or(serde_json::Value::Null);
        let pstr = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let actor = e.get("actor").and_then(|v| v.as_str()).unwrap_or("");
        let agent = e
            .get("agentName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Strobes AI Supervisor");
        match etype {
            "message.created" if actor == "user" => {
                let text = pstr("text");
                if !text.trim().is_empty() {
                    in_tools = false;
                    md.push_str(&format!("\n## 👤 User\n\n{}\n", text.trim()));
                }
            }
            "message.segment.completed" => {
                let text = pstr("text");
                if !text.trim().is_empty() {
                    in_tools = false;
                    md.push_str(&format!("\n## 🤖 {agent}\n\n{}\n", text.trim()));
                }
            }
            "thinking.completed" if !messages_only => {
                let text = pstr("text");
                if !text.trim().is_empty() {
                    in_tools = false;
                    md.push_str(&format!("\n> 💭 {}\n", text.trim().replace('\n', "\n> ")));
                }
            }
            "tool.start" if !messages_only => {
                let name = pstr("toolName");
                let args = app::compact_json(p.get("arguments"), 160);
                tool_line(&mut md, &mut in_tools, format!("🔧 {name} {args}"));
            }
            "tool.output" if !messages_only => {
                let res = app::compact_json(p.get("result"), 300);
                let body = if res.is_empty() { "(ok)".to_string() } else { res };
                tool_line(&mut md, &mut in_tools, format!("⎿ {body}"));
            }
            "tool.failed" if !messages_only => {
                tool_line(&mut md, &mut in_tools, format!("⎿ ✗ {}", pstr("error")));
            }
            "task.created" if !messages_only => {
                let title = pstr("title");
                if !title.trim().is_empty() {
                    tool_line(&mut md, &mut in_tools, format!("◇ task: {}", title.trim()));
                }
            }
            _ => {}
        }
    }
    md
}

/// Render the plain message history (author + text) as Markdown — the fallback
/// for threads that predate the persisted event stream.
fn transcript_md_from_messages(t: &api::Thread, ws: &str, messages: &[api::HistMsg]) -> String {
    let mut md = transcript_md_header(t, ws);
    for m in messages {
        if m.text.trim().is_empty() {
            continue;
        }
        let who = match m.author.as_str() {
            "user" => "👤 User".to_string(),
            "agent" => "🤖 agent".to_string(),
            "orchestrator" => "🤖 Strobes AI Supervisor".to_string(),
            other => format!("🤖 {other}"),
        };
        md.push_str(&format!("\n## {who}\n\n{}\n", m.text.trim()));
    }
    md
}

fn describe(ev: &pulse::AppEvent) -> String {
    use pulse::AppEvent::*;
    match ev {
        Connected => "connected".into(),
        Disconnected(w) => format!("disconnected: {w}"),
        RunStarted => "run started".into(),
        RunFinished(l) => format!("run finished: {l}"),
        Notice(n) => format!("notice: {n}"),
        Credits { credits, tokens, final_run } => format!("credits {credits:.4} tokens {tokens} final={final_run}"),
        Error(e) => format!("error: {e}"),
        LocalToolDone { name, ms, exit, err } => {
            format!("local tool {name} done {ms}ms exit={exit:?} err={err:?}")
        }
        Interrupt { id, title, fields, .. } => {
            format!("interrupt requested id={id} title={title:?} fields={}", fields.len())
        }
        Stream(it) => {
            let mut s = format!("event[{}]", it.kind);
            if let Some(n) = &it.tool_name { s.push_str(&format!(" tool={n}")); }
            if it.local { s.push_str(" LOCAL"); }
            if let Some(t) = &it.text { s.push_str(&format!(" text={:?}", trunc(t, 80))); }
            if let Some(d) = &it.detail { s.push_str(&format!(" detail={:?}", trunc(d, 80))); }
            s
        }
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() }
}

/// Read one line from stdin, showing the current value (redacted if secret) as
/// the default. An empty entry keeps the current value.
fn prompt_line(label: &str, current: &str, secret: bool) -> Result<String> {
    use std::io::Write;
    let hint = if current.is_empty() {
        String::new()
    } else if secret {
        format!(" [{}]", config::redact(current))
    } else {
        format!(" [{current}]")
    };
    print!("{label}{hint}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim().to_string();
    Ok(if line.is_empty() { current.to_string() } else { line })
}

/// Configure the active profile's credentials. Any field passed as a flag is
/// used verbatim; the rest are prompted for interactively.
async fn cmd_login(
    cfg: &mut Config,
    tenant: &str,
    base_url: Option<String>,
    org_id: Option<String>,
    master_key: Option<String>,
    deployment: Option<String>,
    no_verify: bool,
) -> Result<()> {
    let pname = tenant.to_string();
    let had_default = cfg.has_default();
    let cur = cfg.profile_mut(&pname).clone();
    let interactive = base_url.is_none() && org_id.is_none() && master_key.is_none();
    if interactive {
        println!("Configuring tenant '{pname}' — press Enter to keep the current value.");
    }

    let base_url = match base_url {
        Some(v) => v,
        None => prompt_line("Base URL (e.g. https://app.strobes.co)", &cur.base_url, false)?,
    };
    let org_id = match org_id {
        Some(v) => v,
        None => prompt_line("Org ID (UUID)", &cur.org_id, false)?,
    };
    let master_key = match master_key {
        Some(v) => v,
        None => prompt_line("Master key", &cur.master_key, true)?,
    };
    let deployment = match deployment {
        Some(v) => v,
        None if interactive => {
            prompt_line("Deployment (blank = proxy /api/v1, 'direct' = /v1)", &cur.deployment, false)?
        }
        None => cur.deployment.clone(),
    };

    {
        let p = cfg.profile_mut(&pname);
        p.base_url = base_url.trim().trim_end_matches('/').to_string();
        p.org_id = org_id.trim().to_string();
        p.master_key = master_key.trim().to_string();
        p.deployment = deployment.trim().to_string();
    }
    cfg.save()?;
    let path = config::config_dir().join("config.json");
    println!("\n✔ saved tenant '{pname}' → {}", path.display());

    let saved = cfg.profile_mut(&pname).clone();
    // The first tenant with usable credentials becomes the default.
    if saved.is_complete() && !had_default {
        cfg.current_profile = pname.clone();
        cfg.save()?;
        println!("★ '{pname}' is now the default tenant.");
    } else if saved.is_complete() && cfg.current_profile != pname {
        println!(
            "• default tenant remains '{}' — run any command with `--tenant {pname}` to use this one.",
            cfg.current_profile
        );
    }
    if std::env::var("STROBES_AI_BASE_URL").is_ok()
        || std::env::var("STROBES_AI_MASTER_KEY").is_ok()
        || std::env::var("STROBES_AI_ORG_ID").is_ok()
    {
        println!("⚠ STROBES_AI_* env vars are set and will override this file at runtime.");
    }
    if no_verify {
        return Ok(());
    }
    if !saved.is_complete() {
        println!("⚠ profile still incomplete (need base_url, org_id and master_key).");
        return Ok(());
    }
    use std::io::Write;
    print!("Testing connection… ");
    std::io::stdout().flush()?;
    match api::ApiClient::new(saved)?.ping().await {
        Ok(_) => println!("✔ connection OK"),
        Err(e) => println!("✗ connection failed: {e}"),
    }
    Ok(())
}

/// List configured tenants, marking the default with ★.
fn cmd_tenants(cfg: &Config) -> Result<()> {
    let names = cfg.tenants();
    if names.is_empty() {
        println!("no tenants configured — run `strobes --tenant <name> login`");
        return Ok(());
    }
    for name in &names {
        let p = cfg.profile_for(name);
        let mark = if *name == cfg.current_profile { "★" } else { " " };
        println!("{mark} {name}\t{}\t{}", p.org_id, p.base_url);
    }
    println!("\n★ = default tenant. Override per-run with `--tenant <name>`.");
    Ok(())
}

async fn cmd_status(p: &config::Profile) -> Result<()> {
    println!("base_url    {}", if p.base_url.is_empty() { "(unset)" } else { &p.base_url });
    println!("org_id      {}", if p.org_id.is_empty() { "(unset)" } else { &p.org_id });
    println!("master_key  {}", config::redact(&p.master_key));
    println!("deployment  {}", p.deployment);
    if let (Some(ip), Some(host)) = (p.resolve_override(), p.host()) {
        println!("resolve     {host} → {ip} (DNS bypass)");
    }
    println!("workspace   {}", p.workspace_id.clone().unwrap_or_else(|| "(none)".into()));
    println!("thread      {}", p.thread_id.clone().unwrap_or_else(|| "(none)".into()));
    if p.is_complete() {
        let client = api::ApiClient::new(p.clone())?;
        let (ping, latest) = tokio::join!(client.ping(), latest_release_version());
        match ping {
            Ok(_) => println!("\n✔ connection OK"),
            Err(e) => println!("\n✗ connection failed: {e}"),
        }
        match latest {
            Some(v) if version_is_newer(&v, env!("CARGO_PKG_VERSION")) => {
                println!("⬆ update available: v{v} (current v{}) — run `strobes update`", env!("CARGO_PKG_VERSION"));
            }
            _ => println!("✔ up to date (v{})", env!("CARGO_PKG_VERSION")),
        }
    }
    Ok(())
}

/// Compact token count (e.g. 1.8k, 2.5M).
fn fmt_tok(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Map of workspace id → lifetime credits consumed (one credits API call).
async fn workspace_credits_map(client: &api::ApiClient) -> std::collections::HashMap<String, f64> {
    client
        .get_credits(None, None)
        .await
        .map(|s| s.by_workspace.into_iter().map(|w| (w.workspace_id, w.credits)).collect())
        .unwrap_or_default()
}

async fn cmd_credits(
    p: &config::Profile,
    workspace: Option<String>,
    thread: Option<String>,
) -> Result<()> {
    require_complete(p)?;
    let client = api::ApiClient::new(p.clone())?;
    let sum = client.get_credits(workspace.as_deref(), thread.as_deref()).await?;

    let scope = match (&workspace, &thread) {
        (_, Some(t)) => format!("thread {}", &t[..8.min(t.len())]),
        (Some(w), _) => format!("workspace {}", &w[..8.min(w.len())]),
        _ => "organization".into(),
    };
    println!(
        "◈ {scope}:  {:.3} credits · {} tokens · {} runs",
        sum.credits,
        fmt_tok(sum.tokens),
        sum.runs
    );

    if !sum.by_workspace.is_empty() {
        let names: std::collections::HashMap<String, String> = client
            .list_workspaces()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|w| (w.id, w.name))
            .collect();
        println!("\nby workspace:");
        for w in &sum.by_workspace {
            let name = match names.get(&w.workspace_id) {
                Some(n) if !n.is_empty() => n.clone(),
                Some(_) => "(unnamed)".into(),
                None => "(deleted)".into(),
            };
            println!(
                "  {:>8.3} cr · {:>7} tok · {:>3} runs   {}",
                w.credits,
                fmt_tok(w.tokens),
                w.runs,
                name
            );
        }
    }
    Ok(())
}

async fn cmd_workspaces(p: &config::Profile) -> Result<()> {
    require_complete(p)?;
    let client = api::ApiClient::new(p.clone())?;
    let rows = client.list_workspaces().await?;
    if rows.is_empty() {
        println!("(no workspaces)");
    }
    let (counts, credits) =
        tokio::join!(workspace_thread_counts(&client, &rows), workspace_credits_map(&client));
    for (i, w) in rows.iter().enumerate() {
        let bound = if Some(&w.id) == p.workspace_id.as_ref() { " ●" } else { "" };
        let tcount = match counts.get(i).copied().flatten() {
            Some(n) => format!("{n} threads"),
            None => "? threads".into(),
        };
        let cr = credits.get(&w.id).copied().unwrap_or(0.0);
        println!("{}  {}{}  [{}]  {}  · ◈ {cr:.2} cr", w.id, w.name, bound, w.status, tcount);
    }
    Ok(())
}

async fn cmd_threads(p: &config::Profile) -> Result<()> {
    require_complete(p)?;
    let rows = api::ApiClient::new(p.clone())?.list_threads(None).await?;
    if rows.is_empty() {
        println!("(no threads)");
    }
    for t in rows {
        let title = if t.title.is_empty() { "(untitled)" } else { &t.title };
        println!("{}  {}  [{}]  {}", t.id, title, t.status, t.last_message);
    }
    Ok(())
}

fn require_complete(p: &config::Profile) -> Result<()> {
    if !p.is_complete() {
        return Err(anyhow!("profile incomplete — run `strobes login` first"));
    }
    Ok(())
}

/// What a keypress deferred to after the select block (these need `.await`
/// and/or to reassign `rx`, which can't happen while select borrows it).
enum Defer {
    Workspaces,
    Threads,
    Findings,
    Approvals,
    Files,
    OpenFiles,
    UploadFiles(Vec<std::path::PathBuf>),
    SwitchThread(String),
    BindWorkspace(String),
    Models,
}

/// If the input is one or more existing local file paths (e.g. a file dragged
/// onto the terminal, which inserts its path), return them — so a plain run can
/// upload instead of sending the path as a chat message. Returns None for
/// normal text. Handles `~`, quotes and backslash-escaped spaces.
fn parse_dragged_paths(input: &str) -> Option<Vec<std::path::PathBuf>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Tokenize respecting quotes and backslash escapes (how terminals quote
    // dropped paths with spaces).
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let (mut sq, mut dq) = (false, false);
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if !sq => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' if !dq => sq = !sq,
            '"' if !sq => dq = !dq,
            c if c.is_whitespace() && !sq && !dq => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    if tokens.is_empty() {
        return None;
    }
    let mut paths = Vec::new();
    for t in tokens {
        // Only treat path-like tokens (with a separator or ~) as drops, so a
        // normal word/bare filename isn't mistaken for an upload.
        let pathy = t.contains('/') || t.contains('\\') || t.starts_with('~');
        if !pathy {
            return None;
        }
        let p = if let Some(rest) = t.strip_prefix("~/") {
            dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| std::path::PathBuf::from(&t))
        } else {
            std::path::PathBuf::from(&t)
        };
        if !p.is_file() {
            return None;
        }
        paths.push(p);
    }
    Some(paths)
}

/// Load a thread's history, active-run state and title into the app, running
/// the independent round-trips concurrently. Used when switching threads.
async fn load_thread_data(
    client: &api::ApiClient,
    thread_id: &str,
    ws_scope: Option<&str>,
    app: &mut App,
) {
    let (events_res, threads_res, run_res, credits_res) = tokio::join!(
        client.get_thread_events(thread_id, 0, 2000),
        client.list_threads(ws_scope),
        client.get_thread_history(thread_id, 1),
        client.get_credits(None, Some(thread_id)),
    );
    if let Ok(c) = credits_res {
        app.set_thread_credits(c.credits, c.tokens);
    }
    match events_res {
        Ok(events) if !events.is_empty() => app.seed_history_events(events),
        _ => {
            if let Ok(hist) = client.get_thread_history(thread_id, 100).await {
                app.seed_history(hist.messages);
            }
        }
    }
    if let Ok(hist) = run_res {
        if let Some(run) = hist.active_run {
            app.note_active_run(&run.status);
        }
    }
    if let Ok(threads) = threads_res {
        if let Some(t) = threads.into_iter().find(|t| t.id == thread_id) {
            if !t.title.is_empty() {
                app.set_title(t.title);
            }
        }
    }
}

pub async fn run_chat(
    terminal: &mut ratatui::DefaultTerminal,
    tenant: &str,
    profile: config::Profile,
    thread_id: String,
    model: Option<i64>,
    initial_msg: Option<String>,
) -> Result<()> {
    require_complete(&profile)?;

    let mut profile = profile;
    let client = api::ApiClient::new(profile.clone())?;
    let mut app = App::new(thread_id.clone(), profile.base_url.clone(), tenant.to_string(), profile.org_id.clone());
    app.has_workspace = profile.workspace_id.is_some();
    if let Some(m) = model {
        app.set_model(Some(m));
    }

    // Fire the independent startup round-trips (history, title, slash-commands,
    // active-run, and the WebSocket connect) concurrently so the UI appears in
    // ~one round-trip instead of four-plus sequential ones.
    let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
    let mut app_tx = tx.clone(); // for background tasks (workspace sync) to post UI events
    let ws_scope = profile.workspace_id.clone();
    let (conn, events_res, threads_res, cmds_res, run_res, ws_res, credits_res) = tokio::join!(
        pulse::connect(&profile, &thread_id, tx, model),
        client.get_thread_events(&thread_id, 0, 2000),
        client.list_threads(ws_scope.as_deref()),
        client.list_slash_commands(),
        client.get_thread_history(&thread_id, 1),
        client.list_workspaces(),
        client.get_credits(None, Some(&thread_id)),
    );
    let mut handle = conn?;
    // Resolve the bound workspace's name for the top-right indicator.
    if let (Some(wid), Ok(wss)) = (ws_scope.as_deref(), &ws_res) {
        if let Some(w) = wss.iter().find(|w| w.id == wid) {
            app.set_workspace_name(w.name.clone());
        }
    }
    // Seed this thread's lifetime credit usage.
    if let Ok(c) = credits_res {
        app.set_thread_credits(c.credits, c.tokens);
    }

    // Apply the fetched data (in-memory, fast).
    match events_res {
        Ok(events) if !events.is_empty() => app.seed_history_events(events),
        _ => {
            if let Ok(hist) = client.get_thread_history(&thread_id, 100).await {
                app.seed_history(hist.messages);
            }
        }
    }
    if let Ok(hist) = run_res {
        if let Some(run) = hist.active_run {
            app.note_active_run(&run.status);
        }
    }
    if let Ok(threads) = threads_res {
        if let Some(t) = threads.into_iter().find(|t| t.id == thread_id) {
            if !t.title.is_empty() {
                app.set_title(t.title);
            }
        }
    }
    if let Ok(cmds) = cmds_res {
        app.set_slash_commands(cmds);
    }

    if let Some(ws) = profile.workspace_id.clone() {
        spawn_workspace_sync(profile.clone(), ws, app_tx.clone());
    }

    // Background: suggest an update if a newer release is published.
    {
        let tx = app_tx.clone();
        tokio::spawn(async move {
            if let Some(latest) = latest_release_version().await {
                if version_is_newer(&latest, env!("CARGO_PKG_VERSION")) {
                    let _ = tx.send(pulse::AppEvent::Notice(format!(
                        "⬆ update available: v{latest} (you have v{}) — run `strobes update`",
                        env!("CARGO_PKG_VERSION")
                    )));
                }
            }
        });
    }

    // Auto-send the setup prompt for a freshly-created workspace.
    if let Some(msg) = initial_msg {
        let msg = msg.trim().to_string();
        if !msg.is_empty() {
            app.echo_user(&msg);
            handle.send_user_message(&msg);
            app.running = true;
            app.status = "sending…".into();
        }
    }

    let mut events = EventStream::new();
    let mut viewport_h: u16 = 20;
    // Drives the working-spinner animation while a turn is running.
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(120));

    let res = loop {
        terminal.draw(|f| {
            viewport_h = f.area().height.saturating_sub(7).max(1);
            app.draw(f);
        })?;

        let mut defer: Option<Defer> = None;
        let mut quit = false;

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                        if app.overlay_active() {
                            // ----- overlay navigation -----
                            match k.code {
                                KeyCode::Esc => {
                                    // Back-stack: detail → list, chat → threads →
                                    // workspaces → exit. Findings/approvals just
                                    // close back to chat.
                                    if app.overlay_detail_open() {
                                        app.overlay_esc();
                                    } else if app.overlay_kind() == Some(app::OverlayKind::Threads) {
                                        defer = Some(Defer::Workspaces);
                                    } else if app.overlay_kind() == Some(app::OverlayKind::Workspaces) {
                                        quit = true;
                                    } else {
                                        app.overlay_esc();
                                    }
                                }
                                KeyCode::Up => app.overlay_move(true),
                                KeyCode::Down => app.overlay_move(false),
                                KeyCode::PageUp => app.overlay_page(true, viewport_h),
                                KeyCode::PageDown => app.overlay_page(false, viewport_h),
                                KeyCode::Enter => {
                                    match app.overlay_enter() {
                                        Some((app::OverlayKind::Workspaces, id)) => defer = Some(Defer::BindWorkspace(id)),
                                        Some((app::OverlayKind::Threads, id)) => defer = Some(Defer::SwitchThread(id)),
                                        Some((app::OverlayKind::Models, id)) => {
                                            let model_id = match id.parse::<i64>().ok() {
                                                Some(0) => None,
                                                n => n,
                                            };
                                            handle.set_model(model_id);
                                            app.set_model(model_id);
                                            let name = api::model_name(model_id);
                                            app.notice(&format!("⚙ model → {name}"));
                                            app.close_overlay();
                                        }
                                        _ => {}
                                    }
                                }
                                // ^C exits the app from any picker/overlay (cancels
                                // an in-flight run first if one is active).
                                KeyCode::Char('c') if ctrl => { if app.running { handle.cancel(); } else { quit = true; } }
                                // ^P toggles the model picker even while another overlay is shown.
                                KeyCode::Char('p') if ctrl => {
                                    if app.overlay_kind() == Some(app::OverlayKind::Models) {
                                        app.close_overlay();
                                    } else {
                                        defer = Some(Defer::Models);
                                    }
                                }
                                // Type-to-search in the workspace/thread/model pickers.
                                KeyCode::Backspace if app.overlay_searchable() => app.overlay_filter_pop(),
                                KeyCode::Char(c) if !ctrl && app.overlay_searchable() => app.overlay_filter_push(c),
                                // vim-style j/k nav only where there's no search.
                                KeyCode::Char('k') if !ctrl => app.overlay_move(true),
                                KeyCode::Char('j') if !ctrl => app.overlay_move(false),
                                _ => {}
                            }
                        } else if app.awaiting_input() {
                            match k.code {
                                KeyCode::Esc => quit = true,
                                KeyCode::Char(c) if !ctrl => app.input_insert_char(c),
                                KeyCode::Backspace => app.input_backspace(),
                                KeyCode::Left => app.input_left(),
                                KeyCode::Right => app.input_right(),
                                KeyCode::Home => app.input_home(),
                                KeyCode::End => app.input_end(),
                                KeyCode::Enter => {
                                    let raw = app.input.clone();
                                    app.input_clear();
                                    if let Some((id, data)) = app.submit_interrupt_value(raw.trim()) {
                                        handle.respond_interrupt(&id, data);
                                    }
                                }
                                _ => {}
                            }
                        } else if app.slash_open()
                            && matches!(k.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Enter)
                            && !ctrl
                        {
                            // ----- slash-command autocomplete popup -----
                            match k.code {
                                KeyCode::Up => app.slash_move(true),
                                KeyCode::Down => app.slash_move(false),
                                KeyCode::Tab | KeyCode::Enter => app.slash_complete(),
                                _ => {}
                            }
                        } else {
                            // ----- normal chat -----
                            match k.code {
                                // Esc: if scrolled up, snap back to the live
                                // bottom first; otherwise step back into
                                // navigation (threads → workspaces). ^C quits.
                                KeyCode::Esc => {
                                    if app.is_pinned() {
                                        app.jump_to_bottom();
                                    } else {
                                        defer = Some(Defer::Threads);
                                    }
                                }
                                KeyCode::Char('c') if ctrl => { if app.running { handle.cancel(); } else { quit = true; } }
                                KeyCode::Char('t') if ctrl => app.show_thinking = !app.show_thinking,
                                KeyCode::Char('r') if ctrl => app.markdown = !app.markdown,
                                KeyCode::Char('s') if ctrl => {
                                    app.select_mode = !app.select_mode;
                                    if app.select_mode { disable_mouse(); } else { enable_mouse(); }
                                }
                                KeyCode::Char('w') if ctrl => defer = Some(Defer::Workspaces),
                                KeyCode::Char('o') if ctrl => defer = Some(Defer::Threads),
                                KeyCode::Char('p') if ctrl => defer = Some(Defer::Models),
                                KeyCode::Char('f') if ctrl => defer = Some(Defer::Findings),
                                KeyCode::Char('a') if ctrl => defer = Some(Defer::Approvals),
                                KeyCode::Char('l') if ctrl => defer = Some(Defer::Files),
                                KeyCode::Char('e') if ctrl => defer = Some(Defer::OpenFiles),
                                KeyCode::Char('y') if ctrl => {
                                    let text = app.transcript_plaintext();
                                    match copy_to_clipboard(&text) {
                                        Ok(_) => app.notice("copied transcript to clipboard"),
                                        Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("copy failed: {e}"))),
                                    }
                                }
                                // Newline: Shift/Alt+Enter (needs kitty protocol)
                                // or Ctrl+J (works on every terminal). Plain
                                // Enter sends.
                                KeyCode::Char('j') if ctrl => app.input_newline(),
                                KeyCode::Enter if k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => {
                                    app.input_newline();
                                }
                                KeyCode::Enter => {
                                    let text = app.input.trim().to_string();
                                    if !text.is_empty() {
                                        // Dragged local file path(s) + a bound
                                        // workspace → upload instead of sending.
                                        match parse_dragged_paths(&text) {
                                            Some(paths) if app.has_workspace => {
                                                defer = Some(Defer::UploadFiles(paths));
                                                app.input_clear();
                                            }
                                            _ => {
                                                app.echo_user(&text);
                                                handle.send_user_message(&text);
                                                app.input_clear();
                                                app.running = true;
                                                app.status = "sending…".into();
                                            }
                                        }
                                    }
                                }
                                KeyCode::Backspace => app.input_backspace(),
                                KeyCode::Delete => app.input_delete(),
                                KeyCode::Left => app.input_left(),
                                KeyCode::Right => app.input_right(),
                                KeyCode::Home => app.input_home(),
                                KeyCode::End => app.input_end(),
                                KeyCode::PageUp => app.page(true, viewport_h),
                                KeyCode::PageDown => app.page(false, viewport_h),
                                // Up/Down move the cursor within a multiline
                                // message; at the top/bottom edge they scroll the
                                // transcript instead.
                                KeyCode::Up => { if !app.input_up() { app.scroll_line(true); } }
                                KeyCode::Down => { if !app.input_down() { app.scroll_line(false); } }
                                KeyCode::Char(c) if !ctrl => app.input_insert_char(c),
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Event::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => if app.overlay_active() { app.overlay_move(true) } else { app.scroll_lines(true, 3) },
                        MouseEventKind::ScrollDown => if app.overlay_active() { app.overlay_move(false) } else { app.scroll_lines(false, 3) },
                        _ => {}
                    },
                    // A transient parse error (e.g. a stray escape byte) must NOT
                    // kill the session — just ignore it. Only a closed stream quits.
                    Some(Err(_)) => {}
                    None => quit = true,
                    _ => {}
                }
            }
            maybe_app = rx.recv() => {
                match maybe_app {
                    Some(ev) => {
                        let finished = matches!(ev, pulse::AppEvent::RunFinished(_));
                        app.on_app_event(ev);
                        // Notify the (possibly away) user that the reply is ready.
                        if finished {
                            if let Some(secs) = app.last_run_secs() {
                                notify_response_done(secs);
                            }
                        }
                    }
                    None => quit = true,
                }
            }
            _ = ticker.tick() => {
                // Advance the spinner only while running; ratatui's diff means
                // an idle redraw writes nothing.
                if app.running {
                    app.tick();
                }
            }
        }

        if quit {
            break Ok(());
        }

        // Deferred (awaiting / reconnecting) actions happen here, after the
        // select block has released its borrow on `rx`.
        match defer {
            Some(Defer::Workspaces) => match workspace_overlay_items(&client, &profile).await {
                Ok(items) => app.open_overlay(app::OverlayKind::Workspaces, "Workspaces".into(), items),
                Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
            },
            Some(Defer::Threads) => {
                let in_ws = profile.workspace_id.is_some();
                match client.list_threads(profile.workspace_id.as_deref()).await {
                    Ok(ts) => {
                        let title = if in_ws { "Threads (this workspace)" } else { "Threads" };
                        app.open_overlay(app::OverlayKind::Threads, title.into(), thread_items(ts, &thread_id, in_ws));
                    }
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                }
            }
            Some(Defer::Findings) => match profile.workspace_id.clone() {
                Some(ws) => match client.list_workspace_findings(&ws).await {
                    Ok(fs) => app.open_overlay(app::OverlayKind::Findings, "Findings".into(), finding_items(fs)),
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
                None => match workspace_overlay_items(&client, &profile).await {
                    Ok(items) => {
                        app.notice("pick a workspace, then press ^F for its findings");
                        app.open_overlay(app::OverlayKind::Workspaces, "Workspaces".into(), items);
                    }
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
            },
            Some(Defer::Approvals) => match profile.workspace_id.clone() {
                Some(ws) => match client.list_workspace_approvals(&ws).await {
                    Ok(aps) => app.open_overlay(app::OverlayKind::Approvals, "Approvals".into(), approval_items(aps)),
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
                None => match workspace_overlay_items(&client, &profile).await {
                    Ok(items) => {
                        app.notice("pick a workspace, then press ^A for its approvals");
                        app.open_overlay(app::OverlayKind::Workspaces, "Workspaces".into(), items);
                    }
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(e.to_string())),
                },
            },
            Some(Defer::Files) => {
                let dir = local::sandbox_dir();
                let items = file_items(&dir);
                if items.is_empty() {
                    app.notice(&format!(
                        "no local workspace files yet ({}) — bind a workspace to sync",
                        dir.display()
                    ));
                } else {
                    let title = format!("Files — {} ({} items)", dir.display(), items.len());
                    app.open_overlay(app::OverlayKind::Files, title, items);
                }
            }
            Some(Defer::Models) => {
                let models: Vec<(i64, String)> = api::BUILTIN_MODELS
                    .iter()
                    .map(|&(id, name)| (id, name.to_string()))
                    .collect();
                let current = app.current_model();
                let items: Vec<app::OverlayItem> = models
                    .into_iter()
                    .map(|(id, name)| {
                        let mark = if Some(id) == current || (current.is_none() && id == 0) {
                            " ●"
                        } else {
                            ""
                        };
                        let label = format!("{name}{mark}");
                        let detail = vec![
                            name.clone(),
                            format!("model id: {id}"),
                            String::new(),
                            "Press Enter to use this model for the current chat.".into(),
                            "Takes effect on the next message you send.".into(),
                        ];
                        app::OverlayItem { label, detail, action: Some(id.to_string()) }
                    })
                    .collect();
                app.open_overlay(app::OverlayKind::Models, "Select AI model  (^P toggle)".into(), items);
            }
            Some(Defer::OpenFiles) => {
                let dir = local::sandbox_dir();
                match open_in_file_manager(&dir) {
                    Ok(_) => app.notice(&format!("opened {} in your file manager", dir.display())),
                    Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("open failed: {e}"))),
                }
            }
            Some(Defer::UploadFiles(paths)) => match profile.workspace_id.clone() {
                Some(ws) => {
                    // Mirror dropped files into the live workspace sandbox (what
                    // the agent reads) and any bound folder, so local + remote
                    // stay in sync.
                    let sync_roots = workspace_sync_roots(&Config::load(), &ws);
                    let mut ok = 0usize;
                    for path in &paths {
                        let p = path.to_string_lossy().to_string();
                        match upload_one(&client, &ws, &p, "", &sync_roots).await {
                            Ok(dest) => {
                                ok += 1;
                                app.notice(&format!("⬆ uploaded {dest}"));
                            }
                            Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("upload failed: {e}"))),
                        }
                    }
                    if ok > 0 {
                        let where_to = match sync_roots.first() {
                            Some(r) => format!(" (synced to {})", r.display()),
                            None => String::new(),
                        };
                        app.notice(&format!("✔ {ok} file(s) uploaded to the workspace{where_to}"));
                    }
                }
                None => app.notice("bind a workspace first, then drop files to upload"),
            },
            Some(Defer::BindWorkspace(sel)) if sel == "__new_ws__" => {
                // Create a workspace: ask for a name + optional setup prompt,
                // then jump into its setup chat (sending the prompt).
                let auth = auth_line(&profile, tenant);
                let name = match picker::prompt_text(terminal, "New workspace — name", "", &auth).await {
                    Ok(Some(n)) => {
                        let n = n.trim().to_string();
                        if n.is_empty() { "CLI Workspace".to_string() } else { n }
                    }
                    _ => {
                        app.close_overlay();
                        String::new()
                    }
                };
                if !name.is_empty() {
                    let prompt = picker::prompt_text(
                        terminal,
                        "Setup prompt — what should this workspace do? (optional)",
                        "",
                        &auth,
                    )
                    .await
                    .ok()
                    .flatten()
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty());

                    match client.create_workspace(&name).await {
                        Ok((new_id, setup)) => {
                            profile.workspace_id = Some(new_id.clone());
                            handle.set_workspace(Some(new_id.clone()));
                            app.has_workspace = true;
                            app.set_workspace_name(name.clone());
                            {
                                let mut c = Config::load();
                                c.record_workspace_open(&new_id);
                                let _ = c.save();
                            }
                            spawn_workspace_sync(profile.clone(), new_id.clone(), app_tx.clone());
                            app.notice(&format!("✔ created workspace {name} — opening setup chat…"));
                            match setup {
                                Some(setup_tid) => {
                                    // Switch the live connection to the setup thread.
                                    let (ntx, nrx) = mpsc::unbounded_channel::<pulse::AppEvent>();
                                    let nclone = ntx.clone();
                                    match pulse::connect(&profile, &setup_tid, ntx, model).await {
                                        Ok(nh) => {
                                            handle = nh;
                                            rx = nrx;
                                            app_tx = nclone;
                                            app.reset_for_thread(setup_tid.clone());
                                            load_thread_data(&client, &setup_tid, profile.workspace_id.as_deref(), &mut app).await;
                                            app.set_workspace_name(name.clone());
                                            if let Some(p) = &prompt {
                                                app.echo_user(p);
                                                handle.send_user_message(p);
                                                app.running = true;
                                                app.status = "sending…".into();
                                            }
                                        }
                                        Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("open setup chat failed: {e}"))),
                                    }
                                }
                                None => app.close_overlay(),
                            }
                        }
                        Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("create workspace failed: {e}"))),
                    }
                }
            }
            Some(Defer::BindWorkspace(id)) => {
                profile.workspace_id = Some(id.clone());
                handle.set_workspace(Some(id.clone()));
                app.has_workspace = true;
                // Count this open locally for "recent" ranking next time.
                {
                    let mut c = Config::load();
                    c.record_workspace_open(&id);
                    let _ = c.save();
                }
                if let Ok(wss) = client.list_workspaces().await {
                    if let Some(w) = wss.iter().find(|w| w.id == id) {
                        app.set_workspace_name(w.name.clone());
                    }
                }
                app.notice(&format!("✔ bound workspace {} — ^F findings · ^A approvals", &id[..8.min(id.len())]));
                // Sync the workspace files locally IN THE BACKGROUND so the
                // agent's local tools see the real files without freezing the UI.
                spawn_workspace_sync(profile.clone(), id.clone(), app_tx.clone());
                // Lead straight into choosing a new chat or an existing thread
                // for this workspace.
                match client.list_threads(Some(&id)).await {
                    Ok(ts) => app.open_overlay(
                        app::OverlayKind::Threads,
                        "New chat or existing thread".into(),
                        thread_items(ts, &thread_id, true),
                    ),
                    Err(_) => app.close_overlay(),
                }
            }
            Some(Defer::SwitchThread(sel)) => {
                // "__new__" means create a fresh thread (in the bound workspace).
                let target = if sel == "__new__" {
                    match client.create_thread("CLI session", profile.workspace_id.as_deref(), None).await {
                        Ok(id) => Some(id),
                        Err(e) => {
                            app.on_app_event(pulse::AppEvent::Error(format!("new chat failed: {e}")));
                            None
                        }
                    }
                } else {
                    Some(sel)
                };
                if let Some(new_thread) = target {
                    let (ntx, nrx) = mpsc::unbounded_channel::<pulse::AppEvent>();
                    let nclone = ntx.clone();
                    match pulse::connect(&profile, &new_thread, ntx, model).await {
                        Ok(nh) => {
                            handle = nh;
                            rx = nrx;
                            app_tx = nclone;
                            app.reset_for_thread(new_thread.clone());
                            load_thread_data(&client, &new_thread, profile.workspace_id.as_deref(), &mut app).await;
                        }
                        Err(e) => app.on_app_event(pulse::AppEvent::Error(format!("switch failed: {e}"))),
                    }
                }
            }
            None => {}
        }
    };

    res
}

/// Build overlay items for the local workspace files under `dir` (recursive).
fn file_items(dir: &std::path::Path) -> Vec<app::OverlayItem> {
    let mut files: Vec<(String, u64)> = Vec::new();
    walk_files(dir, dir, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
        .into_iter()
        .map(|(rel, size)| {
            let label = format!("{rel}  · {}", human_size(size));
            let detail = vec![
                rel.clone(),
                format!("size: {}", human_size(size)),
                format!("path: {}", dir.join(&rel).display()),
                String::new(),
                "^E opens this folder in your file manager.".into(),
            ];
            app::OverlayItem { label, detail, action: None }
        })
        .collect()
}

/// Recursively collect (relative path, size) of files under `root` (capped).
fn walk_files(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, u64)>) {
    if out.len() >= 2000 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let ft = match entry.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };
        let path = entry.path();
        if ft.is_dir() {
            walk_files(root, &path, out);
        } else if ft.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((rel, size));
        }
        if out.len() >= 2000 {
            return;
        }
    }
}

fn human_size(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let f = n as f64;
    if f >= MB {
        format!("{:.1}M", f / MB)
    } else if f >= KB {
        format!("{:.1}K", f / KB)
    } else {
        format!("{n}B")
    }
}

/// Reveal `dir` in the OS file manager (Finder / Explorer / xdg-open).
fn open_in_file_manager(dir: &std::path::Path) -> Result<()> {
    if !dir.exists() {
        let _ = std::fs::create_dir_all(dir);
    }
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(dir)
        .spawn()
        .map_err(|e| anyhow!("could not launch {program}: {e}"))?;
    Ok(())
}

/// Copy text to the system clipboard (pbcopy / clip / wl-copy|xclip|xsel).
fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "windows") {
        &[("clip", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    };
    let mut last = anyhow!("no clipboard tool available");
    for (prog, args) in tools {
        match Command::new(prog)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(text.as_bytes()).ok();
                }
                let _ = child.wait();
                return Ok(());
            }
            Err(e) => last = anyhow!("{prog}: {e}"),
        }
    }
    Err(last)
}

/// Latest published release version (no leading `v`) from GitHub, or None.
async fn latest_release_version() -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("strobes-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let v: serde_json::Value = client
        .get("https://api.github.com/repos/strobes-co/strobes-agents-cli/releases/latest")
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .map(|s| s.trim_start_matches('v').to_string())
}

/// Release target triple for this platform (matches the published assets).
fn release_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// Self-update: download the latest release for this platform and replace the
/// running binary in place. Headless — no TUI.
async fn cmd_update(force: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("strobes v{current}");
    let latest = latest_release_version()
        .await
        .ok_or_else(|| anyhow!("could not reach GitHub to check the latest release"))?;
    if !force && !version_is_newer(&latest, current) {
        println!("✔ already up to date (v{current}).");
        return Ok(());
    }
    let target = release_target()
        .ok_or_else(|| anyhow!("no prebuilt release for {}/{} — build from source",
            std::env::consts::OS, std::env::consts::ARCH))?;
    let url = format!(
        "https://github.com/strobes-co/strobes-agents-cli/releases/latest/download/strobes-{target}.tar.gz"
    );
    println!("↓ downloading v{latest} ({target})…");
    let bytes = reqwest::Client::builder()
        .user_agent(concat!("strobes-cli/", env!("CARGO_PKG_VERSION")))
        .build()?
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    let tmp = std::env::temp_dir().join(format!("strobes-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let tgz = tmp.join("strobes.tar.gz");
    std::fs::write(&tgz, &bytes)?;
    // Extract via the system `tar` (present on macOS/Linux and Windows 10+).
    let ok = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&tmp)
        .status()
        .map_err(|e| anyhow!("tar not available ({e}); extract manually from {url}"))?
        .success();
    if !ok {
        return Err(anyhow!("failed to extract the release archive"));
    }
    let binname = if cfg!(windows) { "strobes.exe" } else { "strobes" };
    let newbin = tmp.join(format!("strobes-{target}")).join(binname);
    if !newbin.exists() {
        return Err(anyhow!("binary missing after extraction: {}", newbin.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&newbin, std::fs::Permissions::from_mode(0o755))?;
    }

    let cur_exe = std::env::current_exe()?;
    replace_running_exe(&newbin, &cur_exe)?;
    let _ = std::fs::remove_dir_all(&tmp);

    println!("✔ updated to v{latest} → {}", cur_exe.display());
    println!("  restart `strobes` to use it.");
    Ok(())
}

/// Replace the (possibly running) executable at `cur_exe` with `newbin`.
fn replace_running_exe(newbin: &std::path::Path, cur_exe: &std::path::Path) -> Result<()> {
    let dir = cur_exe.parent().ok_or_else(|| anyhow!("cannot resolve install dir"))?;
    if cfg!(windows) {
        // Windows can't overwrite a running .exe; move it aside first.
        let old = cur_exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(cur_exe, &old)
            .map_err(|e| anyhow!("cannot move current exe aside ({e})"))?;
        std::fs::copy(newbin, cur_exe).map_err(|e| anyhow!("cannot place new exe ({e})"))?;
        Ok(())
    } else {
        // Stage in the same dir, then atomic-rename over the target (works even
        // while the old binary is running).
        let staged = dir.join(".strobes-update.tmp");
        std::fs::copy(newbin, &staged).map_err(|e| {
            anyhow!("cannot write to {} ({e}). Re-run with sudo, or your install dir isn't writable.", dir.display())
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
        }
        std::fs::rename(&staged, cur_exe).map_err(|e| {
            let _ = std::fs::remove_file(&staged);
            anyhow!("cannot replace {} ({e}). The install dir likely needs sudo.", cur_exe.display())
        })?;
        Ok(())
    }
}

/// True if dotted-numeric `latest` is newer than `current`.
fn version_is_newer(latest: &str, current: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.split('.')
            .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0))
            .collect()
    }
    let (l, c) = (parts(latest), parts(current));
    for i in 0..l.len().max(c.len()) {
        let (lv, cv) = (l.get(i).copied().unwrap_or(0), c.get(i).copied().unwrap_or(0));
        if lv != cv {
            return lv > cv;
        }
    }
    false
}

fn finding_items(fs: Vec<api::Finding>) -> Vec<app::OverlayItem> {
    fs.into_iter()
        .map(|f| {
            let label = format!("[{}] {}  · {}", f.severity_label, trunc(&f.title, 80), f.state_label);
            let mut detail = vec![
                f.title.clone(),
                String::new(),
                format!("Severity: {}    State: {}    CVSS: {}",
                    f.severity_label, f.state_label,
                    f.cvss.map(|c| c.to_string()).unwrap_or_else(|| "-".into())),
            ];
            if let Some(a) = &f.asset {
                detail.push(format!("Asset: {a}"));
            }
            detail.push(format!("Finding ID: {}", f.id));
            if !f.description.trim().is_empty() {
                detail.push(String::new());
                detail.push("── Description ──".into());
                detail.extend(f.description.lines().map(|l| l.to_string()));
            }
            if !f.mitigation.trim().is_empty() {
                detail.push(String::new());
                detail.push("── Mitigation ──".into());
                detail.extend(f.mitigation.lines().map(|l| l.to_string()));
            }
            app::OverlayItem { label, detail, action: Some(f.id.to_string()) }
        })
        .collect()
}

fn approval_items(aps: Vec<api::Approval>) -> Vec<app::OverlayItem> {
    aps.into_iter()
        .map(|a| {
            let label = format!("[{}] {}  · {}", a.state, a.action_type, trunc(&a.summary, 70));
            let mut detail = vec![
                format!("Action: {}", a.action_type),
                format!("Module: {}    State: {}", a.module, a.state),
                String::new(),
                "── Summary ──".into(),
            ];
            detail.extend(a.summary.lines().map(|l| l.to_string()));
            let targets = serde_json::to_string(&a.target_ids).unwrap_or_default();
            if targets != "null" && !targets.is_empty() {
                detail.push(String::new());
                detail.push(format!("Targets: {targets}"));
            }
            app::OverlayItem { label, detail, action: Some(a.id) }
        })
        .collect()
}

fn thread_items(ts: Vec<api::Thread>, current: &str, in_workspace: bool) -> Vec<app::OverlayItem> {
    let new_label = if in_workspace {
        "➕  New chat (in this workspace)".to_string()
    } else {
        "➕  New chat".to_string()
    };
    let mut items = vec![app::OverlayItem {
        label: new_label,
        detail: vec!["Press Enter to start a new chat thread.".into()],
        action: Some("__new__".into()),
    }];
    items.extend(ts.into_iter().map(|t| {
        let here = if t.id == current { " ●" } else { "" };
        let title = if t.title.is_empty() { "(untitled)".into() } else { t.title.clone() };
        let label = format!("{title}{here}  · {}  {}", t.status, trunc(&t.last_message, 50));
        let detail = vec![title, format!("status: {}", t.status), format!("id: {}", t.id),
            String::new(), "Press Enter to switch to this thread.".into()];
        app::OverlayItem { label, detail, action: Some(t.id) }
    }));
    items
}

fn workspace_items(
    ws: Vec<api::Workspace>,
    counts: &[Option<usize>],
    credits: &std::collections::HashMap<String, f64>,
    profile: &config::Profile,
    cfg: &Config,
) -> Vec<app::OverlayItem> {
    let mut items = vec![app::OverlayItem {
        label: "➕  New workspace".to_string(),
        detail: vec![
            "Press Enter to create a new workspace.".into(),
            "The AI setup chat opens to configure & name it.".into(),
        ],
        action: Some("__new_ws__".into()),
    }];
    items.extend(ws.into_iter().enumerate().map(|(i, w)| {
        let bound = if Some(&w.id) == profile.workspace_id.as_ref() { " ●" } else { "" };
        let name = if w.name.is_empty() { "(unnamed)".into() } else { w.name.clone() };
        let tcount = match counts.get(i).copied().flatten() {
            Some(n) => format!("{n} threads"),
            None => "? threads".into(),
        };
        let cr = credits.get(&w.id).copied().unwrap_or(0.0);
        let opens = cfg.workspace_open_count(&w.id);
        let recent = if opens > 0 { format!(" · ↻ recent ×{opens}") } else { String::new() };
        let label = format!("{name}{bound}  · {} · {tcount} · ◈ {cr:.2} cr{recent}", w.status);
        let mut detail = vec![name, format!("status: {}", w.status), format!("threads: {tcount}"),
            format!("credits: {cr:.3}")];
        if opens > 0 {
            detail.push(format!("opened {opens}× from this machine"));
        }
        detail.push(format!("id: {}", w.id));
        detail.push(String::new());
        detail.push("Press Enter to bind this workspace (enables ^F / ^A).".into());
        app::OverlayItem { label, detail, action: Some(w.id) }
    }));
    items
}

/// Count threads per workspace concurrently (one `list_threads` call each).
async fn workspace_thread_counts(client: &api::ApiClient, ws: &[api::Workspace]) -> Vec<Option<usize>> {
    let futs: Vec<_> = ws.iter().map(|w| client.list_threads(Some(&w.id))).collect();
    futures_util::future::join_all(futs)
        .await
        .into_iter()
        .map(|r| r.ok().map(|t| t.len()))
        .collect()
}

/// List workspaces and build overlay items including per-workspace thread counts.
async fn workspace_overlay_items(
    client: &api::ApiClient,
    profile: &config::Profile,
) -> Result<Vec<app::OverlayItem>> {
    let cfg = Config::load();
    let mut ws = client.list_workspaces().await?;
    // Most-opened (then most-recent) first; never-opened keep server order.
    ws.sort_by(|a, b| {
        cfg.workspace_open_count(&b.id)
            .cmp(&cfg.workspace_open_count(&a.id))
            .then(cfg.workspace_last_opened(&b.id).cmp(&cfg.workspace_last_opened(&a.id)))
    });
    let (counts, credits) =
        tokio::join!(workspace_thread_counts(client, &ws), workspace_credits_map(client));
    Ok(workspace_items(ws, &counts, &credits, profile, &cfg))
}

async fn cmd_workflow(profile: config::Profile, sub: WorkflowCmd, tenant: &str) -> Result<()> {
    match sub {
        WorkflowCmd::Remote { sub } => return cmd_workflow_remote(profile, sub, tenant.to_string()).await,
        WorkflowCmd::Run { file, var, no_tui, non_interactive, var_file, timeout, fail_on_findings } => {
            require_complete(&profile)?;
            let def = workflow::load(&file)?;
            let abs_file = std::path::Path::new(&file)
                .canonicalize()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| file.clone());

            // Vars from --var-file (JSON map) are loaded first (lowest priority).
            let mut file_vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            if let Some(ref vf) = var_file {
                let raw = std::fs::read_to_string(vf)
                    .map_err(|e| anyhow!("cannot read var-file '{vf}': {e}"))?;
                let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw)
                    .map_err(|e| anyhow!("var-file '{vf}' must be a JSON object: {e}"))?;
                for (k, v) in map {
                    file_vars.insert(k, v.as_str().unwrap_or_default().to_string());
                }
            }

            // Vars explicitly set via -v flags take priority over var-file.
            let cli_vars: std::collections::HashMap<String, String> = var
                .iter()
                .filter_map(|kv| {
                    let mut it = kv.splitn(2, '=');
                    Some((it.next()?.to_string(), it.next()?.to_string()))
                })
                .collect();

            // Merge: file_vars < cli_vars
            let mut extra_vars = file_vars;
            extra_vars.extend(cli_vars);

            // Print a brief workflow summary.
            let total_tasks: usize = def.phases.iter().map(|p| p.tasks.len()).sum();
            println!(
                "\n  Workflow : {}\n  Phases   : {}  |  Tasks: {}",
                def.name,
                def.phases.len(),
                total_tasks,
            );
            if !def.description.is_empty() {
                println!("  {}", def.description);
            }

            if !def.variables.is_empty() {
                let mut keys: Vec<&String> = def.variables.keys().collect();
                keys.sort();
                let all_provided = keys.iter().all(|k| extra_vars.contains_key(*k));
                if !all_provided && non_interactive {
                    let missing: Vec<&String> = keys.iter().filter(|k| !extra_vars.contains_key(k.as_str())).copied().collect();
                    return Err(anyhow!(
                        "missing required workflow variable(s): {} — pass with -v KEY=VALUE or --var-file",
                        missing.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")
                    ));
                }
                if !all_provided {
                    println!("\n  Variables (Enter to keep default):");
                }
                for k in keys {
                    if extra_vars.contains_key(k) {
                        println!("  {k} = {}", extra_vars[k]);
                        continue;
                    }
                    let default = &def.variables[k];
                    let secret = ["key", "secret", "token", "password", "pass", "credential"]
                        .iter()
                        .any(|s| k.to_lowercase().contains(s));
                    let val = prompt_line(&format!("  {k}"), default, secret)?;
                    extra_vars.insert(k.clone(), val);
                }
            }
            println!();

            let (ev_tx, ev_rx) =
                mpsc::unbounded_channel::<workflow_runner::WfEvent>();

            if no_tui {
                // Headless: print events to stdout.
                let ev_tx2 = ev_tx.clone();
                let def2 = def.clone();
                let profile2 = profile.clone();
                let extra2 = extra_vars.clone();
                let abs_file2 = abs_file.clone();
                let runner = tokio::spawn(async move {
                    if let Err(e) =
                        workflow_runner::run(def2, profile2, ev_tx2.clone(), extra2, None, abs_file2).await
                    {
                        let _ = ev_tx2.send(workflow_runner::WfEvent::WorkflowFailed {
                            reason: e.to_string(),
                        });
                    }
                });
                let mut rx = ev_rx;
                let mut failed = false;
                let mut any_tasks_failed = false;
                let mut run_workspace_id: Option<String> = None;
                let deadline = timeout.map(|s| tokio::time::Instant::now() + std::time::Duration::from_secs(s));
                loop {
                    let ev_opt = if let Some(dl) = deadline {
                        tokio::select! {
                            _ = tokio::time::sleep_until(dl) => {
                                runner.abort();
                                eprintln!("error: workflow timed out after {}s", timeout.unwrap_or(0));
                                return Err(anyhow!("workflow timed out after {}s", timeout.unwrap_or(0)));
                            }
                            ev = rx.recv() => ev,
                        }
                    } else {
                        rx.recv().await
                    };
                    let ev = match ev_opt {
                        None => break,
                        Some(e) => e,
                    };
                    use workflow_runner::WfEvent::*;
                    match &ev {
                        Log(m) => println!("{m}"),
                        WorkspaceReady { id, name } => {
                            run_workspace_id = Some(id.clone());
                            println!("workspace: {name} [{id}]");
                        }
                        SetupStarted { thread_id } => {
                            println!("▶ workspace-setup ({}…)", &thread_id[..8.min(thread_id.len())])
                        }
                        PhaseStarted { phase } => println!("▶ phase: {phase}"),
                        TaskStarted { task, thread_id, .. } => {
                            println!("▶ {task} ({}…)", &thread_id[..8.min(thread_id.len())])
                        }
                        TaskOutput { task, text } => print!("[{task}] {text}"),
                        TaskDone { task } => println!("✔ {task}"),
                        TaskFailed { task, reason } => {
                            println!("✗ {task}: {reason}");
                            any_tasks_failed = true;
                        }
                        TaskSkipped { task } => println!("↷ {task} (skipped)"),
                        WorkflowDone => println!("✔ workflow complete"),
                        WorkflowFailed { reason } => {
                            println!("✗ workflow failed: {reason}");
                            failed = true;
                        }
                    }
                }
                let _ = runner.await;
                if failed {
                    return Err(anyhow!("workflow failed"));
                }
                if any_tasks_failed {
                    return Err(anyhow!("one or more workflow tasks failed"));
                }
                // --fail-on-findings check after successful workflow completion.
                if let (Some(threshold), Some(ws)) = (&fail_on_findings, &run_workspace_id) {
                    let client = api::ApiClient::new(profile.clone())?;
                    let threshold_level = severity_level(threshold);
                    let findings = client.list_workspace_findings(ws).await.unwrap_or_default();
                    let matching: Vec<_> = findings.iter()
                        .filter(|f| severity_level(&f.severity_label) >= threshold_level)
                        .collect();
                    if !matching.is_empty() {
                        eprintln!("findings: {} finding(s) at or above '{threshold}' severity", matching.len());
                        for f in &matching {
                            eprintln!("  [{}] {}", f.severity_label, f.title);
                        }
                        return Err(anyhow!("{} finding(s) at or above '{}' severity — failing build", matching.len(), threshold));
                    }
                }
            } else {
                // TUI mode — one terminal instance shared with any drill-down chat views.
                let ev_tx2 = ev_tx.clone();
                let def2 = def.clone();
                let profile2 = profile.clone();
                let extra2 = extra_vars.clone();
                let abs_file3 = abs_file.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        workflow_runner::run(def2, profile2, ev_tx2.clone(), extra2, None, abs_file3).await
                    {
                        let _ = ev_tx2.send(workflow_runner::WfEvent::WorkflowFailed {
                            reason: e.to_string(),
                        });
                    }
                });
                let mut terminal = ratatui::init();
                enable_mouse();
                let r = workflow_tui::run_tui(
                    &mut terminal,
                    def,
                    ev_rx,
                    profile.clone(),
                    tenant.to_string(),
                )
                .await;
                disable_mouse();
                ratatui::restore();
                r?;
            }
            Ok(())
        }
        WorkflowCmd::List => {
            let files = workflow::list_workflows(".");
            if files.is_empty() {
                println!(
                    "no workflow files (.yaml/.yml with 'phases:') found in current directory"
                );
            } else {
                for f in &files {
                    println!("{f}");
                }
                println!("\n{} file(s) found", files.len());
            }
            Ok(())
        }
        WorkflowCmd::Init { output } => {
            let tpl = workflow::starter_template();
            match output {
                Some(path) => {
                    std::fs::write(&path, tpl)?;
                    println!("✔ wrote {path}");
                    println!("edit the file, then run: strobes workflow run {path}");
                }
                None => print!("{tpl}"),
            }
            Ok(())
        }

        WorkflowCmd::History => {
            let runs = workflow_state::list_runs();
            if runs.is_empty() {
                println!("No workflow runs recorded yet.");
                println!("Runs are saved in: {}", workflow_state::runs_dir().display());
                return Ok(());
            }
            println!(
                "\n{:<38}  {:<26}  {:<10}  {:<6}  {}",
                "RUN ID", "WORKFLOW", "STATUS", "DONE", "STARTED"
            );
            println!("{}", "─".repeat(96));
            for r in &runs {
                let done = format!("{}/{}", r.done_count(), r.total_tasks());
                let name_trunc = if r.workflow_name.len() > 26 {
                    format!("{}…", &r.workflow_name[..25])
                } else {
                    r.workflow_name.clone()
                };
                let started = r
                    .started_at
                    .trim_end_matches('Z')
                    .replacen('T', " ", 1);
                let started = &started[..started.len().min(19)];
                println!(
                    "{:<38}  {:<26}  {:<10}  {:<6}  {}",
                    r.id, name_trunc, r.status.label(), done, started
                );
            }
            println!();
            Ok(())
        }

        WorkflowCmd::Resume { id, no_tui } => {
            require_complete(&profile)?;
            let resume_record = workflow_state::load(&id)?;

            // Validate we can still load the workflow file.
            let def = workflow::load(&resume_record.workflow_file).map_err(|e| {
                anyhow!(
                    "cannot reload workflow file '{}': {e}\n\
                     (if the file moved, update 'workflow_file' in {})",
                    resume_record.workflow_file,
                    workflow_state::runs_dir().join(format!("{id}.json")).display()
                )
            })?;

            let vars = resume_record.vars.clone();

            println!("\n  Resuming : {}", resume_record.workflow_name);
            println!("  Run ID   : {id}");
            println!(
                "  Progress : {}/{} tasks done",
                resume_record.done_count(),
                resume_record.total_tasks()
            );
            println!();

            let (ev_tx, ev_rx) = mpsc::unbounded_channel::<workflow_runner::WfEvent>();

            if no_tui {
                let ev_tx2 = ev_tx.clone();
                let def2 = def.clone();
                let profile2 = profile.clone();
                let resume2 = Some(resume_record);
                let wf_file = String::new(); // unused — taken from resume record
                let runner = tokio::spawn(async move {
                    if let Err(e) = workflow_runner::run(
                        def2, profile2, ev_tx2.clone(), vars, resume2, wf_file,
                    )
                    .await
                    {
                        let _ = ev_tx2.send(workflow_runner::WfEvent::WorkflowFailed {
                            reason: e.to_string(),
                        });
                    }
                });
                let mut rx = ev_rx;
                let mut failed = false;
                while let Some(ev) = rx.recv().await {
                    use workflow_runner::WfEvent::*;
                    match &ev {
                        Log(m) => println!("{m}"),
                        WorkspaceReady { id, name } => println!("workspace: {name} [{id}]"),
                        SetupStarted { thread_id } => println!(
                            "▶ workspace-setup ({}…)",
                            &thread_id[..8.min(thread_id.len())]
                        ),
                        PhaseStarted { phase } => println!("▶ phase: {phase}"),
                        TaskStarted { task, thread_id, .. } => println!(
                            "▶ {task} ({}…)",
                            &thread_id[..8.min(thread_id.len())]
                        ),
                        TaskOutput { task, text } => println!("[{task}] {text}"),
                        TaskDone { task } => println!("✔ {task}"),
                        TaskFailed { task, reason } => println!("✗ {task}: {reason}"),
                        TaskSkipped { task } => println!("↷ {task} (skipped)"),
                        WorkflowDone => println!("✔ workflow complete"),
                        WorkflowFailed { reason } => {
                            println!("✗ workflow failed: {reason}");
                            failed = true;
                        }
                    }
                }
                let _ = runner.await;
                if failed {
                    return Err(anyhow!("workflow failed"));
                }
            } else {
                let ev_tx2 = ev_tx.clone();
                let def2 = def.clone();
                let profile2 = profile.clone();
                let resume2 = Some(resume_record);
                let wf_file = String::new(); // unused — taken from resume record
                tokio::spawn(async move {
                    if let Err(e) = workflow_runner::run(
                        def2, profile2, ev_tx2.clone(), vars, resume2, wf_file,
                    )
                    .await
                    {
                        let _ = ev_tx2.send(workflow_runner::WfEvent::WorkflowFailed {
                            reason: e.to_string(),
                        });
                    }
                });
                let mut terminal = ratatui::init();
                enable_mouse();
                let r = workflow_tui::run_tui(
                    &mut terminal,
                    def,
                    ev_rx,
                    profile.clone(),
                    tenant.to_string(),
                )
                .await;
                disable_mouse();
                ratatui::restore();
                r?;
            }
            Ok(())
        }
    }
}

// ── Remote workflow management (GraphQL API) ─────────────────────────────────

fn resolve_workspace(w: Option<String>, profile: &config::Profile) -> Result<String> {
    w.or_else(|| profile.workspace_id.clone())
        .ok_or_else(|| anyhow!("no workspace — pass --workspace <UUID> or run `strobes bind` first"))
}

/// Like `resolve_workspace` but shows a workspace picker when no workspace is bound.
/// Returns `None` if the user cancels the picker.
async fn resolve_workspace_or_pick(
    workspace: Option<String>,
    profile: &config::Profile,
    client: &api::ApiClient,
) -> Result<Option<String>> {
    if let Some(w) = workspace.or_else(|| profile.workspace_id.clone()) {
        return Ok(Some(w));
    }
    let workspaces = client.list_workspaces().await?;
    if workspaces.is_empty() {
        return Err(anyhow!("no workspaces found — create one with `strobes bind`"));
    }
    let labels: Vec<String> = workspaces
        .iter()
        .map(|w| format!("{}…  {}", &w.id[..8.min(w.id.len())], w.name))
        .collect();
    match picker::select("Select workspace", &labels).await? {
        picker::Nav::Item(i) => Ok(Some(workspaces[i].id.clone())),
        _ => Ok(None),
    }
}

/// Convert a kebab-case / lower-case name to Title Case words.
fn title_case(s: &str) -> String {
    s.replace('-', " ")
        .replace('_', " ")
        .split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Serialise `Vec<PhaseDef>` from a workflow YAML into a GraphQL input literal
/// suitable for `createCustomWorkflow` / `editCustomWorkflow`.
fn phases_to_json(phases: &[workflow::PhaseDef]) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = phases
        .iter()
        .enumerate()
        .map(|(i, phase)| {
            let tasks: Vec<serde_json::Value> = phase
                .tasks
                .iter()
                .map(|task| {
                    serde_json::json!({
                        "key": task.name,
                        "title": title_case(&task.name),
                        "instructions": task.prompt,
                        "agentType": "general",
                        "taskType": "agent",
                    })
                })
                .collect();
            serde_json::json!({
                "key": phase.name,
                "name": title_case(&phase.name),
                "order": i,
                "gateType": "all_complete",
                "failurePolicy": "continue",
                "tasks": tasks,
            })
        })
        .collect();
    serde_json::Value::Array(arr)
}

async fn cmd_workflow_remote(
    profile: config::Profile,
    sub: RemoteWorkflowCmd,
    tenant: String,
) -> Result<()> {
    require_complete(&profile)?;
    let client = api::ApiClient::new(profile.clone())?;

    match sub {
        RemoteWorkflowCmd::Templates => {
            let templates = client.workflow_templates().await?;
            if templates.is_empty() {
                println!("(no templates available)");
                return Ok(());
            }
            for t in &templates {
                let req = if t.required_variables.is_empty() {
                    String::new()
                } else {
                    format!("  requires: {}", t.required_variables.join(", "))
                };
                println!(
                    "{} {}  [{}]  {} phases{}",
                    if t.icon.is_empty() { "•" } else { &t.icon },
                    t.slug,
                    t.name,
                    t.phase_count,
                    req
                );
                if !t.description.is_empty() {
                    println!("   {}", t.description);
                }
                for p in &t.phases {
                    println!("   {}: {} ({} tasks)", p.order, p.name, p.task_count);
                }
                println!();
            }
        }

        RemoteWorkflowCmd::Status { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            match client.workspace_workflow(&ws).await? {
                None => println!("(no workflow attached to workspace {}…)", &ws[..8.min(ws.len())]),
                Some(wf) => {
                    println!("workflow  {}", wf.workflow_id);
                    println!("status    {}", wf.status);
                    if let Some(slug) = &wf.template_slug {
                        println!("template  {slug}");
                    }
                    if let Some(v) = &wf.template_version {
                        println!("version   {v}");
                    }
                    if let Some(phase) = &wf.current_phase_key {
                        println!("phase     {phase}");
                    }
                    println!("tasks     {}/{}", wf.completed_tasks, wf.total_tasks);
                    if let Some(s) = &wf.started_at { println!("started   {s}"); }
                    if let Some(c) = &wf.completed_at { println!("finished  {c}"); }
                    if !wf.phases.is_empty() {
                        println!("\nphases:");
                        for p in &wf.phases {
                            let cur = if wf.current_phase_key.as_deref() == Some(&p.phase_key) {
                                " ←"
                            } else {
                                ""
                            };
                            println!("  {:>12}  {}  {}{cur}", p.status, p.phase_key, p.phase_name);
                        }
                    }
                }
            }
        }

        RemoteWorkflowCmd::Attach { workspace, template, var } => {
            // For attach, always show the workspace picker when --workspace is not
            // given explicitly — defaulting silently to the bound workspace would
            // attach to the wrong place without the user realising.
            let ws = match workspace {
                Some(w) => w,
                None => {
                    let workspaces = client.list_workspaces().await?;
                    if workspaces.is_empty() {
                        return Err(anyhow!("no workspaces found — create one with `strobes bind`"));
                    }
                    let labels: Vec<String> = workspaces.iter()
                        .map(|w| format!("{}…  {}", &w.id[..8.min(w.id.len())], w.name))
                        .collect();
                    match picker::select("Select workspace", &labels).await? {
                        picker::Nav::Item(i) => workspaces[i].id.clone(),
                        _ => return Ok(()),
                    }
                }
            };

            // Fetch templates once — needed for both the picker and required-var lookup.
            let templates = client.workflow_templates().await?;

            // Resolve template slug and its required variables.
            let (slug, required_vars) = match template {
                Some(s) => {
                    let req = templates.iter()
                        .find(|t| t.slug == s)
                        .map(|t| t.required_variables.clone())
                        .unwrap_or_default();
                    (s, req)
                }
                None => {
                    if templates.is_empty() {
                        return Err(anyhow!("no workflow templates available"));
                    }
                    let labels: Vec<String> = templates.iter().map(|t| {
                        let req_hint = if t.required_variables.is_empty() {
                            String::new()
                        } else {
                            format!("  [{}]", t.required_variables.join(", "))
                        };
                        format!(
                            "{} {}  — {}{}",
                            if t.icon.is_empty() { "•" } else { &t.icon },
                            t.slug,
                            t.name,
                            req_hint,
                        )
                    }).collect();
                    match picker::select("Select a workflow template", &labels).await? {
                        picker::Nav::Item(i) => {
                            let t = &templates[i];
                            (t.slug.clone(), t.required_variables.clone())
                        }
                        _ => return Ok(()),
                    }
                }
            };

            // Start with variables supplied via -v flags.
            let mut vars: std::collections::HashMap<String, String> = var.iter()
                .filter_map(|kv| {
                    let mut it = kv.splitn(2, '=');
                    Some((it.next()?.to_string(), it.next()?.to_string()))
                })
                .collect();

            // Prompt for any required variables not yet provided.
            let missing: Vec<&String> = required_vars.iter()
                .filter(|k| !vars.contains_key(k.as_str()))
                .collect();
            if !missing.is_empty() {
                println!("  This template requires the following variables:");
                use std::io::Write;
                for key in missing {
                    print!("  {key}: ");
                    std::io::stdout().flush()?;
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    let val = line.trim().to_string();
                    if val.is_empty() {
                        return Err(anyhow!("variable '{key}' is required and cannot be empty"));
                    }
                    vars.insert(key.clone(), val);
                }
            }

            let variables: serde_json::Value = vars.iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect::<serde_json::Map<_, _>>()
                .into();

            let wf = client.attach_workflow_template(&ws, &slug, &variables).await?;
            println!("✔ attached '{slug}' to workspace {}…", &ws[..8.min(ws.len())]);
            println!("  workflow {} [{}]", wf.workflow_id, wf.status);
        }

        RemoteWorkflowCmd::Detach { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            print!(
                "detach workflow from workspace {}…? [y/N] ",
                &ws[..8.min(ws.len())]
            );
            use std::io::Write;
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                println!("cancelled.");
                return Ok(());
            }
            client.detach_workflow(&ws).await?;
            println!("✔ workflow detached");
        }

        RemoteWorkflowCmd::Create { workspace, file, var } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            let def = workflow::load(&file)?;
            let phases_json = phases_to_json(&def.phases);
            let mut vars: serde_json::Map<String, serde_json::Value> = def
                .variables
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            for kv in &var {
                let mut it = kv.splitn(2, '=');
                if let (Some(k), Some(v)) = (it.next(), it.next()) {
                    vars.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                }
            }
            let vars_json = serde_json::Value::Object(vars);
            let wf = client
                .create_custom_workflow(&ws, &def.name, &phases_json, &vars_json)
                .await?;
            let total_tasks: usize = def.phases.iter().map(|p| p.tasks.len()).sum();
            println!("✔ workflow created: {} [{}]", wf.workflow_id, wf.status);
            println!(
                "  {} phases, {total_tasks} tasks from '{file}'",
                def.phases.len()
            );
        }

        RemoteWorkflowCmd::Edit { workspace, file } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            let def = workflow::load(&file)?;
            let phases_json = phases_to_json(&def.phases);
            let wf = client
                .edit_custom_workflow(&ws, &def.name, &phases_json)
                .await?;
            println!("✔ workflow updated: {} [{}]", wf.workflow_id, wf.status);
        }

        RemoteWorkflowCmd::Sync { workspace, file, var } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            let def = workflow::load(&file)?;
            let phases_json = phases_to_json(&def.phases);
            let mut vars: serde_json::Map<String, serde_json::Value> = def
                .variables
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            for kv in &var {
                let mut it = kv.splitn(2, '=');
                if let (Some(k), Some(v)) = (it.next(), it.next()) {
                    vars.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                }
            }
            let vars_json = serde_json::Value::Object(vars);
            match client.workspace_workflow(&ws).await? {
                None => {
                    let wf = client
                        .create_custom_workflow(&ws, &def.name, &phases_json, &vars_json)
                        .await?;
                    println!(
                        "✔ created workflow from '{file}': {} [{}]",
                        wf.workflow_id, wf.status
                    );
                }
                Some(wf) if wf.status == "running" => {
                    return Err(anyhow!(
                        "workflow {} is currently running — cancel it first before syncing",
                        wf.workflow_id
                    ));
                }
                Some(existing) => {
                    let updated = client
                        .edit_custom_workflow(&ws, &def.name, &phases_json)
                        .await?;
                    println!(
                        "✔ updated workflow {} from '{file}' [{} → {}]",
                        updated.workflow_id, existing.status, updated.status
                    );
                }
            }
        }

        RemoteWorkflowCmd::Save { workspace, name, description, icon } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            let slug = client
                .save_workflow_as_template(
                    &ws,
                    &name,
                    &description.unwrap_or_default(),
                    &icon,
                )
                .await?;
            println!("✔ saved as template '{slug}'");
            println!("  use with: strobes workflow remote attach --template {slug}");
        }

        RemoteWorkflowCmd::DeleteTemplate { slug } => {
            if !slug.starts_with("custom:") {
                return Err(anyhow!(
                    "only custom: templates can be deleted (got '{slug}')"
                ));
            }
            print!("delete template '{slug}'? [y/N] ");
            use std::io::Write;
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                println!("cancelled.");
                return Ok(());
            }
            client.delete_custom_workflow_template(&slug).await?;
            println!("✔ deleted template '{slug}'");
        }

        RemoteWorkflowCmd::Pause { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            client.pause_workflow(&ws).await?;
            println!("✔ workflow paused");
        }
        RemoteWorkflowCmd::Resume { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            client.resume_workflow(&ws).await?;
            println!("✔ workflow resumed");
        }
        RemoteWorkflowCmd::Cancel { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            client.cancel_workflow(&ws).await?;
            println!("✔ workflow cancelled");
        }
        RemoteWorkflowCmd::Restart { workspace, from_phase } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            match from_phase {
                Some(phase_key) => {
                    client.restart_workflow_from_phase(&ws, &phase_key).await?;
                    println!("✔ workflow restarted from phase '{phase_key}'");
                }
                None => {
                    client.restart_workflow(&ws).await?;
                    println!("✔ workflow restarted");
                }
            }
        }
        RemoteWorkflowCmd::Advance { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            client.advance_workflow_phase(&ws).await?;
            println!("✔ phase advanced");
        }
        RemoteWorkflowCmd::Watch { workspace } => {
            let ws = match resolve_workspace_or_pick(workspace, &profile, &client).await? {
                Some(w) => w,
                None => return Ok(()),
            };
            let mut terminal = ratatui::init();
            enable_mouse();
            let r = remote_wf_tui::run(&mut terminal, &client, ws, profile.clone(), tenant.to_string()).await;
            disable_mouse();
            ratatui::restore();
            r?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn notify_gating_and_payload() {
        // Disabled → nothing.
        assert!(notify_done_bytes(10, true, 4).is_none());
        // Too short → nothing.
        assert!(notify_done_bytes(2, false, 4).is_none());
        // Long enough → BEL + OSC 9 + OSC 777 present.
        let b = notify_done_bytes(5, false, 4).expect("should notify");
        assert_eq!(b[0], 0x07, "starts with BEL");
        let s = String::from_utf8(b).unwrap();
        assert!(s.contains("\x1b]9;"), "has OSC 9 notification");
        assert!(s.contains("\x1b]777;notify;"), "has OSC 777 notification");
        // min=0 notifies even instant replies.
        assert!(notify_done_bytes(0, false, 0).is_some());
    }

    /// Verifies the full CLI_LOCAL round-trip in Rust against a mock pulse
    /// server: server sends a `tool.local_execute`, the client runs it on the
    /// local machine and replies with `tool.local_result` carrying the output.
    #[tokio::test]
    async fn local_execute_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let ev = serde_json::json!({
                "type": "tool",
                "data": {"status": "local_execute", "toolName": "execute_command",
                         "requestId": "r1", "input": {"command": "echo RUST_ROUNDTRIP_OK"}}
            });
            ws.send(Message::Text(ev.to_string())).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(t) = msg {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    if v["type"] == "tool.local_result" {
                        return v;
                    }
                }
            }
            panic!("no tool.local_result received");
        });

        let profile = config::Profile {
            base_url: format!("http://127.0.0.1:{port}"),
            org_id: "o".into(),
            master_key: "k".into(),
            deployment: "enterprise".into(),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
        let _handle = pulse::connect(&profile, "t", tx, None).await.unwrap();

        let v = tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("server timeout")
            .expect("server task");
        assert_eq!(v["type"], "tool.local_result");
        let out = v["payload"]["output"].as_str().unwrap_or("");
        assert!(out.contains("RUST_ROUNDTRIP_OK"), "got: {out}");

        // Drain a couple app events to ensure the client surfaced the activity.
        let mut saw_local = false;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            if let pulse::AppEvent::LocalToolDone { .. } = ev {
                saw_local = true;
            }
        }
        assert!(saw_local, "client did not surface LocalToolDone");
    }

    /// Verifies the request_human_input round-trip: server sends an interrupt
    /// event, the client surfaces AppEvent::Interrupt, and respond_interrupt
    /// sends a correctly-shaped interrupt.response.
    #[tokio::test]
    async fn interrupt_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let ev = serde_json::json!({
                "type": "interrupt",
                "data": {"status": "requested", "interruptId": "i1",
                         "title": "OTP", "message": "Enter the code",
                         "formSchema": {"fields": [{"key": "otp", "label": "OTP", "type": "text"}]}}
            });
            ws.send(Message::Text(ev.to_string())).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(t) = msg {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    if v["type"] == "interrupt.response" {
                        return v;
                    }
                }
            }
            panic!("no interrupt.response received");
        });

        let profile = config::Profile {
            base_url: format!("http://127.0.0.1:{port}"),
            org_id: "o".into(),
            master_key: "k".into(),
            deployment: "enterprise".into(),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<pulse::AppEvent>();
        let handle = pulse::connect(&profile, "t", tx, None).await.unwrap();

        // Wait for the surfaced interrupt, then answer it.
        let mut answered = false;
        while let Ok(Some(ev)) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
            if let pulse::AppEvent::Interrupt { id, fields, .. } = ev {
                assert_eq!(id, "i1");
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].key, "otp");
                handle.respond_interrupt(&id, serde_json::json!({ "otp": "654321" }));
                answered = true;
                break;
            }
        }
        assert!(answered, "client never surfaced the interrupt");

        let v = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("server timeout")
            .expect("server task");
        assert_eq!(v["type"], "interrupt.response");
        assert_eq!(v["interrupt_id"], "i1");
        assert_eq!(v["response_data"]["otp"], "654321");
    }

    #[test]
    fn ws_url_and_prefix() {
        let p = config::Profile {
            base_url: "https://app.strobes.co".into(),
            org_id: "org".into(),
            master_key: "k".into(),
            deployment: "enterprise".into(),
            ..Default::default()
        };
        assert_eq!(p.api_prefix(), "/api/v1");
        assert_eq!(
            p.pulse_ws_url("T").unwrap(),
            "wss://app.strobes.co/ws/org/pulse/T/?api_key=k"
        );
    }
}
