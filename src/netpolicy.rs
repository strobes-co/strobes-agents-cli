//! The network-egress scope enforced by the sandbox.
//!
//! A single [`NetworkPolicy`] answers one question — "may this connection to
//! `host:port` proceed?" — for every command the AI issues during this
//! process's lifetime. It is set once at startup from `--net-allow` /
//! `--net-deny` / `--net-default` and held in [`POLICY`], the Rust analogue
//! of the module-level singleton `strobes-bridge`'s `netpolicy.py` uses: one
//! process, one answer to "what is enforced," read by [`egress_proxy`] on
//! every connection attempt.
//!
//! Cloud-metadata and link-local destinations are denied **unconditionally**,
//! before the configured policy is even consulted — this is deliberately not
//! something `--net-allow` can override, because an SSRF that reaches
//! `169.254.169.254` and steals instance credentials is a materially worse
//! outcome than an SSRF that reaches an in-scope host. Loopback is NOT
//! carved out the same way: reaching a local service is a scope decision the
//! operator gets to make, same as `strobes-bridge`'s proxy lane.

use std::sync::{LazyLock, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Allow,
    Deny,
}

/// One rule in an allow/deny list: an exact host, a `*.suffix` wildcard, or a
/// bare IPv4/IPv6 literal. Deliberately not a full CIDR/glob engine — the CLI
/// flags mirror `strobes-bridge`'s own `--net-allow`/`--net-deny` grammar,
/// which is this same small rule shape.
#[derive(Clone)]
pub enum HostRule {
    Exact(String),
    /// `*.example.com` — matches any subdomain, not the bare domain itself.
    WildcardSuffix(String),
    Ip(std::net::IpAddr),
}

impl HostRule {
    pub fn parse(raw: &str) -> HostRule {
        let raw = raw.trim();
        if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
            return HostRule::Ip(ip);
        }
        if let Some(suffix) = raw.strip_prefix("*.") {
            return HostRule::WildcardSuffix(suffix.to_lowercase());
        }
        HostRule::Exact(raw.to_lowercase())
    }

    fn matches(&self, host: &str, ip: Option<std::net::IpAddr>) -> bool {
        match self {
            HostRule::Exact(h) => host.eq_ignore_ascii_case(h),
            // Deliberately does NOT match the bare suffix itself — `*.example.com`
            // means "any subdomain," not "example.com or any subdomain." Write
            // both rules explicitly if both are intended.
            HostRule::WildcardSuffix(suffix) => host.to_lowercase().ends_with(&format!(".{suffix}")),
            HostRule::Ip(rule_ip) => ip.map(|resolved| &resolved == rule_ip).unwrap_or(false),
        }
    }
}

#[derive(Clone, Debug)]
pub struct NetworkPolicy {
    pub allow: Vec<HostRule>,
    pub deny: Vec<HostRule>,
    pub default: Action,
}

impl NetworkPolicy {
    /// No `--net-*` flags given: every command still runs, unrestricted
    /// except for the unconditional cloud-metadata/link-local deny below —
    /// matches `strobes-bridge`'s "open by default" stance so an
    /// unconfigured CLI keeps working exactly as it did before this feature.
    pub fn open() -> Self {
        NetworkPolicy {
            allow: vec![],
            deny: vec![],
            default: Action::Allow,
        }
    }

    pub fn from_flags(allow: &[String], deny: &[String], default: Action) -> Self {
        NetworkPolicy {
            allow: allow.iter().map(|s| HostRule::parse(s)).collect(),
            deny: deny.iter().map(|s| HostRule::parse(s)).collect(),
            default,
        }
    }

    /// Decide whether `host:port` may be reached, resolving `host` to an IP
    /// first (rules can be written as either) — `resolved` is passed in
    /// rather than resolved here so callers reuse whatever lookup the proxy
    /// already had to do to actually connect.
    pub fn decide(&self, host: &str, resolved: Option<std::net::IpAddr>) -> Decision {
        if let Some(reason) = always_denied(host, resolved) {
            return Decision::Deny(reason);
        }
        if let Some(rule) = self.deny.iter().find(|r| r.matches(host, resolved)) {
            return Decision::Deny(format!("denied by rule {rule:?}"));
        }
        if self.allow.iter().any(|r| r.matches(host, resolved)) {
            return Decision::Allow;
        }
        match self.default {
            Action::Allow => Decision::Allow,
            Action::Deny => Decision::Deny("default-deny: not in --net-allow".to_string()),
        }
    }
}

