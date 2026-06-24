//! Local browser automation for the CLI_LOCAL `browser_*` tools, driving a
//! real Chrome/Chromium via the DevTools Protocol (chromiumoxide).
//!
//! When the agent (running in CLI_LOCAL mode) calls `browser_navigate`,
//! `browser_click`, etc., the pulse client routes them here so the user's
//! local browser is the agent's browser. A single persistent page is reused
//! across calls. Set `STROBES_AI_BROWSER_HEADLESS=1` for headless.

use anyhow::{anyhow, Result};
use base64::Engine;
use chromiumoxide::browser::Browser;
use chromiumoxide::page::Page;
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::sync::Mutex;

// --- Passive CDP network capture ---

const SKIP_EXTS: &[&str] = &[
    ".js", ".css", ".png", ".jpg", ".jpeg", ".gif", ".svg",
    ".ico", ".woff", ".woff2", ".ttf", ".eot", ".map",
];
const BODY_MAX: usize = 32 * 1024;
const BODY_MIMES: &[&str] = &[
    "application/json",
    "application/xml",
    "application/x-www-form-urlencoded",
    "text/",
];

struct CaptureGuard {
    stop_tx: tokio::sync::oneshot::Sender<()>,
    result_rx: tokio::sync::oneshot::Receiver<Vec<Value>>,
}

impl CaptureGuard {
    async fn finish(self) -> Vec<Value> {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _ = self.stop_tx.send(());
        self.result_rx.await.unwrap_or_default()
    }
}

