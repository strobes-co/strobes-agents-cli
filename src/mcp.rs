//! MCP servers on the user's machine, offered to the agent as tools.
//!
//! Config is Claude Code's format, so an existing `.mcp.json` just works:
//!
//! ```json
//! { "mcpServers": {
//!     "burp":  { "command": "burp-mcp", "args": ["--port", "9876"], "env": {"K": "${K}"} },
//!     "notes": { "type": "http", "url": "http://127.0.0.1:8000/mcp", "headers": {"Authorization": "Bearer ${TOKEN}"} }
//! } }
//! ```
//!
//! Read from the user's config dir (`mcp.json`, always trusted) and the project
//! (`.mcp.json`, `.strobes/mcp.json`). A project's servers run commands from
//! the repo, so they start only once the user trusts that project (`/mcp trust`).
//!
//! Each tool reaches the agent as `mcp__<server>__<tool>`; the server proxies
//! calls back here as `tool.local_execute` with that name.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

const PROTOCOL: &str = "2025-06-18";
const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq)]
pub enum Spec {
    Stdio { command: String, args: Vec<String>, env: HashMap<String, String> },
    Http { url: String, headers: HashMap<String, String> },
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    pub spec: Spec,
    /// The file it came from.
    pub source: PathBuf,
    /// From a project file (needs `/mcp trust`).
    pub project: bool,
}

#[derive(Debug, Clone)]
pub struct ToolInfo {
    /// Name the agent sees: mcp__<server>__<tool>.
    pub full: String,
    pub tool: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Debug, Clone)]
pub enum Status {
    Starting,
    Ready,
    Failed(String),
    /// A project server the user hasn't trusted yet.
    NeedsTrust,
}

struct Conn {
    transport: Transport,
    next_id: Mutex<i64>,
}

enum Transport {
    Stdio {
        stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
        pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
        _child: tokio::process::Child,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
        session: Mutex<Option<String>>,
        http: reqwest::Client,
    },
}

pub struct Server {
    pub config: ServerConfig,
    pub status: Status,
    pub tools: Vec<ToolInfo>,
    conn: Option<Arc<Conn>>,
}

fn registry() -> &'static RwLock<Vec<Server>> {
    static R: OnceLock<RwLock<Vec<Server>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(Vec::new()))
}

/// `${VAR}` / `$VAR`-free expansion of `${VAR}` from the environment.
fn expand(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find('}') {
            Some(j) => {
                let var = &rest[i + 2..i + 2 + j];
                let (name, default) = var.split_once(":-").unwrap_or((var, ""));
                out.push_str(&std::env::var(name).unwrap_or_else(|_| default.to_string()));
                rest = &rest[i + 3 + j..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn str_map(v: Option<&Value>) -> HashMap<String, String> {
    v.and_then(|m| m.as_object())
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), expand(s)))).collect())
        .unwrap_or_default()
}

pub fn parse_config(text: &str, source: PathBuf, project: bool) -> Result<Vec<ServerConfig>> {
    let v: Value = serde_json::from_str(text).with_context(|| format!("{} is not valid JSON", source.display()))?;
    let servers = v.get("mcpServers").and_then(|s| s.as_object()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for (name, s) in servers {
        if s.get("disabled").and_then(|d| d.as_bool()) == Some(true) {
            continue;
        }
        let kind = s.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let spec = if let Some(url) = s.get("url").and_then(|u| u.as_str()).filter(|_| kind != "stdio") {
            if kind == "sse" {
                // The old SSE transport isn't supported; streamable HTTP is.
                continue;
            }
            Spec::Http { url: expand(url), headers: str_map(s.get("headers")) }
        } else if let Some(cmd) = s.get("command").and_then(|c| c.as_str()) {
            Spec::Stdio {
                command: expand(cmd),
                args: s
                    .get("args")
                    .and_then(|a| a.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(expand)).collect())
                    .unwrap_or_default(),
                env: str_map(s.get("env")),
            }
        } else {
            continue;
        };
        out.push(ServerConfig { name, spec, source: source.clone(), project });
    }
    Ok(out)
}

pub fn config_files() -> Vec<(PathBuf, bool)> {
    let root = crate::project::root();
    vec![
        (crate::config::config_dir().join("mcp.json"), false),
        (root.join(".mcp.json"), true),
        (root.join(".strobes").join("mcp.json"), true),
    ]
}

/// Every configured server; a later file overrides an earlier one by name.
pub fn configured() -> Vec<ServerConfig> {
    let mut by_name: Vec<ServerConfig> = Vec::new();
    for (file, project) in config_files() {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        match parse_config(&text, file.clone(), project) {
            Ok(list) => {
                for c in list {
                    by_name.retain(|x| x.name != c.name);
                    by_name.push(c);
                }
            }
            Err(e) => eprintln!("strobes: {e}"),
        }
    }
    by_name
}

