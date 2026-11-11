//! One audit pass: every row reads the same projections of the recording,
//! each built once whatever the row count.

use std::sync::Arc;
use std::time::Duration;

use layer_harness::{Recorder, RunContext};
use sip_net::BindUdpOpts;

use super::*;
use crate::contracts::{with_all_contracts, ScopedAuditOptions};
use crate::rfc_audit::{builds, evaluate_rfc_audit, rfc_cross_message_rules};
use crate::simulated::SimulatedSignalingNetwork;

/// A hop declared `{Proxy}` that forwards the B2BUA's b-leg INVITE.
const HOP: &str = "127.0.0.1:5090";

/// A call recorded at a B2BUA and at a forwarding hop: the caller's first
/// INVITE copy lost to the loss model and retransmitted, two finals on the
/// a-leg's INVITE transaction, a reused in-dialog CSeq, and the b-leg the
/// B2BUA originates passed on by the hop.
fn recorded_call() -> Vec<Stamped<SignalingNetworkEvent>> {
    use crate::types::RecvDisposition::{Delivered, LossModel};
    let invite = req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER));
    vec![
        bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0),
        bind_roles(HOP, HashSet::from([UaRole::Proxy]), 1),
        arrival(SUT, invite.clone(), ALICE, 2, LossModel, None),
        arrival(SUT, invite, ALICE, 3, Delivered, Some(2)),
        sent(SUT, b_leg_invite("z9hG4bK-b1"), HOP, 4),
        recv(HOP, b_leg_invite("z9hG4bK-b1"), SUT, 5),
        sent(HOP, b_leg_invite("z9hG4bK-b2"), BOB, 6),
        sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-i1"), ALICE, 7),
        recv(SUT, req("ACK", "z9hG4bK-i1", 1, Some("bt")), ALICE, 8),
        sent(SUT, resp(603, 1, "INVITE", "bt", "z9hG4bK-i1"), ALICE, 9),
        recv(SUT, req("OPTIONS", "z9hG4bK-o", 2, Some("bt")), ALICE, 10),
        recv(SUT, req("BYE", "z9hG4bK-b", 2, Some("bt")), ALICE, 11),
    ]
}

/// Every projection a pass offers, built once.
fn each_once() -> builds::Built {
    ["dropped", "party_views", "positions", "proxy_only_lanes", "recorded", "relay_lanes", "views"]
        .into_iter()
        .map(|projection| (projection, 1))
        .collect()
}

/// The whole suite reads one set of projections: the bind views, their
/// observations, the wire positions, the recorded parties, the modeled drops
/// and the skipped lanes are each built once per pass, not once per row.
#[test]
fn an_audit_pass_builds_each_projection_once_whatever_the_row_count() {
    let evs = recorded_call();
    let rows = rfc_cross_message_rules().len();
    assert!(rows > 1, "the suite runs {rows} rows");
    builds::take();
    let audit = evaluate_rfc_audit(&evs);
    let built = builds::take();
    assert!(!audit.findings.is_empty(), "the trace exercises the suite");
    assert_eq!(built, each_once(), "builds over {rows} rows");
}

/// The layer-close audit runs its rule list over one pass as well.
#[tokio::test]
async fn the_layer_close_audit_builds_each_projection_once() {
    let opts =
        ScopedAuditOptions { cross_message_rules: rfc_cross_message_rules(), ..Default::default() };
    let wrapped = with_all_contracts(
        Arc::new(SimulatedSignalingNetwork::new(0)),
        Recorder::fake(),
        RunContext::RealRun,
        opts,
        false,
    );
    let net = wrapped.network.clone();
    {
        let a = net.bind_udp(BindUdpOpts::new(ALICE.parse().unwrap(), 64)).await.unwrap();
        let b = net.bind_udp(BindUdpOpts::new(SUT.parse().unwrap(), 64)).await.unwrap();
        a.send_to(&req("INVITE", "z9hG4bK-i1", 1, None), b.local_addr()).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(1), b.recv()).await.unwrap();
    }
    builds::take();
    wrapped.recording.close().await.unwrap();
    assert_eq!(builds::take(), each_once());
}

/// A hop that forwards the caller's dialog-establishing INVITE (a relay lane)
/// to a B2BUA whose caller is a recorded bind of its own.
fn relayed_call() -> Vec<Stamped<SignalingNetworkEvent>> {
    let invite = || req("INVITE", "z9hG4bK-r1", 1, None);
    vec![
        bind_roles(ALICE, HashSet::from([UaRole::Uac]), 0),
        sent(ALICE, invite(), HOP, 1),
        recv(HOP, invite(), ALICE, 2),
        sent(HOP, invite(), SUT, 3),
        recv(SUT, invite(), HOP, 4),
        sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-r1"), HOP, 5),
        sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-r1"), HOP, 6),
        recv(HOP, resp(200, 1, "INVITE", "bt", "z9hG4bK-r1"), SUT, 7),
        sent(HOP, resp(200, 1, "INVITE", "bt", "z9hG4bK-r1"), ALICE, 8),
        recv(ALICE, resp(200, 1, "INVITE", "bt", "z9hG4bK-r1"), HOP, 9),
    ]
}

