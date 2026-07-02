//! The layout-owned **egress rewrite** (ADR-0018): the generic transform from a
//! Callflow shape's *logical* outgoing INVITE ("alice calls bob1") to the
//! *actual* INVITE put on a given topology's wire, plus the generic resolution of
//! *any* logical callee role ([`CalleeTarget`]) — the a-leg target, a reroute
//! candidate, or a REFER transfer target — to how this layout addresses it. Each
//! [`InfraShape`] declares an [`EgressPolicy`]; the runtime resolves callees via
//! [`InfraRuntime::callee`] and shapes apply the rewrite via
//! [`InfraRuntime::outgoing_invite`].
//!
//! This is the seam the brief asks for — *"a generic invite-message-from-test →
//! invite-sent function that rewrites whatever is needed"* — and it is **callee-
//! generic**: a shape never hard-codes `cfg.addr("bob2")` or an AOR for any
//! target. It replaces the per-shape, hand-coded
//! `if let Some(dest) = rt.api_call_destination()` + From/To/R-URI block, so the
//! **layout** owns routing:
//!
//!   - **real cluster** ([`EgressPolicy::ApiCallPin`]) attaches the proprietary
//!     [`ApiCall`] control header: a single `destination` pin for one callee, or
//!     a `routes` failover plan for an ordered candidate list (rerouting);
//!   - **register front proxy** ([`EgressPolicy::RegistrarAor`]) rewrites the
//!     Request-URI to the (primary) callee's registered AOR — pure SIP routing;
//!   - the **fake LB + b2bua** and **direct-peer** infras are
//!     [`EgressPolicy::Transparent`]: the scripted engine / direct address
//!     already reaches the callee (and owns failover), so the logical INVITE *is*
//!     the wire INVITE.
//!
//! Because routing is now a layout property, ONE shape runs over every infra —
//! including the register proxy — which is the point of "match the same shape as
//! the real end to end".

use std::net::SocketAddr;

use scenario_harness::Invite;
use serde::Serialize;

/// The `destination` object of the proprietary `X-Api-Call` test-control header
/// the b2bua decision engine reads (`b2bua::decision::test_adapter`):
/// `{"host":…,"port":…,"user":…}`. `host:port` pins the b-leg callee
/// (`route_dest_from_api_call`); the optional `user` sets ONLY the b-leg R-URI
/// userpart so a downstream registrar front proxy can resolve the AOR
/// (`route_user_from_api_call`) — left `None` for the pod-direct real-cluster
/// callee here, modeled so the field is available when that topology is added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiCallDestination {
    pub host: String,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl ApiCallDestination {
    /// The `{host, port}` pin for a resolved callee (no userpart).
    fn of(addr: SocketAddr) -> Self {
        ApiCallDestination { host: addr.ip().to_string(), port: addr.port(), user: None }
    }
}

/// One entry of an `X-Api-Call` `routes` failover plan (ADR-0017): a destination
/// pin plus the b-leg `new_ruri` that must name the callee (the anti-loop
/// invariant behind an LB — `test_adapter::route_from_obj`). The b2bua walks the
/// list, failing over to the next on a b-leg rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiCallRoute {
    pub destination: ApiCallDestination,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_ruri: Option<String>,
}

/// The proprietary `X-Api-Call` JSON payload — the platform's test control
/// channel (ADR-0017). Typed so every site that emits it builds the SAME shape
/// instead of hand-formatting JSON: the real-cluster layout's single b-leg pin
/// ([`ApiCall::pin`]), an ordered failover plan ([`ApiCall::routes`]), and a
/// shape's REFER blind-transfer authorization ([`ApiCall::refer`]). Only the
/// fields the scripted / deployed `test_adapter` reads are modeled; the reader is
/// key-order-independent (`serde_json::Value` `.get(...)`), so field order is
/// irrelevant. `on_exhausted` is omitted — the reader defaults it to `relay`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ApiCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<ApiCallDestination>,
    /// REFER blind-transfer authorization key (`default_call_refer`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refer_key: Option<String>,
    /// Ordered failover plan; when non-empty the b2bua walks it instead of
    /// `destination` (`api_call_has_routes`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<ApiCallRoute>,
}

impl ApiCall {
    /// Pin the b-leg callee to `host:port` (no userpart) — the real cluster's
    /// single-destination `{"destination":{"host":…,"port":…}}`.
    pub fn pin(host: impl Into<String>, port: u16) -> Self {
        ApiCall {
            destination: Some(ApiCallDestination { host: host.into(), port, user: None }),
            ..Default::default()
        }
    }

