//! The local proxy that turns [`netpolicy`] into an enforced decision.
//!
//! One instance lives for the life of the `strobes` process (started lazily
//! by [`crate::sandbox`] on the first command that needs confining, dropped
//! when the process exits) — there is no persistent daemon here the way
//! `strobes-bridge` has one, so the proxy's lifetime is the CLI invocation's
//! lifetime rather than a system service's. It speaks plain HTTP forward
//! proxying and `CONNECT` tunneling (covers `HTTP_PROXY`/`HTTPS_PROXY`, which
//! is what every well-behaved CLI tool — curl, python-requests, node's
//! fetch, etc. — actually honors), listening on loopback only.
//!
//! On Linux it also listens on a Unix domain socket at the same time as the
//! TCP port: a fresh network namespace (`bwrap --unshare-net`) has its own
//! private loopback, so a sandboxed process's `127.0.0.1:<port>` cannot
//! reach this process's `127.0.0.1:<port>` in the *host* namespace — but a
//! bind-mounted *file* (the Unix socket) crosses a network-namespace
//! boundary fine, since it is a filesystem object, not a network one. See
//! [`crate::procsandbox`] for the relay that presents that socket back as a
//! local TCP port inside the sandbox.

use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct EgressProxy {
    pub tcp_addr: SocketAddr,
    #[cfg(unix)]
    pub unix_path: Option<std::path::PathBuf>,
}

impl EgressProxy {
    /// Bind and start accepting connections in the background. Returns once
    /// listening — the accept loop runs for the rest of the process.
    ///
    /// Both the listener sockets *and* the tasks that poll them are created
    /// entirely inside [`background_runtime`], not whichever runtime
    /// happens to be calling `start()`. That's not just about where the
    /// task is scheduled: a `TcpListener`/`UnixListener` internally
    /// registers itself with whichever runtime's I/O driver is current
    /// *at bind time*, so a listener merely handed off to another runtime
    /// via `spawn` still points at its *original* runtime's I/O driver —
    /// and once that original runtime is dropped, every `accept()` on it
    /// fails immediately and permanently ("a Tokio 1.x context was found,
    /// but it is being shutdown"), even though the task polling it lives
    /// on a different, still-alive runtime. [`sandbox::confine`] lazily
    /// initializes this proxy on whichever caller happens to need it
    /// first, and that caller's own runtime is not guaranteed to outlive
    /// the proxy (most visibly in tests: every `#[tokio::test]` function
    /// owns its own runtime, torn down the moment that test returns) — so
    /// the bind calls themselves have to happen on the runtime meant to
    /// outlive all of them, not just the tasks spawned afterward.
    pub async fn start() -> io::Result<EgressProxy> {
        background_runtime()
            .spawn(async move {
                let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
                let tcp_addr = listener.local_addr()?;
                tokio::spawn(accept_loop(listener));

                #[cfg(unix)]
                let unix_path = {
                    // AF_UNIX paths are capped at ~104 bytes (`sun_path`);
                    // `/tmp` directly (not `std::env::temp_dir()`, which on
                    // macOS resolves to a much longer
                    // `/private/var/folders/.../T/` path that blows past
                    // that limit) plus a short id keeps this well clear.
                    let path = std::path::PathBuf::from("/tmp").join(format!(
                        "strobes-egress-{}-{:x}.sock",
                        std::process::id(),
                        uuid::Uuid::new_v4().as_u128() as u32
                    ));
                    match tokio::net::UnixListener::bind(&path) {
                        Ok(unix_listener) => {
                            tokio::spawn(accept_loop_unix(unix_listener));
                            Some(path)
                        }
                        // Non-fatal: only the Linux procsandbox backend
                        // needs this; macOS/Seatbelt reaches the TCP port
                        // directly.
                        Err(e) => {
                            tracing_unavailable(&e);
                            None
                        }
                    }
                };

                Ok::<EgressProxy, io::Error>(EgressProxy {
                    tcp_addr,
                    #[cfg(unix)]
                    unix_path,
                })
            })
            .await
            .unwrap_or_else(|join_err| Err(io::Error::other(join_err.to_string())))
    }