async fn start_capture(page: &Page) -> Option<CaptureGuard> {
    use chromiumoxide::cdp::browser_protocol::network::{
        EnableParams, EventLoadingFinished, EventRequestWillBeSent, EventResponseReceived,
        GetResponseBodyParams,
    };

    let _ = page.execute(EnableParams::default()).await;

    let shared: Arc<StdMutex<Vec<Value>>> = Arc::new(StdMutex::new(Vec::new()));
    let cap = Arc::clone(&shared);
    let page_inner = page.clone();

    let mut req_ev = page.event_listener::<EventRequestWillBeSent>().await.ok()?;
    let mut resp_ev = page.event_listener::<EventResponseReceived>().await.ok()?;
    let mut fin_ev = page.event_listener::<EventLoadingFinished>().await.ok()?;

    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<Vec<Value>>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => break,
                Some(ev) = req_ev.next() => {
                    let url = ev.request.url.clone();
                    if SKIP_EXTS.iter().any(|s| url.ends_with(s)) { continue; }
                    if url.starts_with("data:") || url.starts_with("blob:") || url.starts_with("chrome:") { continue; }
                    let entry = serde_json::json!({
                        "requestId": ev.request_id.inner(),
                        "method": &ev.request.method,
                        "url": url,
                        "headers": ev.request.headers.inner(),
                        "type": serde_json::to_value(&ev.r#type).unwrap_or_default(),
                        "body": "",
                        "bodyTruncated": false,
                    });
                    cap.lock().unwrap().push(entry);
                }
                Some(ev) = resp_ev.next() => {
                    let rid = ev.request_id.inner().clone();
                    let url = ev.response.url.clone();
                    let status = ev.response.status;
                    let mime = ev.response.mime_type.clone();
                    let resp_headers = ev.response.headers.inner().clone();
                    let mut entries = cap.lock().unwrap();
                    for entry in entries.iter_mut() {
                        if entry["requestId"].as_str() == Some(&rid)
                            || (entry["url"].as_str() == Some(&url) && entry.get("status").map_or(true, |s| s.is_null()))
                        {
                            entry["status"] = serde_json::json!(status);
                            entry["responseHeaders"] = resp_headers;
                            entry["mimeType"] = serde_json::json!(&mime);
                            break;
                        }
                    }
                }
                Some(ev) = fin_ev.next() => {
                    let rid = ev.request_id.inner().clone();
                    let mime_lc = cap.lock().unwrap()
                        .iter()
                        .find(|e| e["requestId"].as_str() == Some(&rid))
                        .and_then(|e| e["mimeType"].as_str().map(|s| s.to_lowercase()));
                    if let Some(mime) = mime_lc {
                        if BODY_MIMES.iter().any(|ok| mime.starts_with(ok)) {
                            if let Ok(resp) = page_inner.execute(
                                GetResponseBodyParams::new(ev.request_id.clone())
                            ).await {
                                let raw = resp.body.clone();
                                let decoded = if resp.base64_encoded {
                                    base64::engine::general_purpose::STANDARD
                                        .decode(&raw)
                                        .ok()
                                        .and_then(|b| String::from_utf8(b).ok())
                                        .unwrap_or(raw)
                                } else {
                                    raw
                                };
                                let truncated = decoded.len() > BODY_MAX;
                                let body_out = if truncated { decoded[..BODY_MAX].to_string() } else { decoded };
                                let mut entries = cap.lock().unwrap();
                                for entry in entries.iter_mut() {
                                    if entry["requestId"].as_str() == Some(&rid) {
                                        entry["body"] = serde_json::json!(body_out);
                                        entry["bodyTruncated"] = serde_json::json!(truncated);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let _ = result_tx.send(shared.lock().unwrap().clone());
    });

    Some(CaptureGuard { stop_tx, result_rx })
}

// --- Session ---

// One Chrome process per workspace.  All agents that belong to the same
// workspace share this process (and therefore share cookies, localStorage, and
// any auth state they establish).  Each agent gets its own Page (tab) so their
// navigations don't interfere.  The outer SESSIONS map is keyed on workspace_id
// and the per-workspace Arc<Mutex> is held only during the brief ensure +
// page-clone window, then released so concurrent agents proceed in parallel.
struct WorkspaceSession {
    _browser: Browser,
    _child: std::process::Child,
    pages: HashMap<String, Page>,   // agent_name → Page (tab)
}

impl Drop for WorkspaceSession {
    fn drop(&mut self) {
        let _ = self._child.kill();
    }
}

type WsCell = Arc<Mutex<Option<WorkspaceSession>>>;
static SESSIONS: OnceLock<Mutex<HashMap<String, WsCell>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, WsCell>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Grab a free TCP port by briefly binding to 0 then releasing it.
fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// Profile dir for a workspace — one subdirectory per workspace_id so all
/// agents in the same workspace share the same Chrome profile (cookies, auth).
fn browser_profile_dir(workspace_id: &str) -> PathBuf {
    let safe: String = workspace_id.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if let Ok(base) = std::env::var("STROBES_AI_BROWSER_PROFILE") {
        return PathBuf::from(base).join(&safe);
    }
    crate::config::config_dir().join("browser-profile").join(safe)
}

async fn ensure<'a>(guard: &'a mut Option<WorkspaceSession>, workspace_id: &str) -> Result<&'a mut WorkspaceSession> {
    if guard.is_none() {
        let chrome = match find_chrome() {
            Some(p) => p,
            None => {
                if std::env::var("STROBES_AI_BROWSER_AUTOINSTALL").as_deref() == Ok("1") {
                    autoinstall_chrome().await.map_err(|e| {
                        anyhow!("Chrome auto-install failed: {e}\n\n{}", install_instructions())
                    })?
                } else {
                    return Err(anyhow!("{}", install_instructions()));
                }
            }
        };

        let headless = std::env::var("STROBES_AI_BROWSER_HEADLESS").as_deref() == Ok("1");
        let profile = browser_profile_dir(workspace_id);
        let _ = std::fs::create_dir_all(&profile);

        // Kill any existing Chrome using this profile — on macOS a stale or
        // previous session's Chrome will "adopt" the new launch and never bind
        // our --remote-debugging-port.
        let _ = std::process::Command::new("pkill")
            .args(["-f", &format!("user-data-dir={}", profile.display())])
            .output();
        // Remove Chromium singleton lock files left by a killed instance so
        // Chrome doesn't try to forward to a dead process.
        for lock in &["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            let _ = std::fs::remove_file(profile.join(lock));
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let port = free_port()?;

        // Launch Chrome as a plain subprocess — no automation flags injected.
        // This keeps the TLS/H2 fingerprint clean so Akamai-class WAFs don't
        // block the connection the way they do when Playwright/chromiumoxide
        // launch Chrome (which always adds --enable-automation et al.).
        let mut cmd = std::process::Command::new(&chrome);
        cmd.arg(format!("--remote-debugging-port={port}"))
           .arg(format!("--user-data-dir={}", profile.display()))
           .arg("--no-first-run")
           .arg("--no-default-browser-check")
           .arg("--disable-dev-shm-usage")
           .arg("--disable-extensions");
        if headless {
            cmd.arg("--headless=new");
        }
        cmd.stdout(std::process::Stdio::null())
           .stderr(std::process::Stdio::null());
        let child = cmd.spawn().map_err(|e| anyhow!("failed to launch Chrome: {e}"))?;

        // Poll until Chrome's CDP endpoint is ready (usually < 1 s).
        let cdp_url = format!("http://127.0.0.1:{port}");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(anyhow!(
                    "Chrome CDP not ready after 15 s (port {port}, profile {})",
                    profile.display()
                ));
            }
            if reqwest::get(format!("{cdp_url}/json/version")).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }

        let (browser, mut handler) = Browser::connect(&cdp_url).await
            .map_err(|e| anyhow!("CDP connect failed on port {port}: {e}"))?;
        tokio::spawn(async move {
            while let Some(_event) = handler.next().await {}
        });
        *guard = Some(WorkspaceSession { _browser: browser, _child: child, pages: HashMap::new() });
    }
    Ok(guard.as_mut().unwrap())
}

/// Return the existing tab for `agent_name`, or open a new one in the shared
/// browser.  Called while the workspace cell lock is held.
async fn get_or_create_page(sess: &mut WorkspaceSession, agent_name: &str) -> Result<Page> {
    if !sess.pages.contains_key(agent_name) {
        let page = sess._browser.new_page("about:blank").await?;
        sess.pages.insert(agent_name.to_string(), page);
    }
    Ok(sess.pages[agent_name].clone())
}

/// Where auto-installed Chrome for Testing lives.
fn chrome_cache_dir() -> PathBuf {
    crate::config::config_dir().join("chrome")
}

/// Chrome for Testing platform key + the binary path inside its zip.
fn cft_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("mac-arm64"),
        ("macos", "x86_64") => Some("mac-x64"),
        ("linux", "x86_64") => Some("linux64"),
        ("windows", "x86_64") => Some("win64"),
        _ => None,
    }
}

