//! OS process sandboxing — the half that makes the egress proxy unavoidable.
//!
//! [`crate::egress_proxy`] decides what a command may reach. This module
//! makes sure a command cannot decline to ask: it runs each one under an OS
//! sandbox whose only permitted network destination is the proxy on
//! loopback. Setting `HTTP_PROXY` is a request a program is free to ignore
//! (Go's `net/http` ignores it outright unless the transport opts in) — here
//! the kernel refuses every other destination, so a program that bypasses
//! the proxy gets no network at all rather than unfiltered access.
//!
//! Backends, mirroring `strobes-bridge`'s `procsandbox.py` design:
//!
//! * **macOS** — Seatbelt via `sandbox-exec`. Seatbelt's `remote` filter
//!   cannot express an IP allowlist beyond `*`/`localhost`, which is exactly
//!   why the allowlist lives in the proxy and Seatbelt's job is reduced to
//!   "loopback proxy port, nothing else."
//! * **Linux** — bubblewrap with `--unshare-net`. A fresh network namespace
//!   has no route to the host's loopback, so the sandboxed process reaches
//!   the proxy over a Unix domain socket (which crosses the namespace
//!   boundary fine, being a filesystem object) via a tiny relay that
//!   presents it as a local TCP port inside the sandbox — see
//!   [`run_relay_and_exec`], invoked as the CLI's hidden `__egress-relay`
//!   subcommand.
//! * **Windows** — no equivalent primitive exists; see
//!   [`crate::winsandbox`], which is a stub for now.
//!
//! There is deliberately **no unsandboxed fallback**. If no backend is
//! available, [`confine`] returns [`SandboxUnavailable`] and the caller must
//! refuse to execute rather than quietly running with unrestricted egress.

use crate::egress_proxy::EgressProxy;

#[derive(Debug, Clone)]
pub struct SandboxUnavailable(pub String);

impl std::fmt::Display for SandboxUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sandbox unavailable: {}", self.0)
    }
}

