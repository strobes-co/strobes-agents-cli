//! Chat commands that take arguments and do their work off the UI thread:
//! `/name args` → `run(name, args, ctx)` → text (or an action) for the chat.

use crate::{api, config, mcp, project};
use serde_json::Value;

/// Commands handled here (they also appear in `/` and `/help`).
pub const COMMANDS: &[(&str, &str)] = &[
    ("init", "Create a STROBES.md with scope and rules for this project"),
    ("mcp", "MCP servers: status · trust · reload"),
    ("skills", "Skills: list · show <slug> · push <dir> · enable|disable <slug>"),
];

pub fn is_command(name: &str) -> bool {
    COMMANDS.iter().any(|(c, _)| *c == name)
}

/// What the chat should do with a command's result.
pub enum Outcome {
    Text(String),
}

pub struct Ctx {
    pub profile: config::Profile,
    pub thread_id: String,
}

pub async fn run(name: &str, args: &str, ctx: &Ctx) -> Result<Outcome, String> {
    let args = args.trim();
    let (sub, rest) = args.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((args, ""));
    match name {
        "init" => init().map(Outcome::Text),
        "mcp" => mcp_cmd(sub).await.map(Outcome::Text),
        "skills" => skills(sub, rest, ctx).await.map(Outcome::Text),
        _ => Err(format!("unknown command /{name}")),
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
            let local = project::local_skills();
            let mut out = String::new();
            if local.is_empty() {
                out.push_str(&format!(
                    "No local skills. Put one in {}/<slug>/SKILL.md (or {}).",
                    short_path(&project::root().join(".strobes/skills")),
                    short_path(&config::config_dir().join("skills"))
                ));
            } else {
                out.push_str("Local skills (the agent loads them as local:<slug>)");
                for s in &local {
                    out.push_str(&format!("\n  local:{} — {}", s.slug, s.description));
                }
            }
            let _ = ctx;
            Ok(out)
        }
        "show" => {
            let text = project::load_skill(rest)?;
            Ok(text.lines().take(40).collect::<Vec<_>>().join("\n"))
        }
        other => Err(format!("unknown /skills {other}")),
    }
}

#[allow(dead_code)]
fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}
