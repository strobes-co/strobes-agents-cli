//! The execution lane — every AI-issued command runs here, under egress scope.
//!
//! Ties [`crate::netpolicy`] (the scope), [`crate::egress_proxy`] (the
//! enforcement point) and [`crate::procsandbox`]/[`crate::winsandbox`] (the
//! OS mechanism that makes the proxy unavoidable) into one entry point,
//! [`confine`], that `local.rs`'s `run_shell`/`run_code` call before
//! spawning anything.
//!
//! The proxy is started lazily, once, the first time a command needs
//! confining, and lives for the rest of this `strobes` process — there is
//! no persistent daemon to anchor it to the way `strobes-bridge` has one,
//! so "the life of the service" becomes "the life of this CLI invocation."
//! That still covers every tool call made during one `chat`/`send` session,
//! which is the same unit of work the daemon protects per-connection.

use crate::egress_proxy::EgressProxy;
use crate::procsandbox::{Confined, SandboxUnavailable};
use std::sync::Arc;
use tokio::sync::OnceCell;

static PROXY: OnceCell<Arc<EgressProxy>> = OnceCell::const_new();

async fn proxy() -> Result<Arc<EgressProxy>, SandboxUnavailable> {
    PROXY
        .get_or_try_init(|| async {
            EgressProxy::start()
                .await
                .map(Arc::new)
                .map_err(|e| SandboxUnavailable(format!("failed to start egress proxy: {e}")))
        })
        .await
        .cloned()
}

/// Rewrite `program`/`args` to run under the sandbox. Callers spawn the
/// returned [`Confined`] instead of the original command, and merge its
/// `env` into the child's environment. Returns [`SandboxUnavailable`] when
/// no backend can enforce the policy on this host — callers must treat that
/// as a hard failure, never fall back to running the original command
/// unsandboxed.
pub async fn confine(program: &str, args: &[String]) -> Result<Confined, SandboxUnavailable> {
    let p = proxy().await?;
    crate::procsandbox::confine(program, args, &p)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LaneMode {
    /// Enforcement happens in the local HTTP/CONNECT proxy: covers any tool
    /// that goes through an HTTP client honoring `HTTP_PROXY`. Raw sockets
    /// (a port scanner's SYN probes, bare `nc`) get no network at all under
    /// this lane, rather than an unfiltered path OR a misleadingly "clean"
    /// scan — see `raw_sockets` below, which is what a caller must check
    /// before trusting a scanner's output.
    Proxy,
}

/// What a caller needs to interpret a command's network-facing output
/// correctly. A port scanner run under [`LaneMode::Proxy`] with
/// `raw_sockets: false` reaches nothing and will report every port
/// "filtered" — a fact about the lane, not something worth guessing from
/// the command's text (`strobes-bridge` learned this the hard way: a
/// hardcoded tool-name denylist was trivially defeated by `cp $(which
/// nmap) /tmp/x && /tmp/x`, and silently missed every scanner not on the
/// list).
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct LaneCapability {
    pub mode: LaneMode,
    pub raw_sockets: bool,
    pub udp: bool,
}

/// This port ships the proxy lane only — no `nftables`-keyed packet lane
/// yet (that's the documented Linux follow-up, mirroring
/// `strobes-bridge`'s `l3lane.py`), so `raw_sockets`/`udp` are always false
/// today. Once a packet lane lands, this becomes host-capability-dependent
/// the same way the Python side's `mode` field is.
pub fn lane_capability() -> LaneCapability {
    LaneCapability {
        mode: LaneMode::Proxy,
        raw_sockets: false,
        udp: false,
    }
}

#[derive(Debug, serde::Serialize)]
pub struct CheckResult {
    pub backend_available: bool,
    pub out_of_scope_denied: bool,
    pub in_scope_allowed: bool,
    pub detail: String,
}

/// `strobes sandbox-check`: proves the lane both ways using a **temporary**
/// two-rule policy independent of whatever scope the operator has actually
/// configured, then restores it — a lane passing zero traffic would
/// otherwise look identical to one correctly enforcing, so denying is not
/// enough on its own; an allowed connection has to keep working too.
pub async fn sandbox_check() -> CheckResult {
    let saved = crate::netpolicy::POLICY.read().unwrap().clone();
    crate::netpolicy::set_policy(crate::netpolicy::NetworkPolicy::from_flags(
        &["one.one.one.one".to_string(), "1.1.1.1".to_string()],
        &[],
        crate::netpolicy::Action::Deny,
    ));

    let probe = |program: &'static str, args: Vec<String>| async move {
        match confine(program, &args).await {
            Ok(c) => {
                let mut cmd = tokio::process::Command::new(&c.program);
                cmd.args(&c.args).envs(c.env);
                cmd.output().await.ok()
            }
            Err(_) => None,
        }
    };

    // `-f`: fail (non-zero exit) on a non-2xx response instead of printing
    // it as if it were the page — without this, our own proxy's "403
    // Forbidden, egress denied" body would print successfully and exit 0,
    // looking exactly like an allowed request. `-s` still suppresses
    // progress output either way.
    let denied_out = probe(
        "curl",
        vec!["-sf".into(), "-m".into(), "5".into(), "http://example.com".into()],
    )
    .await;
    // A host explicitly allowed by the temporary policy.
    let allowed_out = probe(
        "curl",
        vec!["-sf".into(), "-m".into(), "5".into(), "http://1.1.1.1".into()],
    )
    .await;

    crate::netpolicy::set_policy(saved);

    let backend_available = denied_out.is_some() || allowed_out.is_some();
    let out_of_scope_denied = denied_out.as_ref().map(|o| !o.status.success()).unwrap_or(false);
    let in_scope_allowed = allowed_out.as_ref().map(|o| o.status.success()).unwrap_or(false);

    let detail = if !backend_available {
        "no sandbox backend available on this host — commands will refuse to run rather than execute unsandboxed".to_string()
    } else {
        format!(
            "out-of-scope denied: {out_of_scope_denied}, in-scope still works: {in_scope_allowed}"
        )
    };

    CheckResult {
        backend_available,
        out_of_scope_denied,
        in_scope_allowed,
        detail,
    }
}

