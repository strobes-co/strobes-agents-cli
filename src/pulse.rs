//! Pulse chat WebSocket client.
//!
//! Connects to `ws/<org>/pulse/<thread>/?api_key=` and speaks the PulseConsumer
//! protocol. The consumer forwards FLAT StreamEvents (top-level `type`, fields
//! in `data` for ephemeral events or `payload` for persisted ones) — there is
//! no `pulse_event` wrapper on the client side. When the session is CLI_LOCAL
//! (`context.client_type == "cli"`), the backend emits `tool.local_execute`
//! events instead of running code in the cloud; we run them locally and reply
//! with `tool.local_result`, making this machine the sandbox.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

use crate::config::Profile;
use crate::local;

/// A normalized, render-ready item derived from a StreamEvent.
#[derive(Debug, Clone)]
pub struct StreamItem {
    pub kind: String, // token | thinking | tool_start | tool_output | tool_failed | task | system | approval | interrupt | note
    pub agent: Option<String>,
    pub text: Option<String>,
    pub tool_name: Option<String>,
    pub detail: Option<String>,
    pub status: Option<String>,
    pub local: bool,
    pub task_id: Option<String>,
}

/// A single form field requested by `request_human_input`.
#[derive(Debug, Clone)]
pub struct Field {
    pub key: String,
    pub label: String,
    pub ftype: String, // text | password | number | select | textarea | checkbox | ...
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    Connected,
    Disconnected(String),
    Stream(StreamItem),
    RunStarted,
    RunFinished(String), // "completed" | "failed: ..."
    LocalToolDone { name: String, ms: u128, exit: Option<i32>, err: Option<String> },
    /// The agent called `request_human_input` and is blocked awaiting a reply.
    Interrupt { id: String, title: String, message: String, fields: Vec<Field> },
    /// A non-error informational line (e.g. background workspace sync progress).
    Notice(String),
    /// Credit/token usage. `final_run=false` is a live per-call delta
    /// (`credit.update`); `final_run=true` is the authoritative run total
    /// (from `run.completed` metrics).
    Credits { credits: f64, tokens: i64, final_run: bool },
    Error(String),
}

/// Handle used by the UI to send frames to the server. Dropping it stops the
/// connection's reconnect supervisor (so a thread switch / chat exit is clean).
pub struct PulseHandle {
    out: mpsc::UnboundedSender<String>,
    workspace_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Mutable so the user can change the model mid-chat via the model picker.
    llm_model: std::sync::Arc<std::sync::Mutex<Option<i64>>>,
    stop: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
}

impl Drop for PulseHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.stop_notify.notify_waiters();
    }
}

impl PulseHandle {
    /// Bind/rebind the workspace used in the send context (mid-session).
    pub fn set_workspace(&self, id: Option<String>) {
        if let Ok(mut w) = self.workspace_id.lock() {
            *w = id;
        }
    }

    /// Change the AI model used for subsequent messages (live, no reconnect needed).
    pub fn set_model(&self, model: Option<i64>) {
        if let Ok(mut m) = self.llm_model.lock() {
            *m = model;
        }
    }

    pub fn send_user_message(&self, text: &str) {
        let mut ctx = json!({ "client_type": "cli" });
        if let Some(ws) = self.workspace_id.lock().ok().and_then(|w| w.clone()) {
            ctx["workspace_id"] = json!(ws);
        }
        if let Some(m) = self.llm_model.lock().ok().and_then(|m| *m) {
            ctx["llm_model"] = json!(m);
        }
        let frame = json!({ "type": "send_message", "text": text, "context": ctx });
        let _ = self.out.send(frame.to_string());
    }

    pub fn cancel(&self) {
        let _ = self.out.send(json!({ "type": "run.cancel" }).to_string());
    }

    /// Answer a `request_human_input` interrupt.
    pub fn respond_interrupt(&self, interrupt_id: &str, response: Value) {
        let frame = json!({
            "type": "interrupt.response",
            "interrupt_id": interrupt_id,
            "response_data": response,
        });
        let _ = self.out.send(frame.to_string());
    }
}

