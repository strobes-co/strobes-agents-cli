//! Chat commands that take arguments and do their work off the UI thread:
//! `/name args` → `run(name, args, ctx)` → text (or an action) for the chat.

use crate::{api, config, mcp, project};
use serde_json::{json, Value};

/// Commands handled here (they also appear in `/` and `/help`).
pub const COMMANDS: &[(&str, &str)] = &[
    ("agent", "Start a chat with a specialist agent: /agent [id]"),
    ("attach", "Attach files to your next message: /attach <file>…"),
    ("artifacts", "Files this chat produced: /artifacts · get|open <path> [--pdf]"),
    ("evidence", "Show the request/response behind an ev-… id: /evidence <ev-id>…"),
    ("memory", "Workspace memory: /memory [search <q> · add <text> · rm <id>] (or start a message with #)"),
    ("skills", "Skills: list · show <slug> · push <dir> · enable|disable <slug>"),
    ("creds", "Give the agent a credential without pasting it: /creds <NAME> KEY=VALUE… · /creds aws|github|jira <id>"),
    ("background", "Run a prompt in the cloud on this chat and keep going: /background <prompt>"),
    ("runs", "Agent runs: /runs [all] · cancel <id>"),
    ("schedule", "Recurring runs: /schedule · add <daily|weekly|…> <HH:MM> <prompt> · pause|resume|run|rm <id>"),
    ("report", "Workspace reports: /report · new [csv|threat|cve] · get <name>"),
    ("fork", "Branch this chat into a new one: /fork [title]"),
    ("share", "Copy a link to this chat for a teammate"),
    ("init", "Create a STROBES.md with scope and rules for this project"),
    ("mcp", "MCP servers: status · trust · reload"),
    ("hooks", "Your scripts around the agent's local tool calls (hooks.json)"),
];

pub fn is_command(name: &str) -> bool {
    COMMANDS.iter().any(|(c, _)| *c == name)
}

/// Commands whose arguments must not land in prompt history (secrets).
pub fn is_secret(name: &str) -> bool {
    name == "creds"
}

/// What the chat should do with a command's result.
pub enum Outcome {
    Text(String),
    /// Open another chat (fork, specialist agent).
    SwitchThread(String, String),
}

pub struct Ctx {
    pub profile: config::Profile,
    pub thread_id: String,
    /// The chat's model, for runs started from a command.
    pub model: Option<i64>,
}

impl Ctx {
    fn workspace(&self) -> Result<String, String> {
        self.profile.workspace_id.clone().ok_or_else(|| "no workspace is bound — pick one with /workspace".to_string())
    }
}

pub async fn run(name: &str, args: &str, ctx: &Ctx) -> Result<Outcome, String> {
    let args = args.trim();
    let (sub, rest) = args.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((args, ""));
    let text = |r: Result<String, String>| r.map(Outcome::Text);
    match name {
        "init" => text(init()),
        "mcp" => text(mcp_cmd(sub).await),
        "skills" => text(skills(sub, rest, ctx).await),
        "hooks" => Ok(Outcome::Text(hooks_text())),
        "artifacts" => text(artifacts(sub, rest, ctx).await),
        "evidence" => text(evidence(args, ctx).await),
        "attach" => text(attach(args, ctx).await),
        "agent" => agent(args, ctx).await,
        "memory" => text(memory(sub, rest, ctx).await),
        "creds" => text(creds(sub, rest, ctx).await),
        "background" => text(background(args, ctx).await),
        "runs" => text(runs(sub, rest, ctx).await),
        "schedule" => text(schedule(sub, rest, ctx).await),
        "report" => text(report(sub, rest, ctx).await),
        "fork" => fork(args, ctx).await,
        "share" => text(share(ctx).await),
        _ => Err(format!("unknown command /{name}")),
    }
}

fn e(err: anyhow::Error) -> String {
    format!("{err:#}")
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn list<'a>(v: &'a Value, k: &str) -> Vec<&'a Value> {
    v.get(k).and_then(|x| x.as_array()).map(|a| a.iter().collect()).unwrap_or_default()
}