    /// `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` value pointing at this proxy,
    /// for launch sites that hand a sandboxed child its environment
    /// directly (the macOS/no-sandbox paths — Linux's confined child reaches
    /// the *relay's* port instead, see [`crate::procsandbox`]).
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.tcp_addr)
    }
}

/// A dedicated OS thread running its own tokio runtime for the rest of the
/// process, used only to host the proxy's accept loops — see the note on
/// [`EgressProxy::start`] for why this can't just be the caller's runtime.
/// `Handle::spawn` works from any thread/runtime, so every other part of
/// the program keeps using its own runtime as normal; this is purely a
/// stable home for these two background tasks.
fn background_runtime() -> tokio::runtime::Handle {
    static HANDLE: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();
    HANDLE
        .get_or_init(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build egress_proxy background runtime");
            let handle = rt.handle().clone();
            std::thread::spawn(move || {
                // Never resolves — this thread's only job is to keep `rt`
                // alive so tasks spawned on `handle` keep running.
                rt.block_on(std::future::pending::<()>());
            });
            handle
        })
        .clone()
}

fn tracing_unavailable(e: &io::Error) {
    eprintln!("egress_proxy: unix socket unavailable ({e}); Linux sandbox lane will be unavailable");
}

async fn accept_loop(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream).await {
                        log_conn_error(&e);
                    }
                });
            }
            Err(e) => {
                log_conn_error(&e);
            }
        }
    }
}

#[cfg(unix)]
async fn accept_loop_unix(listener: tokio::net::UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream).await {
                        log_conn_error(&e);
                    }
                });
            }
            Err(e) => log_conn_error(&e),
        }
    }
}

fn log_conn_error(e: &io::Error) {
    // Connection-level noise (client disconnect mid-request, etc.) — never
    // fatal to the proxy itself, so this stays a log line, not a panic.
    if e.kind() != io::ErrorKind::UnexpectedEof && e.kind() != io::ErrorKind::ConnectionReset {
        eprintln!("egress_proxy: connection error: {e}");
    }
}

/// A parsed proxy request: either `CONNECT host:port` (tunneling, used for
/// HTTPS) or a plain forward-proxy request with an absolute-URI request
/// line (used for HTTP).
struct ProxyRequest {
    is_connect: bool,
    host: String,
    port: u16,
    /// Everything read from the client before we knew where it was going —
    /// for a plain HTTP request this is the (rewritten) request we still
    /// need to forward; for CONNECT it's discarded once we've replied.
    head: Vec<u8>,
    /// The request line + headers, with the request line's absolute-URI
    /// rewritten to origin-form, ready to write to the upstream. `None` for
    /// CONNECT, which has nothing to forward.
    rewritten_head: Option<Vec<u8>>,
}

async fn read_request_head<S: AsyncReadExt + Unpin>(stream: &mut S) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 1024];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > 64 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "proxy request head too large"));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed before full request head"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn parse_request(head: &[u8]) -> io::Result<ProxyRequest> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(target, 443)?;
        return Ok(ProxyRequest {
            is_connect: true,
            host,
            port,
            head: head.to_vec(),
            rewritten_head: None,
        });
    }

    // Plain forward proxy: target is an absolute URI, e.g.
    // `http://example.com/path`. Extract host:port and rewrite the request
    // line to origin-form (`/path`) the way a real forward proxy must,
    // since the upstream server expects a normal relative request.
    let url = target
        .parse::<url::Url>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad proxy target {target:?}: {e}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no host in proxy target"))?
        .to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    let origin_form = {
        let mut s = url.path().to_string();
        if let Some(q) = url.query() {
            s.push('?');
            s.push_str(q);
        }
        if s.is_empty() {
            s.push('/');
        }
        s
    };
    let new_request_line = format!(
        "{method} {origin_form} {}",
        request_line.rsplit(' ').next().unwrap_or("HTTP/1.1")
    );
    let mut rewritten = new_request_line.into_bytes();
    rewritten.extend_from_slice(b"\r\n");
    // Re-emit every header line unchanged (skip the request line itself).
    for line in lines {
        rewritten.extend_from_slice(line.as_bytes());
        rewritten.extend_from_slice(b"\r\n");
    }

    Ok(ProxyRequest {
        is_connect: false,
        host,
        port,
        head: head.to_vec(),
        rewritten_head: Some(rewritten),
    })
}

