//! Per-peer failure attribution (observability only;
//! `b2bua_peer_failures_total{peer,scope,kind}`): resolve which hop a failure
//! belongs to and classify it internal/external against the configured
//! outbound proxy. Runs on the router task, so it never waits on a name
//! lookup: a hostname is read from the outbound proxy's kept addresses
//! ([`RouterCtx::outbound_proxy`]), whose lookup runs off the task.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use http_net::ResolvedTarget;

use call::{Call, TimerType};

use super::RouterCtx;
use crate::config::B2buaConfig;
use b2bua_sdk::event::CallEvent;

/// Per-peer keepalive-timeout attribution (observability only;
/// `b2bua_peer_failures_total{...,kind="keepalive_timeout"}`). The genuine
/// no-200 keepalive timeout is the `KeepaliveTimeout` timer firing for a
/// specific leg L (`leg_id`): the `keepalive-timeout` rule ends the call and
/// BYEs BOTH legs. So we MUST NOT attribute to an outbound BYE's destination
/// (the first is the a-leg's hop, often the healthy one, and mis-classifies
/// internal/external). Attribute to the FAILED leg L's OWN egress-aware next
/// hop — the exact hop the unanswered OPTIONS went to. If L or its dialog
/// can't be resolved we record nothing (no fabricated address). Distinct from
/// the reclaim-time stale drop (`restore_hygiene`), which never reaches this
/// event path.
pub(super) fn record_keepalive_timeout_peer(ctx: &RouterCtx, event: &CallEvent, call: &Call) {
    if let CallEvent::Timer { key, .. } = event {
        if key.timer_type != TimerType::KeepaliveTimeout {
            return;
        }
        let hop = keepalive_timeout_peer(&ctx.config, call, key.leg_id());
        let Some(dest) =
            hop.and_then(|(host, port)| hop_address(ctx.outbound_proxy.as_deref(), &host, port))
        else {
            return;
        };
        ctx.metrics.record_peer_failure(
            &dest,
            classify_b2bua_peer(ctx.outbound_proxy.as_deref(), &dest),
            crate::peer_failures::PeerFailureKind::KeepaliveTimeout,
        );
        // Aggregated per hop: a dead peer is one episode, however many calls
        // it takes with it (ADR-0026).
        ctx.keepalive_waves.record(&dest.to_string(), "timeouts", 1);
    }
}

/// The address of the hop `host:port`, without a lookup: an IP literal is
/// its own; the outbound proxy's name reads its kept address (a lookup is
/// started off the task when none is kept); any other name gives `None`, and
/// nothing is recorded.
fn hop_address(proxy: Option<&ResolvedTarget>, host: &str, port: u16) -> Option<SocketAddr> {
    let literal = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    let proxy = proxy.filter(|p| p.host_port() == (host.to_string(), port))?;
    let kept = proxy.address();
    if kept.is_none() {
        proxy.look_up();
    }
    kept
}

/// The configured outbound proxy as a target whose name is looked up off the
/// router task; an IP literal is never looked up.
pub(crate) fn outbound_proxy_target(config: &B2buaConfig) -> Option<Arc<ResolvedTarget>> {
    let (host, port) = config.b2b_outbound_proxy.as_ref()?;
    let target = match host.parse::<IpAddr>() {
        Ok(ip) => ResolvedTarget::addr(SocketAddr::new(ip, *port)),
        Err(_) => {
            ResolvedTarget::name_with(format!("{host}:{port}"), Arc::new(http_net::SystemResolver))
        }
    };
    Some(Arc::new(target))
}

/// Resolve the egress-aware next hop of the leg a `KeepaliveTimeout` fired for —
/// the hop the unanswered OPTIONS used — for the per-peer keepalive-timeout metric.
/// Mirrors `bye_on_dialog`'s destination decision (to_gen_dialog → dest_of →
/// `relay::leg_egress_dest`) WITHOUT mutating a request. Returns `None` (record
/// nothing) when the leg or its confirmed dialog can't be resolved — never a fabricated
/// address. The returned `(host, port)` may be a hostname (the outbound proxy); the
/// caller reads its address ([`hop_address`]) before recording.
fn keepalive_timeout_peer(
    config: &B2buaConfig,
    call: &Call,
    leg_id: Option<&str>,
) -> Option<(String, u16)> {
    use crate::rules::relay;
    let leg_id = leg_id?;
    let leg = if leg_id == call.a_leg.leg_id {
        &call.a_leg
    } else {
        call.b_legs.iter().find(|l| l.leg_id == leg_id)?
    };
    let d = leg.dialogs.confirmed()?;
    let gd = relay::to_gen_dialog(leg, d);
    let base = relay::target_dest(&gd.remote_target);
    Some(relay::leg_egress_dest(config, leg_id, &gd.route_set, base))
}