fn human_size(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// `~/…` and quotes, for paths typed into the chat.
fn user_path(p: &str) -> std::path::PathBuf {
    let p = p.trim().trim_matches(|c| c == '"' || c == '\'');
    match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => std::path::PathBuf::from(p),
    }
}

/// A path as the user would type it: relative to the current directory when
/// under it, else with `~` for home.
pub fn short_path(p: &std::path::Path) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = p.strip_prefix(&cwd) {
            let r = rel.display().to_string();
            return if r.is_empty() { ".".into() } else { r };
        }
    }
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = p.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    p.display().to_string()
}

fn client(ctx: &Ctx) -> Result<api::ApiClient, String> {
    api::ApiClient::new(ctx.profile.clone()).map_err(|e| e.to_string())
}

fn init() -> Result<String, String> {
    let path = project::root().join("STROBES.md");
    if path.exists() {
        return Ok(format!(
            "{} already exists — the agent reads it every turn. Edit it to change scope or rules.",
            short_path(&path)
        ));
    }
    std::fs::write(&path, project::STARTER).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(format!(
        "created {}\nFill in scope and rules of engagement; the agent reads it at the start of every turn.",
        short_path(&path)
    ))
}

fn hooks_text() -> String {
    let rows = crate::hooks::list();
    let files = format!(
        "{} or {} (project, needs /mcp trust)",
        short_path(&config::config_dir().join("hooks.json")),
        short_path(&project::root().join(".strobes/hooks.json"))
    );
    if rows.is_empty() {
        return format!(
            "No hooks. Add them to {files}, in Claude Code's format:\n  {{\"hooks\": {{\"PreToolUse\": [{{\"matcher\": \"Shell\", \"hooks\": [{{\"type\": \"command\", \"command\": \"./scope-guard.sh\"}}]}}]}}}}\nA PreToolUse hook that exits 2 blocks the call; its stderr tells the agent why."
        );
    }
    let mut out = String::from("Hooks");
    for (event, matcher, cmd) in rows {
        out.push_str(&format!("\n  {event} [{matcher}] → {cmd}"));
    }
    out.push_str(&format!("\n  from {files}"));
    out
}

async fn mcp_cmd(sub: &str) -> Result<String, String> {
    match sub {
        "trust" => {
            mcp::trust_project().map_err(|e| e.to_string())?;
            mcp::start_all();
            mcp::wait_ready(std::time::Duration::from_secs(20)).await;
            Ok(format!("trusted this project ({}) — its MCP servers can run.\n{}", short_path(&project::root()), mcp_status()))
        }
        "reload" | "restart" => {
            mcp::start_all();
            mcp::wait_ready(std::time::Duration::from_secs(20)).await;
            Ok(mcp_status())
        }
        "" | "list" | "status" => Ok(mcp_status()),
        other => Err(format!("unknown /mcp {other} — try /mcp, /mcp trust, /mcp reload")),
    }
}

fn mcp_status() -> String {
    let rows = mcp::status();
    if rows.is_empty() {
        let files: Vec<String> = mcp::config_files().iter().map(|(f, _)| short_path(f)).collect();
        return format!(
            "No MCP servers configured.\nAdd them to one of:\n  {}\nSame format as Claude Code's .mcp.json: {{\"mcpServers\": {{\"name\": {{\"command\": \"…\", \"args\": []}}}}}}",
            files.join("\n  ")
        );
    }
    let mut out = String::from("MCP servers");
    for (name, status, tools, source) in rows {
        out.push_str(&format!("\n{name} — {status}"));
        if !tools.is_empty() {
            let shown: Vec<_> = tools.iter().take(8).cloned().collect();
            let more = if tools.len() > 8 { format!(" +{}", tools.len() - 8) } else { String::new() };
            out.push_str(&format!("\n    {}{more}", shown.join(", ")));
        }
        out.push_str(&format!("\n    from {}", short_path(std::path::Path::new(&source))));
    }
    out
}

