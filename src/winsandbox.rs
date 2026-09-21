//! Windows egress confinement — **not implemented yet**.
//!
//! `strobes-bridge`'s target design for this backend (see its
//! `winsandbox.py`) is a dedicated low-privilege local account fenced by a
//! Windows Filtering Platform rule (via `New-NetFirewallRule`, matched on
//! the account's SID so it follows every child process) — there is no
//! process-sandbox primitive on Windows equivalent to Seatbelt or
//! bubblewrap, so the boundary has to be drawn around an identity instead
//! of a process tree.
//!
//! That backend is deliberately **not** ported in this change: it would be
//! real code that has never once been executed against an actual Windows
//! host, which is a worse trap than an honest "not implemented" — a sandbox
//! that silently fails to sandbox is the exact failure mode this whole
//! feature exists to prevent. Every entry point here returns
//! [`SandboxUnavailable`] unconditionally, matching the sandbox's own
//! no-unsandboxed-fallback principle, so `run_shell`/`run_code` refuse
//! rather than running a Windows command with unrestricted egress.
//!
//! Import-safe on every platform (no `#[cfg(windows)]` gate on the module
//! itself) so it can be referenced from [`crate::procsandbox`]'s
//! non-macOS/non-Linux fallback without conditional-compilation gymnastics.

use crate::egress_proxy::EgressProxy;
use crate::procsandbox::{Confined, SandboxUnavailable};

const NOT_IMPLEMENTED: &str = "Windows egress sandbox is not implemented yet — \
    see strobes-bridge's winsandbox.py for the target design (dedicated \
    local account + WFP firewall rule). Refusing to run this command \
    unsandboxed rather than pretending to confine it.";

pub fn confine(_program: &str, _args: &[String], _proxy: &EgressProxy) -> Result<Confined, SandboxUnavailable> {
    Err(SandboxUnavailable(NOT_IMPLEMENTED.to_string()))
}

/// `strobes sandbox-setup` on Windows — reports the same "not implemented"
/// rather than attempting elevated account/firewall setup for a backend
/// that doesn't exist.
pub fn setup() -> Result<String, SandboxUnavailable> {
    Err(SandboxUnavailable(NOT_IMPLEMENTED.to_string()))
}

pub fn teardown() -> Result<String, SandboxUnavailable> {
    Err(SandboxUnavailable(NOT_IMPLEMENTED.to_string()))
}
