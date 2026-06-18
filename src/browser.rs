//! Local browser automation for the CLI_LOCAL `browser_*` tools, driving a
//! real Chrome/Chromium via the DevTools Protocol (chromiumoxide).
//!
//! When the agent (running in CLI_LOCAL mode) calls `browser_navigate`,
//! `browser_click`, etc., the pulse client routes them here so the user's
//! local browser is the agent's browser. A single persistent page is reused
//! across calls. Set `STROBES_AI_BROWSER_HEADLESS=1` for headless.

use anyhow::{anyhow, Result};
use base64::Engine;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use futures_util::StreamExt;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::sync::Mutex;

struct Session {
    _browser: Browser,
    page: Page,
}

static SESSION: OnceLock<Mutex<Option<Session>>> = OnceLock::new();

fn cell() -> &'static Mutex<Option<Session>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

async fn ensure(guard: &mut Option<Session>) -> Result<&mut Session> {
    if guard.is_none() {
        // Resolve a Chrome/Chromium binary. Missing → auto-install (opt-in) or
        // a clear, platform-specific install message the agent/user can act on.
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
        // Per-process profile dir so we never wedge on a stale SingletonLock.
        let profile = std::env::temp_dir().join(format!("strobes-chrome-{}", std::process::id()));
        let mut builder = BrowserConfig::builder()
            .chrome_executable(&chrome)
            .user_data_dir(&profile)
            .arg("--no-sandbox")
            .arg("--disable-dev-shm-usage");
        if !headless {
            builder = builder.with_head();
        }
        let config = builder.build().map_err(|e| anyhow!(e))?;
        let (browser, mut handler) = Browser::launch(config).await?;
        tokio::spawn(async move {
            while let Some(_event) = handler.next().await {}
        });
        let page = browser.new_page("about:blank").await?;
        *guard = Some(Session { _browser: browser, page });
    }
    Ok(guard.as_mut().unwrap())
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

/// Dispatch a `browser_*` command. Returns the human/agent-readable output, or
/// an error string on failure (incl. "Chrome not found").
pub async fn handle(cmd: &str, input: &Value) -> std::result::Result<String, String> {
    let mut guard = cell().lock().await;
    // ensure() returns a clear, actionable message when Chrome is missing.
    let sess = ensure(&mut guard).await.map_err(|e| e.to_string())?;
    run(cmd, input, sess).await.map_err(|e| e.to_string())
}

async fn run(cmd: &str, input: &Value, sess: &mut Session) -> Result<String> {
    let page = &sess.page;
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