async fn skills(sub: &str, rest: &str, ctx: &Ctx) -> Result<String, String> {
    match sub {
        "" | "list" => {
            let mut out = String::new();
            let local = project::local_skills();
            if local.is_empty() {
                out.push_str(&format!(
                    "No local skills — put one in {}/<slug>/SKILL.md (or {}).",
                    short_path(&project::root().join(".strobes/skills")),
                    short_path(&config::config_dir().join("skills"))
                ));
            } else {
                out.push_str("Local skills (the agent loads them as local:<slug>)");
                for s in &local {
                    out.push_str(&format!("\n  local:{} — {}", s.slug, s.description));
                }
            }
            let c = client(ctx)?;
            let q = ctx.profile.workspace_id.as_deref().map(|w| format!("?workspace_id={w}")).unwrap_or_default();
            if let Ok(v) = c.cli_json("GET", &format!("/cli/skills/{q}"), None).await {
                let org: Vec<_> = list(&v, "skills").into_iter().filter(|k| s(k, "source") == "org").collect();
                let system = list(&v, "skills").len() - org.len();
                out.push_str(&format!("\nOrganization skills ({}) · {system} built-in", org.len()));
                for k in org.iter().take(30) {
                    let on = match k.get("enabled_in_workspace").and_then(|b| b.as_bool()) {
                        Some(true) => " · on in this workspace",
                        _ => "",
                    };
                    out.push_str(&format!("\n  {} — {}{on}", s(k, "slug"), s(k, "description").chars().take(90).collect::<String>()));
                }
                out.push_str("\n/skills push <dir> uploads a local skill to the organization.");
            }
            Ok(out)
        }
        "show" => {
            if let Ok(text) = project::load_skill(rest) {
                return Ok(text.lines().take(40).collect::<Vec<_>>().join("\n"));
            }
            let v = client(ctx)?.cli_json("GET", &format!("/cli/skills/{}/", rest.trim()), None).await.map_err(e)?;
            let body = s(&v, "instructions");
            let mut out = format!("{} — {}\n", s(&v, "name"), s(&v, "description"));
            out.push_str(&body.lines().take(30).collect::<Vec<_>>().join("\n"));
            Ok(out)
        }
        "push" | "upload" => {
            let dir = if rest.is_empty() { return Err("usage: /skills push <dir with SKILL.md>".into()) } else { user_path(rest) };
            let dir = if dir.join("SKILL.md").is_file() {
                dir
            } else {
                // A bare slug: one of the local skills.
                project::local_skills().into_iter().find(|k| k.slug == rest.trim().trim_start_matches("local:")).map(|k| k.dir)
                    .ok_or_else(|| format!("{} has no SKILL.md", dir.display()))?
            };
            let zip = zip_dir(&dir).map_err(|err| format!("could not zip {}: {err}", dir.display()))?;
            let name = format!("{}.zip", dir.file_name().and_then(|n| n.to_str()).unwrap_or("skill"));
            let c = client(ctx)?;
            let check = c.cli_upload("/cli/skills/validate/", &[], &name, zip.clone()).await.map_err(e)?;
            if check.get("is_valid").and_then(|b| b.as_bool()) == Some(false) {
                let errs: Vec<String> = list(&check, "errors").iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect();
                return Err(format!("the skill isn't valid:\n  {}", errs.join("\n  ")));
            }
            let v = c.cli_upload("/cli/skills/upload/", &[], &name, zip).await.map_err(e)?;
            let skill = v.get("skill").cloned().unwrap_or_default();
            let mut out = format!("uploaded {} (v{}) to the organization", s(&skill, "slug"), skill.get("version").cloned().unwrap_or(json!(1)));
            for w in list(&v, "warnings") {
                out.push_str(&format!("\n  warning: {}", w.as_str().unwrap_or("")));
            }
            Ok(out)
        }
        "enable" | "disable" => {
            let ws = ctx.workspace()?;
            let slugs: Vec<&str> = rest.split_whitespace().collect();
            if slugs.is_empty() {
                return Err(format!("usage: /skills {sub} <slug>…"));
            }
            let v = client(ctx)?
                .cli_json("POST", &format!("/cli/workspaces/{ws}/skills/"), Some(json!({"slugs": slugs, "enabled": sub == "enable"})))
                .await
                .map_err(e)?;
            let unknown: Vec<&str> = list(&v, "unknown").iter().filter_map(|x| x.as_str()).collect();
            let mut out = format!("{} {} in this workspace", if sub == "enable" { "enabled" } else { "disabled" }, slugs.join(", "));
            if !unknown.is_empty() {
                out.push_str(&format!("\n  unknown: {}", unknown.join(", ")));
            }
            Ok(out)
        }
        other => Err(format!("unknown /skills {other} — try list, show, push, enable, disable")),
    }
}