fn cft_binary_rel(platform: &str) -> PathBuf {
    match platform {
        "mac-arm64" | "mac-x64" => PathBuf::from(format!("chrome-{platform}"))
            .join("Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
        "win64" => PathBuf::from("chrome-win64").join("chrome.exe"),
        _ => PathBuf::from(format!("chrome-{platform}")).join("chrome"),
    }
}

/// First Chrome/Chromium found: `STROBES_AI_CHROME`, our cached install, common
/// system locations, then `PATH`.
fn find_chrome() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STROBES_AI_CHROME") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    if let Some(plat) = cft_platform() {
        let p = chrome_cache_dir().join(cft_binary_rel(plat));
        if p.exists() {
            return Some(p);
        }
    }
    for p in system_candidates() {
        if p.exists() {
            return Some(p);
        }
    }
    for name in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "chrome"] {
        if let Some(p) = which(name) {
            return Some(p);
        }
    }
    None
}

fn system_candidates() -> Vec<PathBuf> {
    match std::env::consts::OS {
        "macos" => vec![
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
            "/Applications/Chromium.app/Contents/MacOS/Chromium".into(),
            "/Applications/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing".into(),
        ],
        "windows" => vec![
            r"C:\Program Files\Google\Chrome\Application\chrome.exe".into(),
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe".into(),
        ],
        _ => vec![
            "/usr/bin/google-chrome".into(),
            "/usr/bin/google-chrome-stable".into(),
            "/usr/bin/chromium".into(),
            "/usr/bin/chromium-browser".into(),
            "/snap/bin/chromium".into(),
        ],
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let name = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    std::env::split_paths(&path)
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

/// Platform-specific guidance returned when no browser is available.
fn install_instructions() -> String {
    let how = match std::env::consts::OS {
        "macos" => "Install Google Chrome:\n  • brew install --cask google-chrome\n  • or download https://www.google.com/chrome/",
        "windows" => "Install Google Chrome:\n  • winget install -e --id Google.Chrome\n  • or download https://www.google.com/chrome/",
        "linux" => "Install Chrome/Chromium:\n  • Debian/Ubuntu: sudo apt install -y chromium   (or google-chrome-stable)\n  • Fedora: sudo dnf install -y chromium\n  • or download https://www.google.com/chrome/",
        _ => "Install Google Chrome / Chromium from https://www.google.com/chrome/",
    };
    format!(
        "Google Chrome / Chromium is required for browser_* tools but none was found.\n\n{how}\n\n\
         Already installed elsewhere? Point at it: STROBES_AI_CHROME=/path/to/chrome\n\
         Or auto-download a self-contained Chrome for Testing: set STROBES_AI_BROWSER_AUTOINSTALL=1 and retry."
    )
}

/// Download + cache a self-contained Chrome for Testing build (opt-in).
async fn autoinstall_chrome() -> Result<PathBuf> {
    let plat = cft_platform().ok_or_else(|| {
        anyhow!("auto-install is unavailable on {}/{}", std::env::consts::OS, std::env::consts::ARCH)
    })?;
    let dest = chrome_cache_dir().join(cft_binary_rel(plat));
    if dest.exists() {
        return Ok(dest);
    }
    let manifest: Value = reqwest::get(
        "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json",
    )
    .await?
    .json()
    .await?;
    let url = manifest["channels"]["Stable"]["downloads"]["chrome"]
        .as_array()
        .and_then(|arr| arr.iter().find(|d| d["platform"].as_str() == Some(plat)))
        .and_then(|d| d["url"].as_str())
        .ok_or_else(|| anyhow!("no Chrome for Testing build for {plat}"))?
        .to_string();

    let bytes = reqwest::get(&url).await?.bytes().await?;
    let dir = chrome_cache_dir();
    std::fs::create_dir_all(&dir)?;
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let mut zip = zip::ZipArchive::new(cursor)?;
    zip.extract(&dir)?;

    #[cfg(unix)]
    if dest.exists() {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&dest)?.permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&dest, perm)?;
    }
    if !dest.exists() {
        return Err(anyhow!("extracted Chrome for Testing but binary missing at {}", dest.display()));
    }
    Ok(dest)
}

