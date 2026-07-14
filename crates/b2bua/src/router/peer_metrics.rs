//! Per-peer failure attribution (observability only;
//! `b2bua_peer_failures_total{peer,scope,kind}`): resolve which hop a failure
//! belongs to and classify it internal/external against the configured
//! outbound proxy.

use std::net::SocketAddr;

use call::Call;

use crate::config::B2buaConfig;

/// Resolve the egress-aware next hop of the leg a `KeepaliveTimeout` fired for —
/// the hop the unanswered OPTIONS used — for the per-peer keepalive-timeout metric.
/// Mirrors `bye_on_dialog`'s destination decision (to_gen_dialog → dest_of →
/// `relay::leg_egress_dest`) WITHOUT mutating a request. Returns `None` (record
/// nothing) when the leg or its first dialog can't be resolved — never a fabricated
/// address. The returned `(host, port)` may be a hostname (the outbound proxy); the
/// caller resolves it to a `SocketAddr` before recording.
pub(super) fn keepalive_timeout_peer(
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
    let d = leg.dialogs.first()?;
    let gd = relay::to_gen_dialog(&d.sip);
    let base = relay::dest_of(&relay::strip_uri(&gd.remote_target));
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
/// (which are rare and would land in the external LRU/overflow).
///
/// The outbound proxy may be configured as a HOSTNAME (not an IP literal). We
/// resolve it via `ToSocketAddrs` (taking the first resolved addr) and compare by
/// resolved IP+port, so the internal pinning fires for a hostname-configured proxy
/// — not just the cluster's VIP IP literal. This path is cold (keepalive/response
/// timeout), so the lazy resolve here is acceptable. If resolution fails, the dest
/// classifies External (fail-open; the metric stays bounded).
pub(super) fn classify_b2bua_peer(
    config: &B2buaConfig,
    dest: &SocketAddr,
) -> crate::peer_failures::PeerScope {
    use std::net::ToSocketAddrs;
    if let Some((host, port)) = &config.b2b_outbound_proxy {
        if let Ok(mut resolved) = (host.as_str(), *port).to_socket_addrs() {
            if resolved.any(|proxy| proxy == *dest) {
                return crate::peer_failures::PeerScope::Internal;
            }
        }
    }
    crate::peer_failures::PeerScope::External
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::initial_invite::build_initial_call;
    use call::LegState;

    #[test]
    fn classify_b2bua_peer_internal_iff_outbound_proxy() {
        use crate::peer_failures::PeerScope;
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("10.0.0.9".to_string(), 5060));
        let proxy: SocketAddr = "10.0.0.9:5060".parse().unwrap();
        let other: SocketAddr = "203.0.113.7:5060".parse().unwrap();
        assert_eq!(classify_b2bua_peer(&config, &proxy), PeerScope::Internal);
        assert_eq!(classify_b2bua_peer(&config, &other), PeerScope::External);
        // With no outbound proxy configured, every peer is external.
        config.b2b_outbound_proxy = None;
        assert_eq!(classify_b2bua_peer(&config, &proxy), PeerScope::External);
    }

    // A hostname-configured outbound proxy resolving to the candidate's IP+port
    // must classify Internal (a `parse::<SocketAddr>()` on a hostname always
    // fails → everything would fall to External and internal pinning would never
    // fire off-cluster). `localhost` is a stable, network-free resolution.
    #[test]
    fn classify_b2bua_peer_resolves_a_hostname_outbound_proxy() {
        use crate::peer_failures::PeerScope;
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("localhost".to_string(), 5060));
        // localhost resolves to 127.0.0.1 (and/or ::1); the loopback v4 candidate
        // at the configured port must be classified Internal.
        let loopback: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let other: SocketAddr = "127.0.0.1:5999".parse().unwrap();
        assert_eq!(
            classify_b2bua_peer(&config, &loopback),
            PeerScope::Internal,
            "a hostname-configured outbound proxy must resolve and pin internal",
        );
        assert_eq!(
            classify_b2bua_peer(&config, &other),
            PeerScope::External,
            "a different port is not the proxy",
        );
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
        let hop = keepalive_timeout_peer(&config, &call, Some("b-1"))
            .expect("b-leg dialog resolves");
        assert_eq!(
            hop,
            ("10.0.0.9".to_string(), 5060),
            "attribute to the FAILED b-leg's egress hop (the outbound proxy), \
             not the surviving a-leg's external hop",
        );
        assert_eq!(
            classify_b2bua_peer(&config, &"10.0.0.9:5060".parse().unwrap()),
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
        call::Dialog {
            sip: call::StackDialog {
                call_id: "cid@x".into(),
                local_tag: "ltag".into(),
                remote_tag: "rtag".into(),
                local_uri: "sip:svc@b2bua".into(),
                remote_uri: format!("sip:peer@{remote_host}"),
                remote_target: format!("sip:peer@{remote_host}:{remote_port}"),
                local_cseq: 1,
                route_set: route_set.iter().map(|s| s.to_string()).collect(),
            },
            ext: call::B2buaDialogExt {
                remote_cseq: None,
                inbound_pending_requests: vec![],
                ack_branch: None,
                pending_invite_txn: None,
                cached_sdp: None,
                pending_reinvite_2xx: None,
            },
        }
    }

    fn test_leg(leg_id: &str, dialogs: Vec<call::Dialog>) -> call::Leg {
        call::Leg {
            leg_id: leg_id.into(),
            call_id: "cid@x".into(),
            from_tag: "ftag".into(),
            source: call::RemoteInfo { address: "0.0.0.0".into(), port: 0 },
            state: LegState::Confirmed,
            disposition: call::LegDisposition::Bridged,
            dialogs,
            no_answer_timeout_sec: None,
            bye_disposition: None,
            local_uri: None,
            remote_uri: None,
            invite_request_uri: None,
            pending_invite_txn: None,
            ext: None,
            kind: None,
            adopted: None,
        }
    }

    fn test_call(
        a_dialogs: Vec<call::Dialog>,
        b_legs: Vec<(&str, Vec<call::Dialog>)>,
    ) -> Call {
        let mut call = build_initial_call(
            &crate::rules::relay::rebuild_a_leg_invite(&minimal_invite_snapshot()),
            "203.0.113.7:5060".parse().unwrap(),
            &B2buaConfig::default(),
            0,
        );
        call.a_leg.dialogs = a_dialogs;
        call.b_legs = b_legs.into_iter().map(|(id, ds)| test_leg(id, ds)).collect();
        call
    }

    fn minimal_invite_snapshot() -> call::ALegInviteSnapshot {
        call::ALegInviteSnapshot {
            uri: "sip:bob@10.244.2.7:5060".into(),
            headers: vec![
                call::SipHeader { name: "Via".into(), value: "SIP/2.0/UDP 203.0.113.7:5060;branch=z9hG4bKa".into() },
                call::SipHeader { name: "From".into(), value: "<sip:alice@203.0.113.7:5060>;tag=alice".into() },
                call::SipHeader { name: "To".into(), value: "<sip:bob@10.244.2.7:5060>".into() },
                call::SipHeader { name: "Call-ID".into(), value: "cid@x".into() },
                call::SipHeader { name: "CSeq".into(), value: "1 INVITE".into() },
                call::SipHeader { name: "Content-Length".into(), value: "0".into() },
            ],
            body: vec![],
        }
    }
}