fn trusted_projects() -> Vec<String> {
    crate::config::Config::load().mcp_trusted
}

pub fn is_trusted() -> bool {
    let root = crate::project::root().display().to_string();
    trusted_projects().contains(&root)
}

pub fn trust_project() -> Result<()> {
    let mut cfg = crate::config::Config::load();
    let root = crate::project::root().display().to_string();
    if !cfg.mcp_trusted.contains(&root) {
        cfg.mcp_trusted.push(root);
        cfg.save()?;
    }
    Ok(())
}

/// Tool-name-safe piece: letters, digits, `_` and `-`.
fn safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn full_name(server: &str, tool: &str) -> String {
    let mut n = format!("mcp__{}__{}", safe(server), safe(tool));
    n.truncate(64);
    n
}

impl Conn {
    async fn request(&self, method: &str, params: Value, timeout: std::time::Duration) -> Result<Value> {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let reply = match &self.transport {
            Transport::Stdio { stdin, pending, .. } => {
                let (tx, rx) = oneshot::channel();
                pending.lock().unwrap().insert(id, tx);
                {
                    let mut w = stdin.lock().await;
                    w.write_all(format!("{msg}\n").as_bytes()).await?;
                    w.flush().await?;
                }
                match tokio::time::timeout(timeout, rx).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(_)) => return Err(anyhow!("server exited")),
                    Err(_) => {
                        pending.lock().unwrap().remove(&id);
                        return Err(anyhow!("{method} timed out"));
                    }
                }
            }
            Transport::Http { .. } => self.http_send(&msg, Some(id), timeout).await?,
        };
        if let Some(err) = reply.get("error") {
            return Err(anyhow!("{}", err.get("message").and_then(|m| m.as_str()).unwrap_or("MCP error")));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut w = stdin.lock().await;
                w.write_all(format!("{msg}\n").as_bytes()).await?;
                w.flush().await?;
            }
            Transport::Http { .. } => {
                self.http_send(&msg, None, START_TIMEOUT).await?;
            }
        }
        Ok(())
    }

    /// Streamable HTTP: POST one message; the reply is JSON or an SSE stream
    /// carrying it.
    async fn http_send(&self, msg: &Value, id: Option<i64>, timeout: std::time::Duration) -> Result<Value> {
        let Transport::Http { url, headers, session, http } = &self.transport else { unreachable!() };
        let mut req = http
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL)
            .timeout(timeout)
            .json(msg);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(s) = session.lock().unwrap().clone() {
            req = req.header("Mcp-Session-Id", s);
        }
        let resp = req.send().await?;
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *session.lock().unwrap() = Some(s.to_string());
        }
        let status = resp.status();
        let sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.contains("text/event-stream"));
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("HTTP {}: {}", status.as_u16(), body.chars().take(200).collect::<String>()));
        }
        let Some(id) = id else { return Ok(Value::Null) };
        let candidates: Vec<Value> = if sse {
            body.lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .filter_map(|d| serde_json::from_str(d.trim()).ok())
                .collect()
        } else {
            serde_json::from_str(&body).map(|v| vec![v]).unwrap_or_default()
        };
        candidates
            .into_iter()
            .flat_map(|v| if let Value::Array(a) = v { a } else { vec![v] })
            .find(|v| v.get("id").and_then(|i| i.as_i64()) == Some(id))
            .ok_or_else(|| anyhow!("no reply from the server"))
    }
}

async fn connect(cfg: &ServerConfig) -> Result<Arc<Conn>> {
    let transport = match &cfg.spec {
        Spec::Stdio { command, args, env } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args)
                .envs(env)
                .current_dir(crate::project::root())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            let mut child = cmd.spawn().with_context(|| format!("could not run `{command}`"))?;
            let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
            let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
            let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> = Default::default();
            let p = pending.clone();
            let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    let id = v.get("id").and_then(|i| i.as_i64());
                    if v.get("method").is_some() {
                        // A request from the server (ping, roots/list, …):
                        // answer so it never blocks on us.
                        if let Some(id) = v.get("id").cloned() {
                            let result = match v.get("method").and_then(|m| m.as_str()) {
                                Some("roots/list") => json!({"roots": []}),
                                _ => json!({}),
                            };
                            let _ = reply_tx.send(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string());
                        }
                    } else if let Some(id) = id {
                        if let Some(tx) = p.lock().unwrap().remove(&id) {
                            let _ = tx.send(v);
                        }
                    }
                }
                // Server gone: fail everything still waiting.
                p.lock().unwrap().clear();
            });
            let stdin = tokio::sync::Mutex::new(stdin);
            let conn = Arc::new(Conn {
                transport: Transport::Stdio { stdin, pending, _child: child },
                next_id: Mutex::new(0),
            });
            let c = conn.clone();
            tokio::spawn(async move {
                while let Some(line) = reply_rx.recv().await {
                    if let Transport::Stdio { stdin, .. } = &c.transport {
                        let mut w = stdin.lock().await;
                        let _ = w.write_all(format!("{line}\n").as_bytes()).await;
                        let _ = w.flush().await;
                    }
                }
            });
            return init(conn).await;
        }
        Spec::Http { url, headers } => Transport::Http {
            url: url.clone(),
            headers: headers.clone(),
            session: Mutex::new(None),
            http: reqwest::Client::new(),
        },
    };
    init(Arc::new(Conn { transport, next_id: Mutex::new(0) })).await
}