fn s<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

/// Dispatch a `browser_*` command. Returns `(output, captured_network)` on success
/// or an error string on failure (incl. "Chrome not found").
///
/// `input` must contain `_workspace_id` and `_agent_name` injected by pulse.rs
/// from the WebSocket event so the correct Chrome process and tab are selected.
pub async fn handle(cmd: &str, input: &Value) -> std::result::Result<(String, Vec<Value>), String> {
    // pulse.rs injects these from the WebSocket event metadata.
    let workspace_id = input.get("_workspace_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let agent_name = input.get("_agent_name")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    // Grab (or create) the workspace-level Chrome cell.  Map lock held briefly.
    let cell: WsCell = {
        let mut map = sessions().lock().await;
        map.entry(workspace_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };

    // Lock the workspace cell only long enough to ensure the browser is up and
    // clone the agent's tab handle (Page::clone is cheap — Arc inside).
    let page = {
        let mut guard = cell.lock().await;
        let sess = ensure(&mut guard, &workspace_id).await.map_err(|e| e.to_string())?;
        get_or_create_page(sess, &agent_name).await.map_err(|e| e.to_string())?
        // guard drops here — other agents can proceed concurrently
    };

    let capture = start_capture(&page).await;
    let result = run(cmd, input, &page).await.map_err(|e| e.to_string());
    let captured = match capture {
        Some(g) => g.finish().await,
        None => vec![],
    };
    result.map(|output| (output, captured))
}

async fn run(cmd: &str, input: &Value, page: &Page) -> Result<String> {
    match cmd {
        "browser_init" => Ok("browser ready".into()),
        "browser_navigate" => {
            let url = {
                let u = s(input, "url");
                if u.is_empty() { "about:blank" } else { u }
            };
            page.goto(url).await?;
            let _ = page.wait_for_navigation().await;
            let title = page.get_title().await?.unwrap_or_default();
            let cur = page.url().await?.unwrap_or_default();
            Ok(format!("{title} — {cur}").trim_matches([' ', '—']).to_string())
        }
        "browser_snapshot" => {
            let js = r#"(() => {
                const out=[];
                document.querySelectorAll('a,button,input,textarea,select,[role],h1,h2,h3,label').forEach((el,i)=>{
                    if(i>300)return;
                    const tag=el.tagName.toLowerCase();
                    const t=(el.innerText||el.value||el.getAttribute('aria-label')||el.getAttribute('placeholder')||'').trim().slice(0,80);
                    const id=el.id?('#'+el.id):'';
                    out.push(tag+id+' :: '+t);
                });
                return out.join('\n');
            })()"#;
            let v: String = page.evaluate(js).await?.into_value().unwrap_or_default();
            let title = page.get_title().await?.unwrap_or_default();
            Ok(format!("# {title}\n{v}"))
        }
        "browser_click" => {
            let sel = s(input, "selector");
            page.find_element(sel).await?.click().await?;
            Ok(format!("clicked {sel}"))
        }
        "browser_type" => {
            let sel = s(input, "selector");
            let text = s(input, "text");
            let el = page.find_element(sel).await?;
            el.click().await?;
            el.type_str(text).await?;
            Ok(format!("typed into {sel}"))
        }
        "browser_scroll" => {
            let amount = input.get("amount").and_then(|v| v.as_i64()).unwrap_or(600);
            let dir = s(input, "direction");
            let dy = if dir == "up" { -amount } else { amount };
            page.evaluate(format!("window.scrollBy(0,{dy})")).await?;
            Ok(format!("scrolled {} {amount}px", if dir.is_empty() { "down" } else { dir }))
        }
        "browser_execute_script" => {
            let script = s(input, "script");
            let v: Value = page.evaluate(script).await?.into_value().unwrap_or(Value::Null);
            Ok(serde_json::to_string(&v).unwrap_or_default())
        }
        "browser_screenshot" => {
            let params = chromiumoxide::page::ScreenshotParams::builder().build();
            let png = page.screenshot(params).await?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
            Ok(format!("data:image/png;base64,{b64}"))
        }
        "browser_get_cookies" => {
            let cookies = page.get_cookies().await?;
            Ok(serde_json::to_string(&cookies).unwrap_or_default())
        }
        other => Err(anyhow!("unknown browser command: {other}")),
    }
}