/// Connect and spawn the read/write tasks. Returns a handle for the UI.
/// Open the pulse WebSocket. For a plain `ws://` URL with a dev resolve
/// override active, dial the static IP directly (keeping the `Host` header from
/// the URL) so we skip the slow `.local`/mDNS lookup. Otherwise use the normal
/// resolver. `wss://` always uses the standard path.
async fn dial_ws(
    url: &str,
    profile: &Profile,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>
{
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let parsed = url::Url::parse(url)?;
    let is_tls = parsed.scheme() == "wss";
    if !is_tls {
        if let Some(ip) = profile.resolve_override() {
            let port = parsed.port().unwrap_or(80);
            let tcp = tokio::net::TcpStream::connect((ip, port)).await?;
            let req = url.into_client_request()?; // preserves Host + api_key query
            let (ws, _resp) =
                tokio_tungstenite::client_async(req, tokio_tungstenite::MaybeTlsStream::Plain(tcp))
                    .await?;
            return Ok(ws);
        }
    }
    let (ws, _resp) = tokio_tungstenite::connect_async(url).await?;
    Ok(ws)
}

pub async fn connect(
    profile: &Profile,
    thread_id: &str,
    app_tx: mpsc::UnboundedSender<AppEvent>,
    llm_model: Option<i64>,
) -> Result<PulseHandle> {
    let url = profile.pulse_ws_url(thread_id)?;
    // The first connection must succeed so the caller gets a live handle.
    let ws = dial_ws(&url, profile).await?;

    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_notify = Arc::new(Notify::new());

    // Supervisor: run the session, and on any drop reconnect with backoff —
    // forever, until the handle is dropped (stop set). Keeps the same out_tx /
    // app_tx so the UI and send path survive reconnects transparently.
    {
        let profile = profile.clone();
        let app_tx = app_tx.clone();
        let out_tx = out_tx.clone();
        let stop = stop.clone();
        let stop_notify = stop_notify.clone();
        let thread_id_owned = thread_id.to_string();
        tokio::spawn(async move {
            let mut out_rx = out_rx;
            let mut next = Some(ws);
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let sock = match next.take() {
                    Some(s) => s,
                    None => break,
                };
                run_session(sock, &mut out_rx, &out_tx, &app_tx, &stop, &stop_notify, profile.workspace_id.as_deref(), &thread_id_owned).await;
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // Reconnect with capped exponential backoff.
                let mut delay = 1u64;
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let _ = app_tx.send(AppEvent::Disconnected(format!("reconnecting in {delay}s…")));
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
                        _ = stop_notify.notified() => return,
                    }
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let _ = app_tx.send(AppEvent::Disconnected("reconnecting…".into()));
                    match dial_ws(&url, &profile).await {
                        Ok(s) => {
                            next = Some(s);
                            break;
                        }
                        Err(e) => {
                            let _ = app_tx.send(AppEvent::Disconnected(format!("reconnect failed: {e}")));
                            delay = (delay * 2).min(20);
                        }
                    }
                }
            }
        });
    }

    Ok(PulseHandle {
        out: out_tx,
        workspace_id: std::sync::Arc::new(std::sync::Mutex::new(profile.workspace_id.clone())),
        llm_model: std::sync::Arc::new(std::sync::Mutex::new(llm_model)),
        stop,
        stop_notify,
    })
}