async fn init(conn: Arc<Conn>) -> Result<Arc<Conn>> {
    conn.request(
        "initialize",
        json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "strobes", "version": env!("CARGO_PKG_VERSION")},
        }),
        START_TIMEOUT,
    )
    .await?;
    conn.notify("notifications/initialized").await?;
    Ok(conn)
}

async fn list_tools(server: &str, conn: &Conn) -> Result<Vec<ToolInfo>> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let r = conn.request("tools/list", params, START_TIMEOUT).await?;
        for t in r.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default() {
            let Some(name) = t.get("name").and_then(|n| n.as_str()) else { continue };
            tools.push(ToolInfo {
                full: full_name(server, name),
                tool: name.to_string(),
                description: t.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string(),
                schema: t.get("inputSchema").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
            });
        }
        cursor = r.get("nextCursor").and_then(|c| c.as_str()).map(String::from);
        if cursor.is_none() || tools.len() > 500 {
            return Ok(tools);
        }
    }
}

/// (Re)start every configured server in the background. Untrusted project
/// servers are listed but not started.
pub fn start_all() {
    let trusted = is_trusted();
    let configs = configured();
    {
        let mut reg = registry().write().unwrap();
        reg.clear();
        for c in &configs {
            let status = if c.project && !trusted { Status::NeedsTrust } else { Status::Starting };
            reg.push(Server { config: c.clone(), status, tools: vec![], conn: None });
        }
    }
    for c in configs {
        if c.project && !trusted {
            continue;
        }
        tokio::spawn(async move {
            let result = async {
                let conn = connect(&c).await?;
                let tools = list_tools(&c.name, &conn).await?;
                Ok::<_, anyhow::Error>((conn, tools))
            }
            .await;
            let mut reg = registry().write().unwrap();
            if let Some(s) = reg.iter_mut().find(|s| s.config.name == c.name) {
                match result {
                    Ok((conn, tools)) => {
                        s.status = Status::Ready;
                        s.tools = tools;
                        s.conn = Some(conn);
                    }
                    Err(e) => s.status = Status::Failed(format!("{e:#}")),
                }
            }
        });
    }
}