/// Classify a destination as an internal cluster peer or an external one for the
/// per-peer failure metric. A b2bua's only config-resolvable cluster peer is the
/// configured outbound proxy (`b2b_outbound_proxy`): every b-leg egresses through
/// it, so a worker→callee timeout we count against the outbound proxy is the
/// in-cluster hop. Replication-peer addresses are NOT in `B2buaConfig` as
/// `SocketAddr`s (the repl layer addresses peers by endpoint URL, resolved
/// elsewhere), so they fall through to `External` here — a documented limitation;
/// the metric is still bounded and correct, just coarser for repl-peer timeouts
/// (which are rare and would count against the external cap).
///
/// The outbound proxy may be configured as a HOSTNAME: `dest` is compared with
/// every address its name resolved to, as kept now. None kept yet (its first
/// lookup still running, or failed) classifies `External` and starts a lookup
/// off the task, so the metric stays bounded and the router never waits.
pub(super) fn classify_b2bua_peer(
    proxy: Option<&ResolvedTarget>,
    dest: &SocketAddr,
) -> crate::peer_failures::PeerScope {
    let Some(proxy) = proxy else {
        return crate::peer_failures::PeerScope::External;
    };
    match proxy.addresses() {
        Some(addrs) if addrs.contains(dest) => crate::peer_failures::PeerScope::Internal,
        Some(_) => crate::peer_failures::PeerScope::External,
        None => {
            proxy.look_up();
            crate::peer_failures::PeerScope::External
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::initial_invite::build_initial_call;

    #[test]
    fn classify_b2bua_peer_internal_iff_outbound_proxy() {
        use crate::peer_failures::PeerScope;
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("10.0.0.9".to_string(), 5060));
        let proxy: SocketAddr = "10.0.0.9:5060".parse().unwrap();
        let other: SocketAddr = "203.0.113.7:5060".parse().unwrap();
        let target = outbound_proxy_target(&config);
        assert_eq!(classify_b2bua_peer(target.as_deref(), &proxy), PeerScope::Internal);
        assert_eq!(classify_b2bua_peer(target.as_deref(), &other), PeerScope::External);
        // With no outbound proxy configured, every peer is external.
        config.b2b_outbound_proxy = None;
        let target = outbound_proxy_target(&config);
        assert_eq!(classify_b2bua_peer(target.as_deref(), &proxy), PeerScope::External);
    }

    /// A resolver that never answers.
    struct Hanging;

    #[async_trait::async_trait]
    impl http_net::NameResolver for Hanging {
        async fn resolve(&self, _: &str) -> Vec<SocketAddr> {
            std::future::pending().await
        }
    }

    /// A hostname-configured outbound proxy is read from its kept addresses,
    /// never looked up on the router task: with a lookup that never returns
    /// the classification answers at once (`External`), and once the name
    /// resolved, every address it found pins `Internal`.
    #[tokio::test(start_paused = true)]
    async fn a_hostname_outbound_proxy_never_blocks_the_router_task() {
        use crate::peer_failures::PeerScope;
        let loopback: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let hanging = ResolvedTarget::name_with("proxy.svc:5060", Arc::new(Hanging));
        let start = tokio::time::Instant::now();
        assert_eq!(classify_b2bua_peer(Some(&hanging), &loopback), PeerScope::External);
        assert_eq!(hop_address(Some(&hanging), "proxy.svc", 5060), None, "nothing kept yet");
        assert_eq!(start.elapsed(), std::time::Duration::ZERO, "answered at once");

        let resolver = Arc::new(http_net::MemoryResolver::new());
        let second: SocketAddr = "127.0.0.2:5060".parse().unwrap();
        resolver.point("proxy.svc:5060", [loopback, second]);
        let named = ResolvedTarget::name_with("proxy.svc:5060", resolver.clone());
        assert_eq!(classify_b2bua_peer(Some(&named), &second), PeerScope::External, "not yet");
        sip_clock::testkit::settle().await;
        assert_eq!(resolver.lookups(), 1, "looked up off the task");
        assert_eq!(classify_b2bua_peer(Some(&named), &second), PeerScope::Internal);
        assert_eq!(hop_address(Some(&named), "proxy.svc", 5060), Some(loopback));
        assert_eq!(hop_address(Some(&named), "other.svc", 5060), None, "no other name");
        assert_eq!(hop_address(None, "[::1]", 5060), Some("[::1]:5060".parse().unwrap()));
    }

    // A B-leg keepalive timeout must attribute to the FAILED b-leg's egress hop
    // (its OPTIONS went through the outbound proxy → internal), NOT to the
    // surviving a-leg's hop (the BYE destination this turn produced). Drives
    // `keepalive_timeout_peer` directly — the pure helper the router uses.
    #[test]
    fn keepalive_timeout_attributes_to_the_failed_b_leg_egress_hop() {
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("10.0.0.9".to_string(), 5060));

        // a-leg dialog: remote target is the external UAC (the surviving leg's hop).
        let a_dialog = test_dialog("203.0.113.7", 5060, &[]);
        // b-leg dialog: empty route set + b-leg → egress bootstraps through the
        // outbound proxy (the wire dest the unanswered OPTIONS used).
        let b_dialog = test_dialog("10.244.2.7", 5060, &[]);

        let call = test_call(vec![a_dialog], vec![("b-1", vec![b_dialog])]);

        // The KeepaliveTimeout fired for the b-leg.
        let hop =
            keepalive_timeout_peer(&config, &call, Some("b-1")).expect("b-leg dialog resolves");
        assert_eq!(
            hop,
            ("10.0.0.9".to_string(), 5060),
            "attribute to the FAILED b-leg's egress hop (the outbound proxy), \
             not the surviving a-leg's external hop",
        );
        assert_eq!(
            classify_b2bua_peer(
                outbound_proxy_target(&config).as_deref(),
                &"10.0.0.9:5060".parse().unwrap()
            ),
            crate::peer_failures::PeerScope::Internal,
            "the failed b-leg's egress hop classifies internal",
        );

        // Sanity: the a-leg's own hop is the external UAC (what mis-attributing
        // the b-leg failure to the surviving leg would have recorded).
        assert_eq!(
            keepalive_timeout_peer(&config, &call, Some("a")).unwrap(),
            ("203.0.113.7".to_string(), 5060),
        );

        // Unresolvable leg / no leg_id → record nothing (no fabricated address).
        assert!(keepalive_timeout_peer(&config, &call, Some("b-99")).is_none());
        assert!(keepalive_timeout_peer(&config, &call, None).is_none());
    }

    fn test_dialog(remote_host: &str, remote_port: u16, route_set: &[&str]) -> call::Dialog {
        let mut d = call::Dialog::new("ltag", "rtag", call::DialogState::Confirmed, 1);
        d.remote_target = format!("sip:peer@{remote_host}:{remote_port}");
        d.route_set = route_set.iter().map(|s| s.to_string()).collect();
        d
    }

    fn test_leg(leg_id: &str, dialogs: Vec<call::Dialog>) -> call::Leg {
        call::Leg {
            leg_id: leg_id.into(),
            call_id: "cid@x".into(),
            source: call::RemoteInfo { address: "0.0.0.0".into(), port: 0 },
            dialogs: dialogs.into(),
            local_uri: None,
            remote_uri: None,
            invite_request_uri: None,
            outbound_invite: None,
            own_tag: None,
            ext: None,
            kind: None,
            adopted: None,
            invite_final_sent: None,
            answer_preempted: None,
            attempt: None,
            ring: call::Ring::Silent,
            in_session_timer: None,
            messages: Default::default(),
            sdp_session: Default::default(),
        }
    }

    fn test_call(a_dialogs: Vec<call::Dialog>, b_legs: Vec<(&str, Vec<call::Dialog>)>) -> Call {
        let mut call = build_initial_call(
            &crate::rules::relay::rebuild_a_leg_invite(&minimal_invite_snapshot()),
            "203.0.113.7:5060".parse().unwrap(),
            &B2buaConfig::default(),
            &sip_txn::IdGen::seeded(1),
            0,
        );
        call.a_leg.dialogs = a_dialogs.into();
        call.b_legs = b_legs.into_iter().map(|(id, ds)| test_leg(id, ds)).collect();
        call
    }

    fn minimal_invite_snapshot() -> call::ALegInviteSnapshot {
        call::ALegInviteSnapshot {
            uri: "sip:bob@10.244.2.7:5060".into(),
            headers: vec![
                call::SipHeader {
                    name: "Via".into(),
                    value: "SIP/2.0/UDP 203.0.113.7:5060;branch=z9hG4bKa".into(),
                },
                call::SipHeader {
                    name: "From".into(),
                    value: "<sip:alice@203.0.113.7:5060>;tag=alice".into(),
                },
                call::SipHeader { name: "To".into(), value: "<sip:bob@10.244.2.7:5060>".into() },
                call::SipHeader { name: "Call-ID".into(), value: "cid@x".into() },
                call::SipHeader { name: "CSeq".into(), value: "1 INVITE".into() },
                call::SipHeader { name: "Content-Length".into(), value: "0".into() },
            ],
            body: vec![],
            cseq: 1,
            from_tag: "alice".into(),
        }
    }
}
