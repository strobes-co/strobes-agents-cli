//! `strobes mcp serve` — Strobes as an MCP server (stdio), so Claude Code,
//! Cursor and other MCP clients can use it as tools with this CLI's login:
//!
//! ```sh
//! claude mcp add strobes -- strobes mcp serve
//! ```
//!
//! Tools: workspaces, findings, threads, ask (runs the agent and returns its
//! answer) and a read-only API passthrough.

use crate::{api, config};
use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn tools() -> Value {
    json!([
        {
            "name": "list_workspaces",
            "description": "List the Strobes workspaces (engagements) in this organization.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "list_findings",
            "description": "List the security findings of a Strobes workspace.",
            "inputSchema": {"type": "object", "properties": {
                "workspace_id": {"type": "string", "description": "Workspace id (from list_workspaces); defaults to the bound workspace"}
            }}
        },
        {
            "name": "list_threads",
            "description": "List recent Strobes agent chats, optionally for one workspace.",
            "inputSchema": {"type": "object", "properties": {"workspace_id": {"type": "string"}}}
        },
        {
            "name": "ask",
            "description": "Ask the Strobes security agent to do something (assess a target, triage findings, explain an issue) and return its final answer. Can take minutes.",
            "inputSchema": {"type": "object", "properties": {
                "prompt": {"type": "string"},
                "workspace_id": {"type": "string", "description": "Workspace to work in; defaults to the bound one"}
            }, "required": ["prompt"]}
        },
        {
            "name": "api_get",
            "description": "GET a Strobes API path (org-relative, e.g. /cli/workspaces/) and return the JSON.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
        }
    ])
}

async fn call(profile: &config::Profile, tenant: &str, name: &str, args: &Value) -> Result<String, String> {
    let client = api::ApiClient::new(profile.clone()).map_err(|e| e.to_string())?;
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string).filter(|v| !v.is_empty());
    let ws = || s("workspace_id").or_else(|| profile.workspace_id.clone());
    match name {
        "list_workspaces" => client.list_workspaces().await.map(|w| pj(&w)).map_err(|e| e.to_string()),
        "list_findings" => {
            let ws = ws().ok_or("workspace_id is required (no workspace is bound)")?;
            client.list_workspace_findings(&ws).await.map(|f| pj(&f)).map_err(|e| e.to_string())
        }
        "list_threads" => client.list_threads(ws().as_deref()).await.map(|t| pj(&t)).map_err(|e| e.to_string()),
        "api_get" => {
            let path = s("path").ok_or("path is required")?;
            let url = client.resolve_api_path(&path).map_err(|e| e.to_string())?;
            let (status, body) = client.raw("GET", &url, None).await.map_err(|e| e.to_string())?;
            if (200..300).contains(&status) { Ok(body) } else { Err(format!("HTTP {status}: {}", body.chars().take(300).collect::<String>())) }
        }
        "ask" => {
            let prompt = s("prompt").ok_or("prompt is required")?;
            // The agent run is the CLI's own print mode, in a child process so
            // its output never mixes with this server's stdout.
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let mut cmd = tokio::process::Command::new(exe);
            cmd.arg("--tenant").arg(tenant);
            if let Some(w) = ws() {
                cmd.arg("-w").arg(w);
            }
            cmd.arg("-p").arg(&prompt).stdin(std::process::Stdio::null()).kill_on_drop(true);
            let out = cmd.output().await.map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() {
                Ok(text)
            } else {
                Err(format!("{text}\n{}", String::from_utf8_lossy(&out.stderr).trim()))
            }
        }
        other => Err(format!("unknown tool {other}")),
    }
}

fn pj<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

pub async fn serve(profile: config::Profile, tenant: String) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply = match method {
            "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "strobes", "version": env!("CARGO_PKG_VERSION")}
            }}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
            "tools/call" => {
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let (text, err) = match call(&profile, &tenant, name, &args).await {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": text}], "isError": err}})
            }
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}}),
        };
        out.write_all(format!("{reply}\n").as_bytes()).await?;
        out.flush().await?;
    }
    Ok(())
}