/// Wait until no server is still starting (or the timeout passes).
pub async fn wait_ready(timeout: std::time::Duration) {
    let end = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < end {
        if !registry().read().unwrap().iter().any(|s| matches!(s.status, Status::Starting)) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Tools to offer the agent, as the server expects them in `client_tools`.
pub fn client_tools() -> Vec<Value> {
    let reg = registry().read().unwrap();
    reg.iter()
        .filter(|s| matches!(s.status, Status::Ready))
        .flat_map(|s| {
            s.tools.iter().map(move |t| {
                json!({
                    "name": t.full,
                    "description": format!("[{} MCP] {}", s.config.name, t.description),
                    "input_schema": t.schema,
                })
            })
        })
        .take(64)
        .collect()
}

pub fn is_mcp_tool(name: &str) -> bool {
    name.starts_with("mcp__")
}

/// Run one tool call for the agent.
pub async fn call(full: &str, args: &Value) -> Result<String> {
    let (conn, tool) = {
        let reg = registry().read().unwrap();
        reg.iter()
            .find_map(|s| {
                let t = s.tools.iter().find(|t| t.full == full)?;
                Some((s.conn.clone()?, t.tool.clone()))
            })
            .ok_or_else(|| anyhow!("MCP tool {full} is not available (see /mcp)"))?
    };
    // Routing metadata the CLI adds to every call isn't the tool's.
    let mut args = args.clone();
    if let Some(o) = args.as_object_mut() {
        o.retain(|k, _| !k.starts_with('_'));
    }
    let r = conn.request("tools/call", json!({"name": tool, "arguments": args}), CALL_TIMEOUT).await?;
    let text = render_content(&r);
    if r.get("isError").and_then(|e| e.as_bool()) == Some(true) {
        return Err(anyhow!("{text}"));
    }
    Ok(text)
}

fn render_content(r: &Value) -> String {
    let mut parts = Vec::new();
    for c in r.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default() {
        match c.get("type").and_then(|t| t.as_str()) {
            Some("text") => parts.push(c.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
            Some("resource") => {
                let res = c.get("resource").cloned().unwrap_or(Value::Null);
                match res.get("text").and_then(|t| t.as_str()) {
                    Some(t) => parts.push(t.to_string()),
                    None => parts.push(format!("[resource {}]", res.get("uri").and_then(|u| u.as_str()).unwrap_or(""))),
                }
            }
            Some("resource_link") => parts.push(format!("[link {}]", c.get("uri").and_then(|u| u.as_str()).unwrap_or(""))),
            Some(other) => parts.push(format!("[{other} content]")),
            None => {}
        }
    }
    if parts.is_empty() {
        if let Some(s) = r.get("structuredContent") {
            return s.to_string();
        }
    }
    parts.join("\n")
}

/// Rows for `/mcp`: (name, status text, tool names, source).
pub fn status() -> Vec<(String, String, Vec<String>, String)> {
    let reg = registry().read().unwrap();
    reg.iter()
        .map(|s| {
            let st = match &s.status {
                Status::Starting => "starting…".to_string(),
                Status::Ready => format!("connected · {} tools", s.tools.len()),
                Status::Failed(e) => format!("failed: {e}"),
                Status::NeedsTrust => "not started — project server, run /mcp trust".to_string(),
            };
            (
                s.config.name.clone(),
                st,
                s.tools.iter().map(|t| t.tool.clone()).collect(),
                s.config.source.display().to_string(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_code_config() {
        std::env::set_var("STROBES_MCP_TEST_TOKEN", "abc");
        let cfg = parse_config(
            r#"{"mcpServers": {
                "fs": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]},
                "api": {"type": "http", "url": "http://localhost:9/mcp", "headers": {"Authorization": "Bearer ${STROBES_MCP_TEST_TOKEN}"}},
                "old": {"type": "sse", "url": "http://x/sse"},
                "off": {"command": "x", "disabled": true}
            }}"#,
            PathBuf::from(".mcp.json"),
            true,
        )
        .unwrap();
        let names: Vec<_> = cfg.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["api", "fs"]);
        match &cfg[0].spec {
            Spec::Http { headers, .. } => assert_eq!(headers["Authorization"], "Bearer abc"),
            _ => panic!(),
        }
    }

    #[test]
    fn names_are_tool_safe() {
        assert_eq!(full_name("my server", "read.file"), "mcp__my_server__read_file");
        assert!(full_name("s", &"x".repeat(100)).len() <= 64);
        assert_eq!(expand("a${STROBES_NOPE_X:-dflt}b"), "adfltb");
    }

    #[test]
    fn content_renders_as_text() {
        let r = json!({"content": [{"type": "text", "text": "hi"}, {"type": "image", "data": "..."}]});
        assert_eq!(render_content(&r), "hi\n[image content]");
        assert_eq!(render_content(&json!({"content": [], "structuredContent": {"a": 1}})), "{\"a\":1}");
    }

    /// A real stdio server: a tiny Python MCP server, end to end.
    #[tokio::test]
    async fn stdio_roundtrip() {
        let py = r#"
import sys, json
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    if m["method"] == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "t", "version": "1"}}
    elif m["method"] == "tools/list":
        r = {"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}
    elif m["method"] == "tools/call":
        r = {"content": [{"type": "text", "text": "echo: " + m["params"]["arguments"]["text"]}]}
    else:
        r = {}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;
        let cfg = ServerConfig {
            name: "t".into(),
            spec: Spec::Stdio { command: "python3".into(), args: vec!["-c".into(), py.into()], env: Default::default() },
            source: PathBuf::from("test"),
            project: false,
        };
        let conn = connect(&cfg).await.expect("connect");
        let tools = list_tools("t", &conn).await.unwrap();
        assert_eq!(tools[0].full, "mcp__t__echo");
        registry().write().unwrap().push(Server { config: cfg, status: Status::Ready, tools, conn: Some(conn) });
        let out = call("mcp__t__echo", &json!({"text": "hello", "_thread_id": "x"})).await.unwrap();
        assert_eq!(out, "echo: hello");
        assert!(client_tools().iter().any(|t| t["name"] == "mcp__t__echo"));
    }
}