/// Run one connected WebSocket session: drain outgoing frames, read incoming
/// frames, ping periodically. Returns when the socket closes/errors or `stop`
/// is signalled. Leaves `out_rx` intact for the next reconnect.
async fn run_session(
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    out_rx: &mut mpsc::UnboundedReceiver<String>,
    out_tx: &mpsc::UnboundedSender<String>,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
    stop: &Arc<AtomicBool>,
    stop_notify: &Arc<Notify>,
    workspace_id: Option<&str>,
    thread_id: &str,
) {
    let (mut write, mut read) = ws.split();
    let _ = app_tx.send(AppEvent::Connected);
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = stop_notify.notified() => return,
            frame = out_rx.recv() => match frame {
                Some(f) => {
                    if write.send(Message::Text(f)).await.is_err() {
                        return;
                    }
                }
                None => return,
            },
            msg = read.next() => match msg {
                Some(Ok(Message::Text(txt))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                        handle_frame(v, app_tx, out_tx, workspace_id, thread_id);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return,
            },
            _ = ping.tick() => {
                if write.send(Message::Text(json!({ "type": "ping" }).to_string())).await.is_err() {
                    return;
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
    }
}

/// Structured fields live in `data` (ephemeral) or `payload` (persisted).
fn blob(ev: &Value) -> Value {
    if ev.get("data").map(|d| d.is_object()).unwrap_or(false) {
        ev["data"].clone()
    } else if ev.get("payload").map(|d| d.is_object()).unwrap_or(false) {
        ev["payload"].clone()
    } else {
        json!({})
    }
}

fn sval(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// A concise, human-readable summary of a tool's arguments for the call line.
fn concise_args(tool: &str, v: &Value) -> String {
    let pick = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string());
    let chosen = match tool {
        "execute_command" => pick("command"),
        "execute_code" => {
            let lang = pick("language").unwrap_or_else(|| "python".into());
            pick("code").map(|c| format!("{lang}: {c}"))
        }
        t if t.starts_with("browser_") => pick("url").or_else(|| pick("selector")).or_else(|| pick("script")),
        "spawn_subagent" => pick("agent_id").or_else(|| pick("task")),
        _ => None,
    };
    let s = chosen.unwrap_or_else(|| {
        if v.is_null() || (v.is_object() && v.as_object().map(|o| o.is_empty()).unwrap_or(false)) {
            String::new()
        } else {
            v.to_string()
        }
    });
    let s = s.replace('\n', " ");
    if s.chars().count() > 120 {
        format!("{}…", s.chars().take(120).collect::<String>())
    } else {
        s
    }
}

fn compact(v: &Value, limit: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let s = s.replace('\n', " ");
    if s.chars().count() > limit {
        let mut t: String = s.chars().take(limit).collect();
        t.push('…');
        t
    } else {
        s
    }
}

fn handle_frame(v: Value, app_tx: &mpsc::UnboundedSender<AppEvent>, out_tx: &mpsc::UnboundedSender<String>, workspace_id: Option<&str>, thread_id: &str) {
    let mtype = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match mtype {
        "message_sent" => {
            let queued = v.get("queued").and_then(|q| q.as_bool()).unwrap_or(false);
            if queued {
                let _ = app_tx.send(AppEvent::Stream(StreamItem {
                    kind: "note".into(), agent: None,
                    text: Some("queued — will be injected after the current turn".into()),
                    tool_name: None, detail: None, status: None, local: false, task_id: None,
                }));
            } else {
                let _ = app_tx.send(AppEvent::RunStarted);
            }
            return;
        }
        "pong" => return,
        "error" => {
            let detail = sval(&v, "detail")
                .or_else(|| sval(&v, "error"))
                .or_else(|| sval(&v, "reason"))
                .unwrap_or_else(|| v.to_string());
            let _ = app_tx.send(AppEvent::Error(detail));
            let _ = app_tx.send(AppEvent::RunFinished("failed".into()));
            return;
        }
        "pulse_event" => {
            // Defensive: handle a wrapped form too.
            let inner = v.get("data").or_else(|| v.get("event")).cloned().unwrap_or(Value::Null);
            if inner.is_object() {
                return handle_frame(inner, app_tx, out_tx, workspace_id, thread_id);
            }
            return;
        }
        _ => {}
    }

    // Otherwise this frame IS a flat StreamEvent.
    let etype = mtype;
    let b = blob(&v);
    let status = sval(&b, "status");
    let agent = sval(&v, "agentName");

    // CLI_LOCAL tool execution.
    if etype == "tool" && status.as_deref() == Some("local_execute") {
        let tool_name = sval(&b, "toolName").unwrap_or_default();
        let request_id = sval(&b, "requestId").unwrap_or_default();
        // Inject routing metadata into every local tool call.
        // _thread_id keys the per-task sandbox directory in local.rs so each
        // workflow task (which has its own thread) gets a fully isolated
        // working directory for execute_command / execute_code / todo_*.
        let mut input = b.get("input").cloned().unwrap_or(json!({}));
        input["_thread_id"] = json!(thread_id);
        if let Some(ws) = workspace_id {
            input["_workspace_id"] = json!(ws);
        }
        let arg_summary = concise_args(&tool_name, &input);
        let _ = app_tx.send(AppEvent::Stream(StreamItem {
            kind: "tool_start".into(),
            agent,
            text: None,
            tool_name: Some(tool_name.clone()),
            detail: Some(arg_summary),
            status: Some("local_execute".into()),
            local: true,
            task_id: None,
        }));
        let app_tx = app_tx.clone();
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            let start = std::time::Instant::now();
            let res = local::run_tool(&tool_name, &input).await;
            let ms = start.elapsed().as_millis();
            let frame = if let Some(err) = &res.error {
                json!({ "type": "tool.local_error", "payload": {
                    "request_id": request_id, "tool_name": tool_name,
                    "error": err, "error_type": "Error" }})
            } else if res.captured_network.is_empty() {
                json!({ "type": "tool.local_result", "payload": {
                    "request_id": request_id, "tool_name": tool_name,
                    "output": res.output, "exit_code": res.exit_code, "duration_ms": ms }})
            } else {
                json!({ "type": "tool.local_result", "payload": {
                    "request_id": request_id, "tool_name": tool_name,
                    "output": res.output, "exit_code": res.exit_code, "duration_ms": ms,
                    "captured_network": res.captured_network }})
            };
            let _ = out_tx.send(frame.to_string());
            let _ = app_tx.send(AppEvent::LocalToolDone {
                name: tool_name, ms, exit: res.exit_code, err: res.error,
            });
        });
        return;
    }

    // Blocking approval — auto-approve in the TUI (configurable later).
    if etype == "approval" && status.as_deref() == Some("requested") {
        let approval_id = sval(&b, "approvalId").or_else(|| sval(&b, "approval_id")).unwrap_or_default();
        let _ = out_tx.send(
            json!({ "type": "approval.response", "approval_id": approval_id, "decision": "approved" })
                .to_string(),
        );
        let _ = app_tx.send(AppEvent::Stream(StreamItem {
            kind: "approval".into(), agent, text: sval(&b, "preview"),
            tool_name: None, detail: sval(&b, "module"), status: Some("requested".into()), local: false, task_id: None,
        }));
        return;
    }

    // request_human_input → blocking interrupt.
    if etype == "interrupt" {
        match status.as_deref() {
            Some("requested") => {
                let id = sval(&b, "interruptId").or_else(|| sval(&b, "interrupt_id")).unwrap_or_default();
                let title = sval(&b, "title").unwrap_or_else(|| "Input requested".into());
                let message = sval(&b, "message").unwrap_or_default();
                let mut fields = Vec::new();
                if let Some(fs) = b.get("formSchema").and_then(|x| x.get("fields")).and_then(|x| x.as_array()) {
                    for f in fs {
                        let key = sval(f, "key").unwrap_or_else(|| "value".into());
                        let label = sval(f, "label").unwrap_or_else(|| key.clone());
                        let ftype = sval(f, "type").unwrap_or_else(|| "text".into());
                        fields.push(Field { key, label, ftype });
                    }
                }
                if fields.is_empty() {
                    fields.push(Field { key: "value".into(), label: "response".into(), ftype: "text".into() });
                }
                let _ = app_tx.send(AppEvent::Interrupt { id, title, message, fields });
            }
            Some(other) => {
                let _ = app_tx.send(AppEvent::Stream(StreamItem {
                    kind: "note".into(), agent: None, text: Some(format!("interrupt {other}")),
                    tool_name: None, detail: None, status: None, local: false, task_id: None,
                }));
            }
            None => {}
        }
        return;
    }

    // Credit/token usage (live per-call deltas).
    let sub = sval(&b, "type");
    if etype == "credit.update" || sub.as_deref() == Some("credit.update") {
        let credits = b.get("credits_used").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let it = b.get("input_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        let ot = b.get("output_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        let _ = app_tx.send(AppEvent::Credits { credits, tokens: it + ot, final_run: false });
        return;
    }

    // plan.updated — batch task upserts from workspace_add_tasks.
    // payload: { "tasks": [{ "id", "title", "agentName", "status" }] }
    if etype == "plan.updated" {
        if let Some(tasks) = b.get("tasks").and_then(|t| t.as_array()) {
            for task in tasks {
                let tid = task.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                let title = task.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if title.is_empty() { continue; }
                let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("pending").to_string();
                let agent = task.get("agentName").and_then(|v| v.as_str()).map(|s| s.to_string());
                let _ = app_tx.send(AppEvent::Stream(StreamItem {
                    kind: "task".into(), agent, text: Some(title),
                    tool_name: None, detail: None, status: Some(status),
                    local: false, task_id: tid,
                }));
            }
        }
        return;
    }

    // Run lifecycle → finished.
    let terminal = etype == "run.completed"
        || etype == "run.failed"
        || (etype == "system"
            && matches!(sub.as_deref(), Some("run.completed") | Some("run.failed")));

    let item = match etype {
        "token" => Some(StreamItem {
            kind: "token".into(), agent,
            text: sval(&v, "content").or_else(|| sval(&b, "text")).or_else(|| sval(&b, "content")),
            tool_name: None, detail: None, status: None, local: false, task_id: None,
        }),
        "thinking" => Some(StreamItem {
            kind: "thinking".into(), agent,
            text: sval(&v, "content").or_else(|| sval(&b, "text")).or_else(|| sval(&b, "content")),
            tool_name: None, detail: None, status: None, local: false, task_id: None,
        }),
        "tool" => {
            let name = sval(&b, "toolName");
            match status.as_deref() {
                Some("start") => {
                    let detail = concise_args(name.as_deref().unwrap_or(""), b.get("arguments").unwrap_or(&Value::Null));
                    Some(StreamItem {
                        kind: "tool_start".into(), agent, text: None, tool_name: name,
                        detail: Some(detail), status: Some("start".into()), local: false, task_id: None,
                    })
                }
                Some("output") => Some(StreamItem {
                    kind: "tool_output".into(), agent, text: None, tool_name: name,
                    detail: Some(compact(b.get("result").unwrap_or(&Value::Null), 600)),
                    status: b.get("durationMs").map(|d| format!("{d}ms")), local: false, task_id: None,
                }),
                Some("failed") => Some(StreamItem {
                    kind: "tool_failed".into(), agent, text: None, tool_name: name,
                    detail: sval(&b, "error"), status: Some("failed".into()), local: false, task_id: None,
                }),
                _ => None,
            }
        }
        "task" => Some(StreamItem {
            kind: "task".into(), agent,
            text: sval(&b, "title").or_else(|| sval(&b, "taskId")),
            tool_name: None, detail: sval(&b, "error"), status: sval(&b, "status"), local: false,
            task_id: sval(&v, "taskId").or_else(|| sval(&b, "taskId")),
        }),
        "artifact" => Some(StreamItem {
            kind: "note".into(), agent,
            text: Some(format!("artifact {}: {}", sval(&b, "status").unwrap_or_default(), sval(&b, "name").unwrap_or_default())),
            tool_name: None, detail: sval(&b, "downloadUrl"), status: None, local: false, task_id: None,
        }),
        "system" => {
            let kind = sval(&b, "type").unwrap_or_default();
            Some(StreamItem {
                kind: "system".into(), agent, text: Some(kind),
                tool_name: None, detail: None, status: None, local: false, task_id: None,
            })
        }
        s if s.starts_with("run.") => Some(StreamItem {
            kind: "system".into(), agent, text: Some(s.to_string()),
            tool_name: None, detail: None, status: None, local: false, task_id: None,
        }),
        _ => None,
    };

    if let Some(it) = item {
        let _ = app_tx.send(AppEvent::Stream(it));
    }
    if terminal {
        // Authoritative run total from metrics (if present).
        if let Some(m) = b.get("metrics").filter(|m| m.is_object()) {
            let credits = m.get("credits_used").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let tokens = m.get("total_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
            if credits > 0.0 || tokens > 0 {
                let _ = app_tx.send(AppEvent::Credits { credits, tokens, final_run: true });
            }
        }
        let label = if etype.ends_with("failed") || sval(&b, "type").as_deref() == Some("run.failed") {
            format!("failed: {}", sval(&b, "error").unwrap_or_default())
        } else {
            "completed".into()
        };
        let _ = app_tx.send(AppEvent::RunFinished(label));
    }
}