/// A command rewritten to run under the sandbox: the real argv[0] and args
/// the caller should now spawn instead of the original ones, plus the
/// environment variables that steer the sandboxed process at the proxy.
pub struct Confined {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

fn proxy_env(url: &str) -> Vec<(String, String)> {
    vec![
        ("HTTP_PROXY".to_string(), url.to_string()),
        ("HTTPS_PROXY".to_string(), url.to_string()),
        ("ALL_PROXY".to_string(), url.to_string()),
        ("http_proxy".to_string(), url.to_string()),
        ("https_proxy".to_string(), url.to_string()),
        ("all_proxy".to_string(), url.to_string()),
    ]
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file())
        })
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub fn confine(program: &str, args: &[String], proxy: &EgressProxy) -> Result<Confined, SandboxUnavailable> {
    if !which("sandbox-exec") {
        return Err(SandboxUnavailable(
            "sandbox-exec not found (unexpected on macOS)".to_string(),
        ));
    }
    let port = proxy.tcp_addr.port();
    // `(allow default)` then narrow just network-outbound to loopback-only,
    // then re-open exactly the proxy port. Everything else (filesystem,
    // process exec, signals) is intentionally left open — the sandbox's job
    // here is the network boundary, not general confinement; the per-thread
    // working directory (`local.rs::sandbox_dir_for`) already scopes
    // filesystem writes to what the AI is expected to touch.
    // Seatbelt's `remote ip` filter literally only accepts the string `*`
    // or `localhost:PORT` as its address form — not a dotted-quad literal,
    // confirmed against a real `sandbox-exec` run ("host must be * or
    // localhost in network address"). `localhost` still resolves to the
    // loopback the proxy is bound to, so this is not a gap.
    let profile = format!(
        r#"(version 1)
(allow default)
(deny network-outbound (remote ip))
(allow network-outbound (remote ip "localhost:{port}"))
"#
    );
    let profile_path = std::env::temp_dir().join(format!(
        "strobes-seatbelt-{}-{}.sb",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&profile_path, profile)
        .map_err(|e| SandboxUnavailable(format!("failed to write seatbelt profile: {e}")))?;

    let mut full_args = vec!["-f".to_string(), profile_path.to_string_lossy().to_string(), "--".to_string(), program.to_string()];
    full_args.extend(args.iter().cloned());
    Ok(Confined {
        program: "sandbox-exec".to_string(),
        args: full_args,
        env: proxy_env(&proxy.proxy_url()),
    })
}

#[cfg(target_os = "linux")]
pub fn confine(program: &str, args: &[String], proxy: &EgressProxy) -> Result<Confined, SandboxUnavailable> {
    if !which("bwrap") {
        return Err(SandboxUnavailable(
            "bubblewrap (bwrap) not installed — install it to enable the egress sandbox on Linux".to_string(),
        ));
    }
    let unix_path = proxy
        .unix_path
        .clone()
        .ok_or_else(|| SandboxUnavailable("egress proxy has no unix socket to relay through".to_string()))?;
    let relay_port = proxy.tcp_addr.port();
    let self_exe = relay_self_exe()?;

    // `--bind / /` passes the real filesystem through unrestricted — this
    // sandbox's boundary is network, not files (see the macOS comment
    // above); `--unshare-net` is the actual isolation. `--die-with-parent`
    // means a killed CLI process doesn't leave an orphaned sandboxed child
    // running.
    let mut full_args: Vec<String> = vec![
        "--unshare-net".to_string(),
        "--die-with-parent".to_string(),
        "--bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--".to_string(),
        self_exe.to_string_lossy().to_string(),
        "__egress-relay".to_string(),
        "--unix".to_string(),
        unix_path.to_string_lossy().to_string(),
        "--port".to_string(),
        relay_port.to_string(),
        "--".to_string(),
        program.to_string(),
    ];
    full_args.extend(args.iter().cloned());

    Ok(Confined {
        program: "bwrap".to_string(),
        args: full_args,
        // The relay listens on this same port number inside its own fresh
        // netns — no collision with the host proxy despite the identical
        // number, since the two are different network namespaces.
        env: proxy_env(&format!("http://127.0.0.1:{relay_port}")),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn confine(program: &str, args: &[String], proxy: &EgressProxy) -> Result<Confined, SandboxUnavailable> {
    crate::winsandbox::confine(program, args, proxy)
}

/// The real `strobes` binary's own path, for Linux's self-re-exec relay
/// trick. `std::env::current_exe()` is correct for a normally-run binary,
/// but under `cargo test` it resolves to the *test harness* binary
/// (`target/debug/deps/strobes-<hash>`) rather than the CLI — re-execing
/// that with our `--unix`/`--port` flags hands them to libtest's own
/// argument parser instead of ours. Detected by name (the harness binary's
/// file name never matches the real one) and corrected by looking one
/// directory up, where `cargo test` also always builds the real binary.
#[cfg(target_os = "linux")]
fn relay_self_exe() -> Result<std::path::PathBuf, SandboxUnavailable> {
    let exe = std::env::current_exe()
        .map_err(|e| SandboxUnavailable(format!("cannot resolve own executable path: {e}")))?;
    if exe.file_name().and_then(|n| n.to_str()) == Some("strobes") {
        return Ok(exe);
    }
    if let Some(sibling) = exe.parent().and_then(|p| p.parent()).map(|p| p.join("strobes")) {
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    Ok(exe)
}

/// The hidden `__egress-relay` entry point bwrap invokes inside the
/// sandboxed netns. Binds a TCP listener on loopback at `port` (valid even
/// though this namespace's loopback is otherwise disconnected — a fresh
/// netns still has its own private `lo`), bridges every connection to the
/// Unix socket at `unix_path` (reaching back out to the real
/// [`crate::egress_proxy`] listening on the host side of that socket, which
/// is a filesystem object and so visible inside the sandbox via the `--bind
/// / /` above), then execs the real command and exits with its status.
///
/// Deliberately blocking/`std`-only rather than async: this process's only
/// job is to shuttle bytes for a little while and then get out of the way,
/// so a tokio runtime would be pure overhead here.
pub fn run_relay_and_exec(unix_path: &std::path::Path, port: u16, program: &str, args: &[String]) -> ! {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::net::UnixStream;

    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("__egress-relay: cannot bind loopback:{port} inside sandbox: {e}");
            std::process::exit(126);
        }
    };
    let unix_path = unix_path.to_path_buf();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(client) = conn else { continue };
            let unix_path = unix_path.clone();
            std::thread::spawn(move || {
                let Ok(upstream) = UnixStream::connect(&unix_path) else { return };
                let mut c_read = client.try_clone().expect("clone tcp stream");
                let mut c_write = client;
                let mut u_read = upstream.try_clone().expect("clone unix stream");
                let mut u_write = upstream;
                let t1 = std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = c_read.read(&mut buf) {
                        if n == 0 || u_write.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
                let mut buf = [0u8; 8192];
                while let Ok(n) = u_read.read(&mut buf) {
                    if n == 0 || c_write.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = t1.join();
            });
        }
    });

    let status = std::process::Command::new(program).args(args).status();
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("__egress-relay: failed to exec {program}: {e}");
            std::process::exit(127);
        }
    }
}