    /// An ordered b-leg failover plan over `candidates` (primary first): each
    /// route pins a callee's address and sets `new_ruri` to its URI so the LB
    /// forwards to the actual callee, not the VIP (the anti-loop invariant).
    pub fn routes(candidates: &[CalleeTarget]) -> Self {
        ApiCall {
            routes: candidates
                .iter()
                .map(|c| ApiCallRoute {
                    destination: ApiCallDestination::of(c.addr),
                    new_ruri: Some(c.uri.clone()),
                })
                .collect(),
            ..Default::default()
        }
    }

    /// Authorize a REFER blind transfer to `host:port` under `key` — the
    /// `{"refer_key":…,"destination":{…}}` the transfer shapes send on the REFER.
    pub fn refer(key: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        ApiCall {
            destination: Some(ApiCallDestination { host: host.into(), port, user: None }),
            refer_key: Some(key.into()),
            ..Default::default()
        }
    }

    /// Serialize to the `X-Api-Call` header value (a compact JSON object).
    pub fn to_header(&self) -> String {
        serde_json::to_string(self).expect("ApiCall serializes")
    }
}

/// How THIS layout addresses a logical callee **role** on its wire — the generic
/// resolution every shape uses for ANY callee (the a-leg INVITE target, a reroute
/// candidate, a REFER target). Derived from the layout's [`EgressPolicy`] + the
/// Endpoint config (via [`InfraRuntime::callee`]), so a shape never hard-codes a
/// callee's address or AOR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalleeTarget {
    /// The logical role this resolves (e.g. `"bob2"`).
    pub role: String,
    /// The request / Refer-To URI that reaches the callee on this topology: the
    /// registered AOR (`sip:<role>@<domain>`) on the register proxy, else
    /// `sip:<role>@<host:port>`.
    pub uri: String,
    /// The callee's wire address (the pod/agent socket) — the `X-Api-Call`
    /// destination pin and the host:port a routes-plan entry carries.
    pub addr: SocketAddr,
}

/// How a layout realizes a logical INVITE on its wire — the per-Infra-shape
/// egress policy. Resolves callee roles to URIs ([`EgressPolicy::callee_uri`])
/// and an ordered candidate list to an [`EgressRewrite`]
/// ([`EgressPolicy::rewrite_for`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressPolicy {
    /// No rewrite: the SUT's own routing (the fake LB's scripted decision engine,
    /// or a direct peer call) reaches the callee — and owns any failover — from
    /// the logical INVITE as-is.
    Transparent,
    /// Pin the b-leg callee via the proprietary `X-Api-Call` header — the real
    /// cluster, whose deployed worker otherwise falls back to its own in-cluster
    /// `B2BUA_DEST`. One candidate → a `destination` pin; several → a `routes`
    /// failover plan walked on b-leg rejection.
    ApiCallPin,
    /// Dial the (primary) callee's registered AOR: rewrite the Request-URI to
    /// `sip:<callee>@<domain>` so the register front proxy resolves the binding.
    /// A pure proxy has no failover, so only the primary candidate is used.
    RegistrarAor { domain: String },
}

impl EgressPolicy {
    /// The request / Refer-To URI that addresses `role` (resolved to `addr`) on
    /// this topology — the registered AOR or `sip:<role>@<host:port>`.
    pub fn callee_uri(&self, role: &str, addr: SocketAddr) -> String {
        match self {
            EgressPolicy::RegistrarAor { domain } => format!("sip:{role}@{domain}"),
            _ => format!("sip:{role}@{}:{}", addr.ip(), addr.port()),
        }
    }

    /// Realize the a-leg egress rewrite for an ordered candidate list (the
    /// primary first, failover targets after). One callee on a pinned layout is a
    /// single `destination`; several become a `routes` failover plan. The register
    /// layout dials the primary's AOR; a transparent layout rewrites nothing.
    pub fn rewrite_for(&self, candidates: &[CalleeTarget]) -> EgressRewrite {
        match self {
            EgressPolicy::Transparent => EgressRewrite::default(),
            EgressPolicy::RegistrarAor { .. } => EgressRewrite {
                ruri: candidates.first().map(|c| c.uri.clone()),
                headers: vec![],
            },
            EgressPolicy::ApiCallPin => {
                let api = match candidates {
                    [] => return EgressRewrite::default(),
                    [one] => ApiCall::pin(one.addr.ip().to_string(), one.addr.port()),
                    many => ApiCall::routes(many),
                };
                EgressRewrite {
                    ruri: None,
                    headers: vec![("X-Api-Call".to_string(), api.to_header())],
                }
            }
        }
    }
}