fn zip_dir(dir: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d)?.flatten() {
                let p = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "__pycache__" || name == "node_modules" {
                    continue;
                }
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                    zip.start_file(rel, opts)?;
                    zip.write_all(&std::fs::read(&p)?)?;
                }
            }
        }
        zip.finish()?;
    }
    Ok(buf.into_inner())
}

async fn artifacts(sub: &str, rest: &str, ctx: &Ctx) -> Result<String, String> {
    let c = client(ctx)?;
    let t = &ctx.thread_id;
    match sub {
        "" | "list" => {
            let v = c.cli_json("GET", &format!("/cli/threads/{t}/artifacts/?recursive=true"), None).await.map_err(e)?;
            let files: Vec<_> = list(&v, "artifacts").into_iter().filter(|a| s(a, "kind") != "folder").collect();
            if files.is_empty() {
                return Ok("This chat hasn't produced any files yet.".into());
            }
            let mut out = format!("Artifacts ({})", files.len());
            for a in files.iter().take(40) {
                let size = a.get("size").and_then(|n| n.as_u64()).map(human_size).unwrap_or_default();
                out.push_str(&format!("\n  {}  {size}", s(a, "path")));
            }
            if files.len() > 40 {
                out.push_str(&format!("\n  … +{} more", files.len() - 40));
            }
            out.push_str("\n/artifacts get <path> saves one here · /artifacts open <path> opens it");
            Ok(out)
        }
        "get" | "open" | "download" => {
            let pdf = rest.ends_with("--pdf");
            let path = rest.trim_end_matches("--pdf").trim();
            if path.is_empty() {
                return Err(format!("usage: /artifacts {sub} <path> [--pdf]"));
            }
            let mut q = vec![("path", path)];
            if pdf {
                q.push(("format", "pdf"));
            }
            let bytes = c.cli_bytes(&format!("/cli/threads/{t}/artifacts/download/"), &q).await.map_err(e)?;
            let mut name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("artifact").to_string();
            if pdf && !name.ends_with(".pdf") {
                name = format!("{}.pdf", name.rsplit_once('.').map(|(a, _)| a).unwrap_or(&name));
            }
            let dest = if sub == "open" {
                std::env::temp_dir().join("strobes-artifacts").join(&name)
            } else {
                std::path::PathBuf::from("strobes-artifacts").join(&name)
            };
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
            }
            std::fs::write(&dest, &bytes).map_err(|err| err.to_string())?;
            if sub == "open" {
                crate::open_browser(&dest.display().to_string());
                Ok(format!("opened {name} ({})", human_size(bytes.len() as u64)))
            } else {
                Ok(format!("saved {} ({})", short_path(&std::fs::canonicalize(&dest).unwrap_or(dest)), human_size(bytes.len() as u64)))
            }
        }
        other => Err(format!("unknown /artifacts {other} — try list, get, open")),
    }
}

