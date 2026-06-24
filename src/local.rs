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
    pub captured_network: Vec<serde_json::Value>,
}

/// Return the sandbox directory for a given thread.
///
/// When a `thread_id` is present (injected by pulse.rs) each thread gets its
/// own isolated working directory under `~/.strobes-ai/sandboxes/<thread_id>/`.
/// This ensures workflow tasks can't clobber each other's files.
/// Falls back to the global `STROBES_AI_SANDBOX` env override or the legacy
/// single `~/.strobes-ai/sandbox/` path when no thread_id is available.
pub fn sandbox_dir_for(thread_id: Option<&str>) -> PathBuf {
    if let Ok(d) = std::env::var("STROBES_AI_SANDBOX") {
        return PathBuf::from(d);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    if let Some(tid) = thread_id {
        let safe: String = tid
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        home.join(".strobes-ai").join("sandboxes").join(safe)
    } else {
        home.join(".strobes-ai").join("sandbox")
    }
}

/// Convenience wrapper — used by callers that don't have a thread_id.
pub fn sandbox_dir() -> PathBuf {
    sandbox_dir_for(None)
}


/// Route a proxied tool call to the right local executor.
pub async fn run_tool(tool_name: &str, input: &serde_json::Value) -> LocalResult {
    // Each task runs in its own thread; the thread_id is injected by pulse.rs.
    // We use it to key an isolated sandbox directory per task.
    let thread_id = input.get("_thread_id").and_then(|v| v.as_str());
    let sandbox = sandbox_dir_for(thread_id);

    match tool_name {
        "execute_command" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            run_shell(cmd, &sandbox).await
        }
        "execute_code" => {
            let code = input.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let lang = input
                .get("language")
                .and_then(|v| v.as_str())
                .unwrap_or("python");
            run_code(code, lang, &sandbox).await
        }
        "workspace_get_meta" => LocalResult {
            output: meta_json(&sandbox),
            exit_code: Some(0),
            error: None,
            captured_network: vec![],
        },
        "todo_write" => todo_write(input, &sandbox),
        "todo_read" => todo_read(&sandbox),
        b if b.starts_with("browser_") => match crate::browser::handle(b, input).await {
            Ok((output, captured_network)) => LocalResult {
                output,
                exit_code: Some(0),
                error: None,
                captured_network,
            },
            Err(e) => LocalResult {
                output: String::new(),
                exit_code: None,
                error: Some(e),
                captured_network: vec![],
            },
        },
        other => LocalResult {
            output: String::new(),
            exit_code: None,
            error: Some(format!("unsupported local tool in TUI build: {other}")),
            captured_network: vec![],
        },
    }
}

async fn run_shell(command: &str, sandbox: &std::path::Path) -> LocalResult {
    let _ = std::fs::create_dir_all(sandbox);
    // Use the native shell: cmd.exe on Windows, login bash elsewhere.
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("/bin/bash");
        c.arg("-lc").arg(command);
        c
    };
    let out = cmd
        .current_dir(sandbox)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    finish(out)
}

async fn run_code(code: &str, lang: &str, sandbox: &std::path::Path) -> LocalResult {
    // The Python launcher is `python` on Windows, `python3` elsewhere.
    let python = if cfg!(windows) { "python" } else { "python3" };
    let (program, args, suffix): (&str, Vec<&str>, &str) = match lang.to_lowercase().as_str() {
        "python" | "python3" | "py" => (python, vec![], ".py"),
        "bash" | "sh" | "shell" => ("bash", vec![], ".sh"),
        "javascript" | "js" | "node" => ("node", vec![], ".js"),
        "ruby" => ("ruby", vec![], ".rb"),
        _ => {
            return LocalResult {
                output: String::new(),
                exit_code: Some(127),
                error: Some(format!("unsupported language: {lang}")),
                captured_network: vec![],
            }
        }
    };
    let _ = std::fs::create_dir_all(sandbox);
    let file = sandbox.join(format!("snippet-{}{}", uuid::Uuid::new_v4().simple(), suffix));
    if let Err(e) = tokio::fs::write(&file, code).await {
        return LocalResult {
            output: String::new(),
            exit_code: Some(1),
            error: Some(format!("write temp failed: {e}")),
            captured_network: vec![],
        };
    }
    let out = Command::new(program)
        .args(&args)
        .arg(&file)
        .current_dir(sandbox)
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
                captured_network: vec![],
            }
        }
        Err(e) => LocalResult {
            output: String::new(),
            exit_code: Some(127),
            error: Some(e.to_string()),
            captured_network: vec![],
        },
    }
}

/// Persist the agent's todo list to `<sandbox>/todos.json`.
/// Input: `{ "todos": [{ "id", "content", "status", "priority" }] }`
fn todo_write(input: &serde_json::Value, sandbox: &std::path::Path) -> LocalResult {
    let todos = input.get("todos").cloned().unwrap_or(serde_json::json!([]));
    let _ = std::fs::create_dir_all(sandbox);
    let path = sandbox.join("todos.json");
    match serde_json::to_string_pretty(&todos) {
        Ok(json) => match std::fs::write(&path, &json) {
            Ok(_) => LocalResult {
                output: json,
                exit_code: Some(0),
                error: None,
                captured_network: vec![],
            },
            Err(e) => LocalResult {
                output: String::new(),
                exit_code: Some(1),
                error: Some(format!("todo_write: {e}")),
                captured_network: vec![],
            },
        },
        Err(e) => LocalResult {
            output: String::new(),
            exit_code: Some(1),
            error: Some(format!("todo_write serialize: {e}")),
            captured_network: vec![],
        },
    }
}

/// Read the agent's persisted todo list from `<sandbox>/todos.json`.
fn todo_read(sandbox: &std::path::Path) -> LocalResult {
    let path = sandbox.join("todos.json");
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| "[]".to_string());
    LocalResult {
        output: content,
        exit_code: Some(0),
        error: None,
        captured_network: vec![],
    }
}

fn meta_json(sandbox: &std::path::Path) -> String {
    let file_count = std::fs::read_dir(sandbox).map(|d| d.count()).unwrap_or(0);
    // Shell + user vary by platform (Windows has no SHELL/USER).
    let (shell, user) = if cfg!(windows) {
        (
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
            std::env::var("USERNAME").unwrap_or_default(),
        )
    } else {
        (
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()),
            std::env::var("USER").unwrap_or_default(),
        )
    };
    let mut meta = serde_json::json!({
        "working_directory": sandbox.to_string_lossy(),
        "platform": std::env::consts::OS,
        "shell": shell,
        "user": user,
        "entry_count": file_count,
        "note": "Isolated sandbox for this task. Files here are private to this thread.",
    });
    if let Ok(ws) = std::env::var("STROBES_AI_WORKSPACE_ID") {
        meta["workspace_id"] = serde_json::Value::from(ws);
    }
    meta.to_string()
}