/// The concrete rewrite of a logical INVITE into the wire INVITE: an optional
/// Request-URI override and a set of extra headers (e.g. the proprietary
/// `X-Api-Call` pin/plan, or a forced `Route`). Produced by
/// [`EgressPolicy::rewrite_for`]; applied by [`EgressRewrite::apply`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressRewrite {
    /// Replace the Request-URI (e.g. the registrar layout dialing the AOR). `None`
    /// keeps the logical R-URI (the default or the Test case's `core.ruri`).
    pub ruri: Option<String>,
    /// Extra headers `(name, value)` to attach: the `X-Api-Call` pin/plan on the
    /// real cluster, a forced `Route` for a future strict/loose-route layout, etc.
    pub headers: Vec<(String, String)>,
}

impl EgressRewrite {
    /// Apply this rewrite onto an INVITE builder — the R-URI override first (so it
    /// supersedes any topology-agnostic authored R-URI), then the extra headers.
    pub fn apply<'a>(self, mut invite: Invite<'a>) -> Invite<'a> {
        if let Some(ruri) = self.ruri {
            invite = invite.ruri(ruri);
        }
        for (name, value) in self.headers {
            invite = invite.with_header(&name, &value);
        }
        invite
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(role: &str, addr: &str) -> CalleeTarget {
        let addr: SocketAddr = addr.parse().unwrap();
        CalleeTarget { role: role.to_string(), uri: format!("sip:{role}@{addr}"), addr }
    }

    #[test]
    fn pin_serializes_host_port_only() {
        // Matches the hand-formatted payload the shapes used to emit verbatim,
        // and what `route_dest_from_api_call` reads (host string, port number).
        assert_eq!(
            ApiCall::pin("10.0.0.9", 5070).to_header(),
            r#"{"destination":{"host":"10.0.0.9","port":5070}}"#
        );
    }

    #[test]
    fn refer_carries_key_and_destination() {
        let v: serde_json::Value =
            serde_json::from_str(&ApiCall::refer("refer-allow-c", "127.0.0.1", 5071).to_header())
                .unwrap();
        assert_eq!(v["refer_key"], "refer-allow-c");
        assert_eq!(v["destination"]["host"], "127.0.0.1");
        assert_eq!(v["destination"]["port"], 5071);
    }

    #[test]
    fn user_is_omitted_unless_set() {
        let with_user =
            ApiCallDestination { host: "core".into(), port: 5060, user: Some("bob".into()) };
        let h = ApiCall { destination: Some(with_user), ..Default::default() }.to_header();
        assert!(h.contains(r#""user":"bob""#), "{h}");
        assert!(!ApiCall::pin("h", 1).to_header().contains("user"));
    }

    #[test]
    fn one_pinned_candidate_is_a_single_destination() {
        // The single-callee case stays byte-identical to the old hand-formatted
        // pin — no `routes`, so the deployed worker's single-dest path is unchanged.
        let rw = EgressPolicy::ApiCallPin.rewrite_for(&[target("bob1", "127.0.0.1:5070")]);
        assert_eq!(rw.ruri, None);
        assert_eq!(rw.headers, vec![(
            "X-Api-Call".to_string(),
            r#"{"destination":{"host":"127.0.0.1","port":5070}}"#.to_string(),
        )]);
    }

    #[test]
    fn several_pinned_candidates_become_a_failover_routes_plan() {
        let rw = EgressPolicy::ApiCallPin
            .rewrite_for(&[target("bob1", "127.0.0.1:5070"), target("bob2", "127.0.0.1:5071")]);
        assert_eq!(rw.ruri, None);
        let v: serde_json::Value = serde_json::from_str(&rw.headers[0].1).unwrap();
        let routes = v["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0]["destination"]["port"], 5070);
        assert_eq!(routes[0]["new_ruri"], "sip:bob1@127.0.0.1:5070");
        assert_eq!(routes[1]["destination"]["port"], 5071);
        assert_eq!(routes[1]["new_ruri"], "sip:bob2@127.0.0.1:5071");
    }

    #[test]
    fn registrar_dials_the_primary_aor_and_ignores_failover() {
        let policy = EgressPolicy::RegistrarAor { domain: "register.example".into() };
        assert_eq!(policy.callee_uri("bob2", "127.0.0.1:5071".parse().unwrap()), "sip:bob2@register.example");
        let rw = policy.rewrite_for(&[
            CalleeTarget { role: "bob1".into(), uri: "sip:bob1@register.example".into(), addr: "127.0.0.1:5070".parse().unwrap() },
            CalleeTarget { role: "bob2".into(), uri: "sip:bob2@register.example".into(), addr: "127.0.0.1:5071".parse().unwrap() },
        ]);
        assert_eq!(rw.ruri.as_deref(), Some("sip:bob1@register.example"));
        assert!(rw.headers.is_empty(), "no proprietary header on the register proxy");
    }

    #[test]
    fn transparent_rewrites_nothing() {
        let rw = EgressPolicy::Transparent.rewrite_for(&[target("bob1", "127.0.0.1:5070")]);
        assert_eq!(rw, EgressRewrite::default());
    }
}