/// A rule's findings and the occasions it noted, over a pass shared by every
/// row of every list run before it, are the ones it finds over a pass of its
/// own: sharing changes no finding, order, lane, position, charged party or
/// occasion, unscoped or scoped.
#[test]
fn a_shared_pass_finds_what_a_pass_per_rule_finds() {
    let lists: Vec<Vec<Arc<dyn CrossMessageAuditRule>>> = vec![
        rfc_cross_message_rules().to_vec(),
        crate::rfc_audit::rfc_cross_message_rules_scoped(HashSet::from([ALICE.to_string()])),
        crate::rfc_audit::rfc_cross_message_rules_scoped(HashSet::from([BOB.to_string()])),
    ];
    let mut found = 0;
    let mut occasions = 0;
    for evs in [recorded_call(), relayed_call()] {
        let shared = AuditPass::new(&evs);
        for rule in lists.iter().flatten() {
            let own = crate::rfc_audit::occasions::tallied(|| rule.check_positioned(&evs));
            let in_shared = crate::rfc_audit::occasions::tallied(|| rule.check_pass(&shared));
            assert_eq!(in_shared, own, "{}", rule.name());
            found += own.0.len();
            occasions += own.1.len();
        }
    }
    assert!(found > 0 && occasions > 0, "the traces exercise the suite");
}

/// The pass's recorded parties with a consumer's scoped-out binds added hold
/// the parties a recording built with those binds scoped out holds.
#[test]
fn the_scoped_recording_of_a_pass_is_the_recording_built_scoped() {
    use crate::rfc_audit::recorded::Recorded;
    let parties = [
        ALICE,
        BOB,
        SUT,
        HOP,
        "127.0.0.1:5999",
        "127.0.0.2:5060",
        "127.0.0.3:5060",
        "lane-without-addr",
    ];
    for evs in [recorded_call(), relayed_call()] {
        let pass = AuditPass::new(&evs);
        let visible: Vec<_> = evs
            .iter()
            .filter(|s| crate::contracts::audit_visible_event(&s.event))
            .cloned()
            .collect();
        for scoped_out in [
            HashSet::new(),
            HashSet::from([ALICE.to_string()]),
            HashSet::from([BOB.to_string(), "127.0.0.2:5060".to_string()]),
            // A key that names an address with a suffix is held by that address.
            HashSet::from(["127.0.0.3:5060#x".to_string()]),
        ] {
            let built = Recorded::of(&visible, &scoped_out);
            let scoped = pass.recorded().scoped(&scoped_out);
            for party in parties {
                assert_eq!(scoped.holds(party), built.holds(party), "{party} of {scoped_out:?}");
            }
        }
    }
}

/// A finding's offending position indexes the audit's wire entries, which
/// leave the arrival the loss model dropped out: the second final on the
/// a-leg's INVITE transaction, sent after that drop, is the entry it names.
#[test]
fn a_position_after_a_dropped_arrival_indexes_the_audit_wire_entries() {
    let evs = recorded_call();
    let findings = evaluate_rfc_audit(&evs).findings;
    let single_final = rfc_rules::RuleId::SingleFinalPerServerTxn.token();
    let finding = findings.iter().find(|f| f.rule == single_final).expect("{findings:?}");
    let entries = crate::rfc_audit::audit_wire_entries(&evs);
    assert_eq!(finding.offending, Some(6), "{finding:?}");
    assert_eq!(entries[5].seq, 9, "the 603 the B2BUA sent after its 487");
}

/// A party view holds its members' streams in event order, a datagram one
/// member sent another once, as sent, under strictly rising stamps (two
/// members' events in one millisecond included); a bind
/// with no other in its party keeps its own view.
#[test]
fn a_party_view_merges_its_members_once_in_event_order() {
    const ALICE_SEND: &str = "127.0.0.1:40000";
    let mut events = vec![
        sent(ALICE_SEND, req("INVITE", "z9hG4bK-i1", 1, None), BOB, 0),
        recv(BOB, req("INVITE", "z9hG4bK-i1", 1, None), ALICE_SEND, 1),
        sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i1"), ALICE, 2),
        recv(ALICE, resp(200, 1, "INVITE", "bt", "z9hG4bK-i1"), BOB, 3),
        sent(ALICE_SEND, req("ACK", "z9hG4bK-a1", 1, Some("bt")), BOB, 4),
        recv(BOB, req("ACK", "z9hG4bK-a1", 1, Some("bt")), ALICE_SEND, 5),
        sent(ALICE_SEND, req("OPTIONS", "z9hG4bK-o", 2, Some("bt")), ALICE, 6),
        recv(ALICE, req("OPTIONS", "z9hG4bK-o", 2, Some("bt")), ALICE_SEND, 7),
    ];
    events[3].at_ms = events[0].at_ms;
    let pass = AuditPass::new(&events);
    let views = pass.party_views();
    let of = |bind: &str| {
        let (view, _) = views.iter().find(|(v, _)| v.bind == bind).expect("a view");
        let seqs: Vec<u64> = view.meta.iter().map(|(seq, _)| *seq).collect();
        let stamps: Vec<u64> = view.msgs.iter().map(|m| m.at_us).collect();
        (seqs, stamps)
    };
    let (seqs, stamps) = of(ALICE);
    assert_eq!(seqs, [0, 3, 4, 6], "the OPTIONS stands once, as sent");
    assert_eq!(of(ALICE_SEND).0, seqs, "both members read one stream");
    assert!(stamps.windows(2).all(|w| w[0] < w[1]), "strictly rising: {stamps:?}");
    assert_eq!(of(BOB).0, [1, 2, 5], "a party of one bind keeps its own view");
}