async fn evidence(args: &str, ctx: &Ctx) -> Result<String, String> {
    let refs: Vec<&str> = args.split(|c: char| c.is_whitespace() || c == ',').filter(|r| !r.is_empty()).collect();
    if refs.is_empty() {
        return Err("usage: /evidence ev-… [ev-…]".into());
    }
    let v = client(ctx)?
        .cli_json("GET", &format!("/cli/evidence/?refs={}&thread_id={}", refs.join(","), ctx.thread_id), None)
        .await
        .map_err(e)?;
    let mut out = String::new();
    if let Some(map) = v.get("evidence").and_then(|m| m.as_object()) {
        for (id, ev) in map {
            out.push_str(&format!("{id}\n"));
            let pretty = serde_json::to_string_pretty(ev).unwrap_or_default();
            for l in pretty.lines().take(30) {
                out.push_str(&format!("  {l}\n"));
            }
        }
    }
    let missing: Vec<&str> = list(&v, "unresolved").iter().filter_map(|x| x.as_str()).collect();
    if !missing.is_empty() {
        out.push_str(&format!("not found: {}", missing.join(", ")));
    }
    Ok(out.trim_end().to_string())
}

async fn attach(args: &str, ctx: &Ctx) -> Result<String, String> {
    let paths: Vec<std::path::PathBuf> = shell_words(args).iter().map(|p| user_path(p)).collect();
    if paths.is_empty() {
        return Err("usage: /attach <file>… (images, HAR, Burp exports, logs …)".into());
    }
    let c = client(ctx)?;
    let mut names = Vec::new();
    for p in paths {
        let bytes = std::fs::read(&p).map_err(|err| format!("{}: {err}", p.display()))?;
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        let mut fields = vec![("thread_id", ctx.thread_id.clone())];
        if let Some(w) = &ctx.profile.workspace_id {
            fields.push(("workspace_id", w.clone()));
        }
        let v = c.cli_upload("/cli/attachments/", &fields, &name, bytes).await.map_err(e)?;
        crate::pulse::queue_attachment(v);
        names.push(name);
    }
    Ok(format!("attached {} — sent with your next message", names.join(", ")))
}

