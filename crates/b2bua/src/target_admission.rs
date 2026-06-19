//! TargetAdmission — pre-flight check on b-leg destinations. Port of
//! `src/b2bua/TargetAdmission.ts`.
//!
//! The b2bua worker accepts call-control's `route.destination.host` verbatim and
//! hands it to the send path. A bogus host (e.g. `kindlab` from a misconfigured
//! fixture, or a `.svc.cluster.local` name the runner constructs) flows through
//! to the UDP send path, where — in the TS source — Node's `dns.lookup` blocks
//! the libuv threadpool for ~5 s on `EAI_AGAIN`.
//!
//! This module rejects targets whose host is neither an IP literal nor in the
//! allow-list at the **decision boundary** — before any state is allocated for
//! the b-leg. The admission gate emits a `503` and terminates the call. The
//! proxy's buffered send path quarantines the blocking even when admission lets
//! a doomed target through; admission is the cheap early filter.
//!
//! Allow-list semantics (`is_allowed_suffix`):
//!   - IPv4/IPv6 literals always pass (no DNS needed) — `is_ip_literal`.
//!   - A list containing the literal `"*"` matches every host (rollback
//!     sentinel; restores pre-admission behaviour without a redeploy).
//!   - Otherwise the host must end with one of the suffixes (case-insensitive).
//!     The suffix is matched verbatim — operators write `.svc.cluster.local`
//!     (with leading dot) to constrain to subdomains and avoid matching
//!     `example.svc.cluster.local.evil.com`.

use std::net::IpAddr;
use std::str::FromStr;

/// The verdict for a destination host against the suffix allow-list. Port of the
/// TS string union `"ip-literal" | "allow-listed" | "reject"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionVerdict {
    /// Short-circuit accept — the host is an IP literal, so no DNS is required at
    /// send time (regardless of the suffix list).
    IpLiteral,
    /// The host matched the configured suffix policy (a suffix or the `*` wildcard).
    AllowListed,
    /// Neither an IP literal nor a configured suffix — the admission gate emits a
    /// `503` and terminates the call.
    Reject,
}

/// Returns true if `host` is a valid IPv4 or IPv6 literal. Strips a single pair
/// of brackets (for IPv6 in URI form like `[::1]`).
///
/// Mirrors the TS `isIpLiteral` (Node's `net.isIP`): `IpAddr::from_str` accepts
/// exactly the dotted-quad IPv4 and the canonical IPv6 forms and rejects out-of-
/// range octets (`999.999.999.999`), an unclosed bracket (`[broken`), the empty
/// string, and ordinary hostnames.
///
/// The two oracles agree on every host shape that occurs in this system (K8s
/// FQDNs are never IPs; pod IPs are canonical dotted-quad / colon-hex) and on all
/// ported test vectors. They *can* diverge on a handful of inputs std accepts or
/// rejects differently from `net.isIP` (e.g. leading-zero IPv4 octets, IPv6
/// zone/scope ids, and a few platform-libc-specific forms) — **none reachable
/// here**. The Rust std parser is the deliberately chosen oracle: it is
/// intentionally slightly stricter/looser than `net.isIP` on those non-occurring
/// edge cases, so a future reader should treat the divergence as a chosen
/// definition, not a porting defect.
pub fn is_ip_literal(host: &str) -> bool {
    let stripped = if host.len() >= 2 && host.starts_with('[') && host.ends_with(']') {
        &host[1..host.len() - 1]
    } else {
        host
    };
    IpAddr::from_str(stripped).is_ok()
}

/// Returns true if any suffix in `suffixes` matches `host` (case-insensitive), or
/// if the list contains the wildcard `"*"`. Port of TS `isAllowedSuffix`.
///
/// The wildcard is checked first (so `["*"]` matches even the empty host); an
/// empty list rejects everything.
pub fn is_allowed_suffix(host: &str, suffixes: &[String]) -> bool {
    for s in suffixes {
        if s == "*" {
            return true;
        }
    }
    let lower = host.to_lowercase();
    for s in suffixes {
        if s == "*" {
            continue;
        }
        if lower.ends_with(&s.to_lowercase()) {
            return true;
        }
    }
    false
}

/// Classify a destination host against the suffix allow-list. Port of TS
/// `classifyAdmission`.
///
///   - [`AdmissionVerdict::IpLiteral`]  — short-circuit accept; no DNS will be
///     required at send time.
///   - [`AdmissionVerdict::AllowListed`] — host matches the configured suffix policy.
///   - [`AdmissionVerdict::Reject`]      — neither; the gate emits `503` and
///     terminates the call.
pub fn classify_admission(host: &str, suffixes: &[String]) -> AdmissionVerdict {
    if is_ip_literal(host) {
        return AdmissionVerdict::IpLiteral;
    }
    if is_allowed_suffix(host, suffixes) {
        return AdmissionVerdict::AllowListed;
    }
    AdmissionVerdict::Reject
}

