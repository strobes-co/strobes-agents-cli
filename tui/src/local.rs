//! Local execution — the user's machine is the agent's sandbox.
//!
//! Handles the CLI_LOCAL proxied tools the backend asks the client to run:
//! `execute_command`, `execute_code`, and `workspace_get_meta`. Browser tools
//! are reported as unsupported by this TUI build (shell sandbox is the core).

use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

/// Result of a local tool execution, shaped for the `tool.local_result` /
/// `tool.local_error` reply payload consumed by the cloud LocalProxyTool.
pub struct LocalResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

pub fn sandbox_dir() -> PathBuf {
    if let Ok(d) = std::env::var("STROBES_AI_SANDBOX") {
        return PathBuf::from(d);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".strobes-ai").join("sandbox")
}

fn ensure_sandbox() -> PathBuf {
    let dir = sandbox_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Route a proxied tool call to the right local executor.
pub async fn run_tool(tool_name: &str, input: &serde_json::Value) -> LocalResult {
    match tool_name {
        "execute_command" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            run_shell(cmd).await
        }
        "execute_code" => {
            let code = input.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let lang = input
                .get("language")
                .and_then(|v| v.as_str())
                .unwrap_or("python");
            run_code(code, lang).await
        }
        "workspace_get_meta" => LocalResult {
            output: meta_json(),
            exit_code: Some(0),
            error: None,
        },
        b if b.starts_with("browser_") => match crate::browser::handle(b, input).await {
            Ok(output) => LocalResult { output, exit_code: Some(0), error: None },
            Err(e) => LocalResult { output: String::new(), exit_code: None, error: Some(e) },
        },
        other => LocalResult {
            output: String::new(),
            exit_code: None,
            error: Some(format!("unsupported local tool in TUI build: {other}")),
        },
    }
}

async fn run_shell(command: &str) -> LocalResult {
    let dir = ensure_sandbox();
    let out = Command::new("/bin/bash")
        .arg("-lc")
        .arg(command)
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    finish(out)
}

async fn run_code(code: &str, lang: &str) -> LocalResult {
    let (program, args, suffix): (&str, Vec<&str>, &str) = match lang.to_lowercase().as_str() {
        "python" | "python3" | "py" => ("python3", vec![], ".py"),
        "bash" | "sh" | "shell" => ("bash", vec![], ".sh"),
        "javascript" | "js" | "node" => ("node", vec![], ".js"),
        "ruby" => ("ruby", vec![], ".rb"),
        _ => {
            return LocalResult {
                output: String::new(),
                exit_code: Some(127),
                error: Some(format!("unsupported language: {lang}")),
            }
        }
    };
    let dir = ensure_sandbox();
    let file = dir.join(format!("snippet-{}{}", uuid::Uuid::new_v4().simple(), suffix));
    if let Err(e) = tokio::fs::write(&file, code).await {
        return LocalResult {
            output: String::new(),
            exit_code: Some(1),
            error: Some(format!("write temp failed: {e}")),
        };
    }
    let out = Command::new(program)
        .args(&args)
        .arg(&file)
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    let _ = tokio::fs::remove_file(&file).await;
    finish(out)
}

fn finish(out: std::io::Result<std::process::Output>) -> LocalResult {
    match out {
        Ok(o) => {
            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&o.stdout));
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            LocalResult {
                output: text.trim_end().to_string(),
                exit_code: o.status.code(),
                error: None,
            }
        }
        Err(e) => LocalResult {
            output: String::new(),
            exit_code: Some(127),
            error: Some(e.to_string()),
        },
    }
}

fn meta_json() -> String {
    let dir = sandbox_dir();
    let file_count = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    let mut meta = serde_json::json!({
        "working_directory": dir.to_string_lossy(),
        "platform": std::env::consts::OS,
        "shell": std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()),
        "user": std::env::var("USER").unwrap_or_default(),
        "entry_count": file_count,
        "note": "Files synced locally from the remote workspace. Use shell (ls/find/cat) to inspect.",
    });
    if let Ok(ws) = std::env::var("STROBES_AI_WORKSPACE_ID") {
        meta["workspace_id"] = serde_json::Value::from(ws);
    }
    meta.to_string()
}
