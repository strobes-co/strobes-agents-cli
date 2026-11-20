//! Hooks: the user's scripts around every tool call the agent runs on this
//! machine, in Claude Code's `hooks.json` format:
//!
//! ```json
//! { "hooks": {
//!     "PreToolUse":  [{ "matcher": "Shell|Run code", "hooks": [{ "type": "command", "command": "./scope-guard.sh" }] }],
//!     "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "tee -a ~/strobes-audit.jsonl" }] }]
//! } }
//! ```
//!
//! The hook gets the call as JSON on stdin (`hook_event_name`, `tool_name`,
//! `tool_input`, `cwd`, `thread_id`, plus `tool_response` after the call).
//! A PreToolUse hook that exits 2 blocks the call; its stderr goes back to the
//! agent as the reason. Other exit codes let it run.
//!
//! Read from the user's config dir (`hooks.json`) and the project
//! (`.strobes/hooks.json`). Project hooks run only in a trusted project (`/mcp
//! trust`), since they run commands from the repo.

use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct Hook {
    matcher: String,
    command: String,
}

/// Names a matcher can use for a tool, besides its wire name.
fn aliases(tool: &str) -> Vec<&str> {
    let mut v = vec![tool];
    match tool {
        "execute_command" => v.extend(["Shell", "Bash"]),
        "execute_code" => v.extend(["Run code", "Code", "Python"]),
        "load_local_skill" => v.push("Skill"),
        t if t.starts_with("mcp__") => v.push("MCP"),
        _ => {}
    }
    v
}

fn matches(matcher: &str, tool: &str) -> bool {
    let m = matcher.trim();
    if m.is_empty() || m == "*" {
        return true;
    }
    let names = aliases(tool);
    m.split('|').map(str::trim).any(|pat| {
        glob::Pattern::new(pat).map(|p| names.iter().any(|n| p.matches(n))).unwrap_or(false)
    })
}

fn load(event: &str) -> Vec<Hook> {
    let mut files = vec![(crate::config::config_dir().join("hooks.json"), false)];
    files.push((crate::project::root().join(".strobes").join("hooks.json"), true));
    let trusted = crate::mcp::is_trusted();
    let mut out = Vec::new();
    for (file, project) in files {
        if project && !trusted {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            eprintln!("strobes: {} is not valid JSON", file.display());
            continue;
        };
        for group in v.pointer(&format!("/hooks/{event}")).and_then(|g| g.as_array()).cloned().unwrap_or_default() {
            let matcher = group.get("matcher").and_then(|m| m.as_str()).unwrap_or("*").to_string();
            for h in group.get("hooks").and_then(|h| h.as_array()).cloned().unwrap_or_default() {
                if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                    out.push(Hook { matcher: matcher.clone(), command: cmd.to_string() });
                }
            }
        }
    }
    out
}

/// Rows for `/hooks`: (event, matcher, command).
pub fn list() -> Vec<(String, String, String)> {
    ["PreToolUse", "PostToolUse"]
        .iter()
        .flat_map(|e| load(e).into_iter().map(move |h| (e.to_string(), h.matcher, h.command)))
        .collect()
}

/// Run one hook; (exit code, stderr).
async fn run(hook: &Hook, payload: &Value) -> (i32, String) {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&hook.command)
        .current_dir(crate::project::root())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else { return (1, format!("could not run hook `{}`", hook.command)) };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.to_string().as_bytes()).await;
    }
    match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => (out.status.code().unwrap_or(1), String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Ok(Err(e)) => (1, e.to_string()),
        Err(_) => (1, format!("hook `{}` timed out", hook.command)),
    }
}

fn payload(event: &str, tool: &str, input: &Value) -> Value {
    let mut clean = input.clone();
    if let Some(o) = clean.as_object_mut() {
        o.retain(|k, _| !k.starts_with('_'));
    }
    json!({
        "hook_event_name": event,
        "tool_name": tool,
        "tool_input": clean,
        "cwd": crate::project::root(),
        "thread_id": input.get("_thread_id"),
    })
}

/// Before a call: `Err(reason)` when a hook blocks it.
pub async fn pre_tool(tool: &str, input: &Value) -> Result<(), String> {
    let hooks: Vec<Hook> = load("PreToolUse").into_iter().filter(|h| matches(&h.matcher, tool)).collect();
    if hooks.is_empty() {
        return Ok(());
    }
    let p = payload("PreToolUse", tool, input);
    for h in &hooks {
        let (code, stderr) = run(h, &p).await;
        if code == 2 {
            let why = if stderr.is_empty() { format!("blocked by hook `{}`", h.command) } else { stderr };
            return Err(format!("Blocked by the user's PreToolUse hook: {why}"));
        }
    }
    Ok(())
}

/// After a call (fire and forget — the result is already decided).
pub async fn post_tool(tool: &str, input: &Value, output: &str, error: Option<&str>) {
    let hooks: Vec<Hook> = load("PostToolUse").into_iter().filter(|h| matches(&h.matcher, tool)).collect();
    if hooks.is_empty() {
        return;
    }
    let mut p = payload("PostToolUse", tool, input);
    p["tool_response"] = json!({"output": output, "error": error});
    for h in &hooks {
        let _ = run(h, &p).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matchers() {
        assert!(matches("*", "execute_code"));
        assert!(matches("Shell|Run code", "execute_command"));
        assert!(matches("Run code", "execute_code"));
        assert!(matches("mcp__burp__*", "mcp__burp__send"));
        assert!(matches("MCP", "mcp__x__y"));
        assert!(!matches("Shell", "execute_code"));
    }

    #[tokio::test]
    async fn exit_2_blocks_with_stderr() {
        let p = payload("PreToolUse", "execute_command", &json!({"command": "nmap prod", "_thread_id": "t"}));
        assert_eq!(p["tool_input"], json!({"command": "nmap prod"}));
        let block = Hook { matcher: "*".into(), command: "grep -q prod && { echo 'prod is out of scope' >&2; exit 2; } || exit 0".into() };
        assert_eq!(run(&block, &p).await, (2, "prod is out of scope".into()));
        let ok = payload("PreToolUse", "execute_command", &json!({"command": "nmap staging"}));
        assert_eq!(run(&block, &ok).await.0, 0);
    }
}