#[cfg(test)]
mod tests {
    //! Port of `tests/b2bua/TargetAdmission.test.ts` — the pure-helper suite.
    //! The two admission WIRING sites have their own ports of the source's
    //! dedicated wiring tests: the `apply_route` decision-boundary gate by
    //! `b2bua-harness/tests/target_admission_gate.rs` (port of
    //! `apply-route-admission-reject.test.ts`, incl. the never-touch-the-limiter
    //! property), and the rule-path `ActionExecutor::CreateLeg` branch by
    //! `b2bua/tests/rules.rs::create_leg_admission` (port of
    //! `action-executor-create-leg-admission.test.ts`: reject + IP-literal &
    //! wildcard admit). No clock, no timers — pure functions.

    use super::*;

    // Build the `Vec<String>` the Rust signature wants from string literals, so
    // each case reads like its TS array-literal counterpart.
    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    mod is_ip_literal {
        use super::*;

        #[test]
        fn accepts_ipv4_literals() {
            assert!(is_ip_literal("10.0.0.1"));
            assert!(is_ip_literal("127.0.0.1"));
            assert!(is_ip_literal("172.20.255.250"));
        }

        #[test]
        fn accepts_ipv6_literals_bare_and_bracketed() {
            assert!(is_ip_literal("::1"));
            assert!(is_ip_literal("[::1]"));
            assert!(is_ip_literal("fe80::1"));
            assert!(is_ip_literal("[2001:db8::1]"));
        }

        #[test]
        fn rejects_hostnames() {
            assert!(!is_ip_literal("kindlab"));
            assert!(!is_ip_literal("worker-0.b2bua.svc.cluster.local"));
            assert!(!is_ip_literal("example.com"));
        }

        #[test]
        fn rejects_malformed_strings() {
            assert!(!is_ip_literal(""));
            assert!(!is_ip_literal("not.an.ip"));
            assert!(!is_ip_literal("999.999.999.999"));
            assert!(!is_ip_literal("[broken"));
        }
    }

    mod is_allowed_suffix {
        use super::*;

        #[test]
        fn matches_case_insensitively() {
            assert!(is_allowed_suffix(
                "worker.svc.cluster.local",
                &list(&[".svc.cluster.local"])
            ));
            assert!(is_allowed_suffix(
                "WORKER.SVC.CLUSTER.LOCAL",
                &list(&[".svc.cluster.local"])
            ));
        }

        #[test]
        fn requires_the_suffix_to_actually_match_the_tail() {
            assert!(!is_allowed_suffix("example.com", &list(&[".svc.cluster.local"])));
            assert!(!is_allowed_suffix(
                "svc.cluster.local.evil.com",
                &list(&[".svc.cluster.local"])
            ));
        }

        #[test]
        fn treats_star_as_wildcard_regardless_of_host() {
            assert!(is_allowed_suffix("kindlab", &list(&["*"])));
            assert!(is_allowed_suffix("anything.example", &list(&["*"])));
            assert!(is_allowed_suffix("", &list(&["*"])));
        }

        #[test]
        fn supports_multiple_suffixes() {
            let l = list(&[".svc.cluster.local", ".example.test"]);
            assert!(is_allowed_suffix("a.svc.cluster.local", &l));
            assert!(is_allowed_suffix("b.example.test", &l));
            assert!(!is_allowed_suffix("c.elsewhere", &l));
        }

        #[test]
        fn empty_list_rejects_everything() {
            assert!(!is_allowed_suffix("anything", &list(&[])));
        }
    }

    mod classify_admission {
        use super::*;

        #[test]
        fn returns_ip_literal_for_ip_hosts_regardless_of_suffix_list() {
            assert_eq!(classify_admission("10.0.0.1", &list(&[])), AdmissionVerdict::IpLiteral);
            assert_eq!(
                classify_admission("[::1]", &list(&[".svc.cluster.local"])),
                AdmissionVerdict::IpLiteral
            );
        }

        #[test]
        fn returns_allow_listed_when_the_suffix_matches() {
            assert_eq!(
                classify_admission("worker.svc.cluster.local", &list(&[".svc.cluster.local"])),
                AdmissionVerdict::AllowListed
            );
        }

        #[test]
        fn returns_reject_for_non_ip_non_matching_hostnames() {
            assert_eq!(
                classify_admission("kindlab", &list(&[".svc.cluster.local"])),
                AdmissionVerdict::Reject
            );
            assert_eq!(
                classify_admission("example.com", &list(&[".svc.cluster.local"])),
                AdmissionVerdict::Reject
            );
        }

        #[test]
        fn star_wildcard_short_circuits_to_allow_listed_for_non_ip() {
            assert_eq!(
                classify_admission("kindlab", &list(&["*"])),
                AdmissionVerdict::AllowListed
            );
        }
    }
}