#[cfg(test)]
mod lane_tests {
    /// The core property this whole feature exists for: a tool that knows
    /// nothing about `HTTP_PROXY` gets no network at all, not an unfiltered
    /// path. Verified against `nc` for the same reason `strobes-bridge`'s
    /// own test suite does — it is about as proxy-blind as a tool gets.
    #[tokio::test]
    async fn nc_gets_no_network_under_the_sandbox() {
        let confined = super::confine("nc", &["-G".into(), "3".into(), "1.1.1.1".into(), "80".into()])
            .await
            .expect("sandbox should be available on this test host");
        let out = tokio::process::Command::new(&confined.program)
            .args(&confined.args)
            .envs(confined.env)
            .output()
            .await
            .expect("spawn");
        assert!(!out.status.success(), "nc should have failed to connect under the sandbox");
    }

    /// The other half of the same proof: the sandbox does not simply drop
    /// all traffic (which would make `nc` "failing" above meaningless) — a
    /// proxy-aware tool asking for something in scope still gets through.
    ///
    /// Pins its own policy explicitly rather than trusting the ambient
    /// default: `netpolicy::POLICY` is process-global and another test
    /// (`egress_proxy::tests::denies_out_of_scope_and_allows_in_scope`) runs
    /// concurrently and mutates it.
    #[tokio::test]
    async fn proxy_aware_tool_still_reaches_an_in_scope_host() {
        let saved = crate::netpolicy::POLICY.read().unwrap().clone();
        crate::netpolicy::set_policy(crate::netpolicy::NetworkPolicy::open());

        let confined = super::confine(
            "curl",
            &["-sf".into(), "-m".into(), "10".into(), "http://1.1.1.1".into()],
        )
        .await
        .expect("sandbox should be available on this test host");
        let out = tokio::process::Command::new(&confined.program)
            .args(&confined.args)
            .envs(confined.env)
            .output()
            .await
            .expect("spawn");
        crate::netpolicy::set_policy(saved);
        assert!(
            out.status.success(),
            "an in-scope curl should still succeed under the sandbox: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