/// Split on spaces, keeping quoted parts and `\ ` escapes together.
fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (_, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

async fn agent(args: &str, ctx: &Ctx) -> Result<Outcome, String> {
    let c = client(ctx)?;
    let v = c.cli_json("GET", "/cli/agents/", None).await.map_err(e)?;
    let agents = list(&v, "agents");
    let want = args.trim();
    if want.is_empty() {
        let mut out = String::from("Specialist agents — /agent <id> starts a chat with one");
        for a in &agents {
            out.push_str(&format!("\n  {} — {}", s(a, "id"), s(a, "description").chars().take(80).collect::<String>()));
        }
        return Ok(Outcome::Text(out));
    }
    let w = want.to_lowercase();
    let pick = agents
        .iter()
        .find(|a| s(a, "id") == want)
        .or_else(|| agents.iter().find(|a| s(a, "id").to_lowercase().contains(&w) || s(a, "name").to_lowercase().contains(&w)))
        .ok_or_else(|| format!("no agent matches '{want}' — /agent lists them"))?;
    let id = s(pick, "id");
    let thread = c
        .create_thread(s(pick, "name"), ctx.profile.workspace_id.as_deref(), Some(id))
        .await
        .map_err(e)?;
    Ok(Outcome::SwitchThread(thread, format!("new chat with {}", s(pick, "name"))))
}

async fn memory(sub: &str, rest: &str, ctx: &Ctx) -> Result<String, String> {
    let ws = ctx.workspace()?;
    let c = client(ctx)?;
    let base = format!("/cli/workspaces/{ws}/learnings/");
    match sub {
        "add" | "+" => {
            if rest.is_empty() {
                return Err("usage: /memory add <what the agent should remember>".into());
            }
            let v = c.cli_json("POST", &base, Some(json!({"content": rest}))).await.map_err(e)?;
            let id = v.get("id").or_else(|| v.pointer("/learning/id")).map(|x| x.to_string()).unwrap_or_default();
            Ok(format!("remembered for this workspace {}", id.trim_matches('"')))
        }
        "rm" | "delete" | "forget" => {
            if rest.is_empty() {
                return Err("usage: /memory rm <id>".into());
            }
            c.cli_json("DELETE", &format!("{base}{}/", rest.trim()), None).await.map_err(e)?;
            Ok(format!("forgot {}", rest.trim()))
        }
        _ => {
            let q = if sub == "search" { rest.to_string() } else { format!("{sub} {rest}").trim().to_string() };
            let path = if q.is_empty() { base.clone() } else { format!("{base}?q={}", urlencode(&q)) };
            let v = c.cli_json("GET", &path, None).await.map_err(e)?;
            let items = list(&v, "learnings");
            if items.is_empty() {
                return Ok(if q.is_empty() {
                    "Nothing remembered for this workspace yet. Start a message with # to add something.".into()
                } else {
                    format!("nothing matches '{q}'")
                });
            }
            let mut out = format!("Workspace memory ({})", items.len());
            for l in items.iter().take(25) {
                let text = if s(l, "title").is_empty() { s(l, "content") } else { s(l, "title") };
                out.push_str(&format!("\n  {}  {}", l.get("id").map(|x| x.to_string()).unwrap_or_default().trim_matches('"'), text.chars().take(100).collect::<String>()));
            }
            Ok(out)
        }
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn creds(sub: &str, rest: &str, ctx: &Ctx) -> Result<String, String> {
    let c = client(ctx)?;
    let path = format!("/cli/threads/{}/credentials/", ctx.thread_id);
    match sub {
        "" => Err("usage: /creds <NAME> KEY=VALUE…  (injected as NAME_KEY env vars) · /creds aws|github|jira <credential id>".into()),
        "aws" | "github" | "jira" => {
            let id = rest.trim();
            if id.is_empty() {
                return Err(format!("usage: /creds {sub} <credential id from Strobes>"));
            }
            c.cli_json("POST", &path, Some(json!({"kind": sub, "credential_id": id}))).await.map_err(e)?;
            Ok(format!("{sub} credential {id} is available to the agent in this chat"))
        }
        name => {
            let mut data = serde_json::Map::new();
            for pair in shell_words(rest) {
                let (k, v) = pair.split_once('=').ok_or_else(|| format!("'{pair}' isn't KEY=VALUE"))?;
                data.insert(k.to_string(), json!(v));
            }
            if data.is_empty() {
                return Err("usage: /creds <NAME> KEY=VALUE…".into());
            }
            let keys: Vec<String> = data.keys().map(|k| format!("{}_{}", name.to_uppercase(), k.to_uppercase())).collect();
            c.cli_json("POST", &path, Some(json!({"kind": "custom", "name": name, "data": data}))).await.map_err(e)?;
            Ok(format!("the agent can use {} in this chat (values aren't shown or saved in history)", keys.join(", ")))
        }
    }
}

async fn background(args: &str, ctx: &Ctx) -> Result<String, String> {
    if args.is_empty() {
        return Err("usage: /background <prompt> — runs in the cloud; check it with /runs".into());
    }
    let mut body = json!({"text": args});
    if let Some(m) = ctx.model {
        body["llm_model"] = json!(m);
    }
    let v = client(ctx)?
        .cli_json("POST", &format!("/cli/threads/{}/background/", ctx.thread_id), Some(body))
        .await
        .map_err(e)?;
    Ok(format!("running in the cloud (run {}) — you can close the terminal; /runs shows it", s(&v, "run_id")))
}

async fn runs(sub: &str, rest: &str, ctx: &Ctx) -> Result<String, String> {
    let c = client(ctx)?;
    if sub == "cancel" {
        let id = rest.trim();
        if id.is_empty() {
            return Err("usage: /runs cancel <run id>".into());
        }
        // /runs shows the first 8 characters; accept any unique prefix.
        let v = c.cli_json("GET", "/cli/runs/?active=true", None).await.map_err(e)?;
        let hits: Vec<String> = list(&v, "runs").iter().map(|r| s(r, "run_id").to_string()).filter(|r| r.starts_with(id)).collect();
        let full = match hits.len() {
            1 => hits[0].clone(),
            0 => id.to_string(),
            _ => return Err(format!("{id} matches {} runs — give more of the id", hits.len())),
        };
        c.cli_json("POST", &format!("/cli/runs/{full}/cancel/"), Some(json!({"stop_background": true}))).await.map_err(e)?;
        return Ok(format!("cancelled run {}", &full[..full.len().min(8)]));
    }
    let active = sub != "all";
    let mut q = vec![];
    if active {
        q.push("active=true".to_string());
    }
    if let Some(w) = &ctx.profile.workspace_id {
        q.push(format!("workspace_id={w}"));
    }
    let v = c.cli_json("GET", &format!("/cli/runs/?{}", q.join("&")), None).await.map_err(e)?;
    let items = list(&v, "runs");
    if items.is_empty() {
        return Ok(if active { "No runs in progress. /runs all shows recent ones.".into() } else { "No runs yet.".into() });
    }
    let mut out = format!("{} runs", if active { "Active" } else { "Recent" });
    for r in items.iter().take(25) {
        out.push_str(&format!(
            "\n  {}  {:<10} {}  {}",
            s(r, "run_id").chars().take(8).collect::<String>(),
            s(r, "status"),
            s(r, "started").chars().take(16).collect::<String>().replace('T', " "),
            s(r, "title").chars().take(60).collect::<String>()
        ));
    }
    if active {
        out.push_str("\n/runs cancel <id> stops one");
    }
    Ok(out)
}

async fn schedule(sub: &str, rest: &str, ctx: &Ctx) -> Result<String, String> {
    let c = client(ctx)?;
    match sub {
        "" | "list" => {
            let q = ctx.profile.workspace_id.as_deref().map(|w| format!("?workspace_id={w}")).unwrap_or_default();
            let v = c.cli_json("GET", &format!("/cli/schedules/{q}"), None).await.map_err(e)?;
            let items = list(&v, "schedules");
            if items.is_empty() {
                return Ok("No schedules. /schedule add daily 09:00 <prompt> runs a prompt every day.".into());
            }
            let mut out = String::from("Schedules");
            for sch in items {
                out.push_str(&format!(
                    "\n  {}  {} {} · {} · {}",
                    s(sch, "id").chars().take(8).collect::<String>(),
                    s(sch, "frequency"),
                    s(sch, "time"),
                    s(sch, "status"),
                    s(sch, "name")
                ));
            }
            Ok(out)
        }
        "add" | "new" => {
            let mut it = rest.splitn(3, char::is_whitespace);
            let (freq, time, prompt) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or("").trim());
            if freq.is_empty() || !time.contains(':') || prompt.is_empty() {
                return Err("usage: /schedule add <hourly|daily|weekly|monthly|quarterly> <HH:MM> <prompt>".into());
            }
            let mut body = json!({"name": prompt.chars().take(60).collect::<String>(), "prompt": prompt, "frequency": freq, "time": time});
            if let Some(w) = &ctx.profile.workspace_id {
                body["workspace_id"] = json!(w);
            }
            let v = c.cli_json("POST", "/cli/schedules/", Some(body)).await.map_err(e)?;
            let id = v.get("id").or_else(|| v.pointer("/schedule/id")).map(|x| x.to_string()).unwrap_or_default();
            Ok(format!("scheduled {freq} at {time} ({})", id.trim_matches('"')))
        }
        "pause" | "resume" | "run" | "trigger" => {
            let id = full_id(&c, rest, ctx).await?;
            let action = if sub == "run" { "trigger" } else { sub };
            c.cli_json("POST", &format!("/cli/schedules/{id}/{action}/"), None).await.map_err(e)?;
            Ok(format!("{} {}", match action { "trigger" => "started", "pause" => "paused", _ => "resumed" }, rest.trim()))
        }
        "rm" | "delete" => {
            let id = full_id(&c, rest, ctx).await?;
            c.cli_json("DELETE", &format!("/cli/schedules/{id}/"), None).await.map_err(e)?;
            Ok(format!("deleted schedule {}", rest.trim()))
        }
        other => Err(format!("unknown /schedule {other} — try list, add, pause, resume, run, rm")),
    }
}

/// A schedule id from its first characters (as /schedule lists them).
async fn full_id(c: &api::ApiClient, prefix: &str, ctx: &Ctx) -> Result<String, String> {
    let p = prefix.trim();
    if p.is_empty() {
        return Err("which schedule? give its id (from /schedule)".into());
    }
    let q = ctx.profile.workspace_id.as_deref().map(|w| format!("?workspace_id={w}")).unwrap_or_default();
    let v = c.cli_json("GET", &format!("/cli/schedules/{q}"), None).await.map_err(e)?;
    let hits: Vec<String> = list(&v, "schedules").iter().map(|x| s(x, "id").to_string()).filter(|id| id.starts_with(p)).collect();
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => Err(format!("no schedule {p}")),
        _ => Err(format!("{p} matches {} schedules — give more of the id", hits.len())),
    }
}

async fn report(sub: &str, rest: &str, ctx: &Ctx) -> Result<String, String> {
    let ws = ctx.workspace()?;
    let c = client(ctx)?;
    let path = format!("/cli/workspaces/{ws}/report/");
    match sub {
        "new" | "generate" => {
            let kind = if rest.is_empty() { "CSV".to_string() } else { rest.trim().to_uppercase() };
            c.cli_json("POST", &path, Some(json!({"kind": kind}))).await.map_err(e)?;
            Ok(format!("generating a {kind} report — /report shows it when it's finished"))
        }
        "" | "list" | "get" => {
            let v = c.cli_json("GET", &path, None).await.map_err(e)?;
            let items = list(&v, "reports");
            if sub == "get" {
                let want = rest.trim().to_lowercase();
                let r = items
                    .iter()
                    .find(|r| s(r, "name").to_lowercase().contains(&want) || r.get("id").map(|i| i.to_string().trim_matches('"').to_string()) == Some(want.clone()))
                    .ok_or_else(|| format!("no report matches '{}'", rest.trim()))?;
                let url = s(r, "url");
                if url.is_empty() {
                    return Err(format!("{} isn't ready yet ({})", s(r, "name"), s(r, "status")));
                }
                crate::open_browser(url);
                return Ok(format!("opening {}", s(r, "name")));
            }
            if items.is_empty() {
                return Ok("No reports yet. /report new [csv|threat|cve] generates one.".into());
            }
            let mut out = String::from("Reports");
            for r in items.iter().take(20) {
                out.push_str(&format!("\n  {} · {} · {}", s(r, "name"), s(r, "status"), s(r, "created").chars().take(10).collect::<String>()));
            }
            out.push_str("\n/report get <name> opens one");
            Ok(out)
        }
        other => Err(format!("unknown /report {other} — try list, new, get")),
    }
}

async fn fork(args: &str, ctx: &Ctx) -> Result<Outcome, String> {
    let mut body = json!({});
    if !args.is_empty() {
        body["title"] = json!(args);
    }
    let v = client(ctx)?
        .cli_json("POST", &format!("/cli/threads/{}/fork/", ctx.thread_id), Some(body))
        .await
        .map_err(e)?;
    let id = s(&v, "thread_id").to_string();
    if id.is_empty() {
        return Err("the server didn't return the new chat".into());
    }
    Ok(Outcome::SwitchThread(id, "forked — you're in the copy; the original is unchanged (/resume)".into()))
}

async fn share(ctx: &Ctx) -> Result<String, String> {
    let v = client(ctx)?
        .cli_json("POST", &format!("/cli/threads/{}/share/", ctx.thread_id), None)
        .await
        .map_err(e)?;
    let url = s(&v, "url").to_string();
    let copied = crate::copy_to_clipboard(&url).is_ok();
    Ok(format!("{url}{}\nTeammates with access to this workspace can open it.", if copied { "  (copied)" } else { "" }))
}

#[allow(dead_code)]
fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}