fn split_host_port(target: &str, default_port: u16) -> io::Result<(String, u16)> {
    if let Some((h, p)) = target.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            return Ok((h.to_string(), port));
        }
    }
    Ok((target.to_string(), default_port))
}

async fn handle_conn<S>(mut client: S) -> io::Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let head = read_request_head(&mut client).await?;
    let req = match parse_request(&head) {
        Ok(r) => r,
        Err(e) => {
            let _ = client
                .write_all(format!("HTTP/1.1 400 Bad Request\r\n\r\n{e}").as_bytes())
                .await;
            return Err(e);
        }
    };

    let resolved = resolve_first(&req.host, req.port).await;
    let decision = crate::netpolicy::decide(&req.host, resolved);
    if let crate::netpolicy::Decision::Deny(reason) = decision {
        let body = format!("egress denied: {}:{} — {reason}", req.host, req.port);
        let response = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        client.write_all(response.as_bytes()).await?;
        return Ok(());
    }

    let mut upstream = TcpStream::connect((req.host.as_str(), req.port)).await?;

    if req.is_connect {
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    } else if let Some(rewritten) = &req.rewritten_head {
        upstream.write_all(rewritten).await?;
    }

    splice(client, upstream).await
}

async fn splice<C, U>(mut client: C, mut upstream: U) -> io::Result<()>
where
    C: AsyncReadExt + AsyncWriteExt + Unpin,
    U: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut client_buf = [0u8; 8192];
    let mut upstream_buf = [0u8; 8192];
    loop {
        tokio::select! {
            r = client.read(&mut client_buf) => {
                match r? {
                    0 => break,
                    n => upstream.write_all(&client_buf[..n]).await?,
                }
            }
            r = upstream.read(&mut upstream_buf) => {
                match r? {
                    0 => break,
                    n => client.write_all(&upstream_buf[..n]).await?,
                }
            }
        }
    }
    Ok(())
}

async fn resolve_first(host: &str, port: u16) -> Option<std::net::IpAddr> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Some(ip);
    }
    tokio::net::lookup_host((host, port))
        .await
        .ok()
        .and_then(|mut addrs| addrs.next())
        .map(|addr| addr.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_target() {
        let req = parse_request(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n").unwrap();
        assert!(req.is_connect);
        assert_eq!(req.host, "example.com");
        assert_eq!(req.port, 443);
    }

    #[test]
    fn parses_and_rewrites_plain_get() {
        let req = parse_request(b"GET http://example.com/a/b?x=1 HTTP/1.1\r\nHost: example.com\r\n\r\n").unwrap();
        assert!(!req.is_connect);
        assert_eq!(req.host, "example.com");
        assert_eq!(req.port, 80);
        let rewritten = String::from_utf8(req.rewritten_head.unwrap()).unwrap();
        assert!(rewritten.starts_with("GET /a/b?x=1 HTTP/1.1\r\n"));
        assert!(rewritten.contains("Host: example.com"));
    }

    #[tokio::test]
    async fn denies_out_of_scope_and_allows_in_scope() {
        crate::netpolicy::set_policy(crate::netpolicy::NetworkPolicy::from_flags(
            &["allowed.example".into()],
            &[],
            crate::netpolicy::Action::Deny,
        ));
        assert!(matches!(
            crate::netpolicy::decide("blocked.example", None),
            crate::netpolicy::Decision::Deny(_)
        ));
        assert_eq!(
            crate::netpolicy::decide("allowed.example", None),
            crate::netpolicy::Decision::Allow
        );
        // Reset for other tests in this process.
        crate::netpolicy::set_policy(crate::netpolicy::NetworkPolicy::open());
    }
}