impl std::fmt::Debug for HostRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostRule::Exact(h) => write!(f, "{h}"),
            HostRule::WildcardSuffix(s) => write!(f, "*.{s}"),
            HostRule::Ip(ip) => write!(f, "{ip}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Carries a human-readable reason, mirroring `strobes-bridge`'s
    /// `egress_denied` result field: a refusal that names the destination
    /// and the rule is something an agent can act on (retarget); a bare
    /// timeout or connection-refused just makes it retry the same thing.
    Deny(String),
}

/// AWS/GCP/Azure/OCI instance-metadata endpoints and the whole link-local
/// block, denied regardless of policy. `169.254.0.0/16` covers every cloud's
/// metadata IP (`169.254.169.254` for AWS/GCP/Azure, `169.254.169.254` for
/// OCI too) since link-local addressing is exactly where clouds park it.
fn always_denied(host: &str, resolved: Option<std::net::IpAddr>) -> Option<String> {
    let host_lc = host.to_lowercase();
    if host_lc == "metadata.google.internal" || host_lc == "metadata" {
        return Some("cloud metadata hostname (always denied)".to_string());
    }
    let check_ip = |ip: &std::net::IpAddr| -> bool {
        match ip {
            std::net::IpAddr::V4(v4) => v4.is_link_local() || v4.octets()[0..2] == [169, 254],
            std::net::IpAddr::V6(v6) => {
                v6.is_unicast_link_local()
                    // fd00:ec2::254 — AWS's IMDS IPv6 address.
                    || v6.segments()[0] == 0xfd00 && v6.segments()[1] == 0xec2
            }
        }
    };
    if let Ok(literal) = host.parse::<std::net::IpAddr>() {
        if check_ip(&literal) {
            return Some("link-local / cloud-metadata address (always denied)".to_string());
        }
    }
    if let Some(ip) = resolved {
        if check_ip(&ip) {
            return Some("resolves to a link-local / cloud-metadata address (always denied)".to_string());
        }
    }
    None
}

pub static POLICY: LazyLock<RwLock<NetworkPolicy>> =
    LazyLock::new(|| RwLock::new(NetworkPolicy::open()));

pub fn set_policy(policy: NetworkPolicy) {
    *POLICY.write().unwrap() = policy;
}

pub fn decide(host: &str, resolved: Option<std::net::IpAddr>) -> Decision {
    POLICY.read().unwrap().decide(host, resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_policy_allows_everything_except_metadata() {
        let p = NetworkPolicy::open();
        assert_eq!(p.decide("example.com", None), Decision::Allow);
        assert!(matches!(p.decide("169.254.169.254", None), Decision::Deny(_)));
        assert!(matches!(
            p.decide("metadata.google.internal", None),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn metadata_denied_even_when_explicitly_allowed() {
        let p = NetworkPolicy::from_flags(&["169.254.169.254".into()], &[], Action::Deny);
        assert!(matches!(p.decide("169.254.169.254", None), Decision::Deny(_)));
    }

    #[test]
    fn deny_rule_wins_over_allow_rule() {
        let p = NetworkPolicy::from_flags(
            &["*.example.com".into()],
            &["evil.example.com".into()],
            Action::Deny,
        );
        assert!(matches!(p.decide("evil.example.com", None), Decision::Deny(_)));
        assert_eq!(p.decide("good.example.com", None), Decision::Allow);
    }

    #[test]
    fn default_deny_blocks_unlisted_hosts() {
        let p = NetworkPolicy::from_flags(&["allowed.com".into()], &[], Action::Deny);
        assert_eq!(p.decide("allowed.com", None), Decision::Allow);
        assert!(matches!(p.decide("other.com", None), Decision::Deny(_)));
    }

    #[test]
    fn wildcard_does_not_match_bare_domain() {
        let rule = HostRule::parse("*.example.com");
        assert!(!rule.matches("example.com", None));
        assert!(rule.matches("api.example.com", None));
    }

    #[test]
    fn link_local_ip_literal_always_denied() {
        let p = NetworkPolicy::open();
        assert!(matches!(p.decide("169.254.1.1", None), Decision::Deny(_)));
    }

    #[test]
    fn resolved_metadata_ip_denied_even_for_an_unrelated_hostname() {
        let p = NetworkPolicy::open();
        let ip: std::net::IpAddr = "169.254.169.254".parse().unwrap();
        assert!(matches!(p.decide("sneaky.example.com", Some(ip)), Decision::Deny(_)));
    }
}
