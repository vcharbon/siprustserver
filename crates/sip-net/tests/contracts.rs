//! Contract decorators — recording capture, paranoid preconditions, and the
//! three-tier scoped audit. Port of the `SignalingNetwork.contracts.ts`
//! behaviours exercised by the source harness.

use std::sync::Arc;
use std::time::Duration;

use layer_harness::{Recorder, RunContext, Severity, Stamped};
use sip_net::{
    with_all_contracts, BindUdpOpts, PeerAuditRule, ScopedAuditOptions, SignalingNetworkEvent,
    SimulatedSignalingNetwork,
};
use tokio::time::timeout;

fn opts(addr: &str, queue_max: usize) -> BindUdpOpts {
    BindUdpOpts::new(addr.parse().unwrap(), queue_max)
}

fn sim() -> Arc<SimulatedSignalingNetwork> {
    Arc::new(SimulatedSignalingNetwork::new(0))
}

#[tokio::test]
async fn recording_captures_every_call() {
    let recorder = Recorder::fake();
    let wrapped = with_all_contracts(
        sim(),
        recorder.clone(),
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        true,
    );
    let net = wrapped.network;

    let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
    let b = net.bind_udp(opts("10.0.0.2:5060", 64)).await.unwrap();
    a.send_to(b"INVITE", b.local_addr()).await.unwrap();
    let _ = timeout(Duration::from_secs(1), b.recv()).await.unwrap();

    let events: Vec<_> = wrapped
        .recording
        .channel()
        .snapshot()
        .into_iter()
        .map(|s| s.event)
        .collect();

    let binds = events
        .iter()
        .filter(|e| matches!(e, SignalingNetworkEvent::BindAcquire { .. }))
        .count();
    assert_eq!(binds, 2);
    assert!(events
        .iter()
        .any(|e| matches!(e, SignalingNetworkEvent::SendCalled { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, SignalingNetworkEvent::SendResult { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, SignalingNetworkEvent::RecvItem { .. })));
}

#[tokio::test]
#[should_panic(expected = "PA4_send_msgBuffer")]
async fn paranoid_rejects_empty_send() {
    let recorder = Recorder::fake();
    let wrapped = with_all_contracts(
        sim(),
        recorder,
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        true,
    );
    let a = wrapped
        .network
        .bind_udp(opts("10.0.0.1:5060", 64))
        .await
        .unwrap();
    // Empty buffer is a programmer error → defect (panic).
    let _ = a.send_to(b"", "10.0.0.2:5060".parse().unwrap()).await;
}

#[tokio::test]
#[should_panic(expected = "PA2_bindOpts_queueMax")]
async fn paranoid_rejects_zero_queue_max() {
    let recorder = Recorder::fake();
    let wrapped = with_all_contracts(
        sim(),
        recorder,
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        true,
    );
    let _ = wrapped.network.bind_udp(opts("10.0.0.1:5060", 0)).await;
}

#[tokio::test]
async fn queue_leak_is_advisory_and_does_not_fail_close() {
    let recorder = Recorder::fake();
    let wrapped = with_all_contracts(
        sim(),
        recorder.clone(),
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        false,
    );
    let net = wrapped.network.clone();

    {
        let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
        let b = net.bind_udp(opts("10.0.0.2:5060", 64)).await.unwrap();
        // Deliver a packet to b but never consume it.
        a.send_to(b"unread", b.local_addr()).await.unwrap();
        net.await_in_flight(Duration::from_secs(1)).await;
        // b drops here with depth 1 → advisory queueLeak.
    }

    // close() runs the layer-close audit. The per-bind queue leak is advisory,
    // so close() succeeds; but the (now-unbound) endpoint also leaves no
    // residual entry in queue_depths, so there's nothing deferred-fail.
    let res = wrapped.recording.close().await;
    assert!(res.is_ok(), "advisory queueLeak must not fail close: {res:?}");

    let leaks = recorder
        .anomalies()
        .into_iter()
        .filter(|a| a.kind == "queueLeak" && a.severity == Severity::Advisory)
        .count();
    assert_eq!(leaks, 1);
}

#[tokio::test]
async fn undeliverable_is_deferred_fail() {
    let recorder = Recorder::fake();
    let wrapped = with_all_contracts(
        sim(),
        recorder,
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        false,
    );
    let net = wrapped.network.clone();

    {
        let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
        a.send_to(b"nowhere", "10.9.9.9:5060".parse().unwrap())
            .await
            .unwrap();
        net.await_in_flight(Duration::from_secs(1)).await;
    }

    let res = wrapped.recording.close().await;
    let err = res.expect_err("undeliverable packet must fail close");
    assert_eq!(err.check, "A2_undeliverable");
}

// A per-peer rule that flags any observed send. Used to exercise the
// deferred-fail tier and the real-run silencing.
struct NoSendRule;
impl PeerAuditRule for NoSendRule {
    fn name(&self) -> &'static str {
        "test.noSend"
    }
    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        events
            .iter()
            .filter(|s| matches!(s.event, SignalingNetworkEvent::SendCalled { .. }))
            .map(|_| "a send was observed".to_string())
            .collect()
    }
}

#[tokio::test]
async fn peer_rule_fails_close_in_recorder_mode() {
    let recorder = Recorder::fake();
    let opts_audit = ScopedAuditOptions {
        rules: vec![Arc::new(NoSendRule)],
        ..Default::default()
    };
    let wrapped = with_all_contracts(
        sim(),
        recorder,
        RunContext::TestWithRecorder,
        opts_audit,
        false,
    );
    let net = wrapped.network.clone();

    {
        let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
        a.send_to(b"x", "10.0.0.2:5060".parse().unwrap())
            .await
            .unwrap();
        // a drops → per-bind NoSendRule fires (deferred-fail).
    }

    let err = wrapped
        .recording
        .close()
        .await
        .expect_err("peer rule violation must fail close");
    assert_eq!(err.check, "test.noSend");
}

#[tokio::test]
async fn real_run_silences_rules() {
    let recorder = Recorder::fake();
    let opts_audit = ScopedAuditOptions {
        rules: vec![Arc::new(NoSendRule)],
        ..Default::default()
    };
    // Same rule + same traffic, but RealRun → rules don't fire.
    let wrapped = with_all_contracts(sim(), recorder, RunContext::RealRun, opts_audit, false);
    let net = wrapped.network.clone();

    {
        let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
        a.send_to(b"x", "10.0.0.2:5060".parse().unwrap())
            .await
            .unwrap();
        net.await_in_flight(Duration::from_secs(1)).await;
    }

    assert!(wrapped.recording.close().await.is_ok());
}

// ---------------------------------------------------------------------------
// Record-at-demux (newkahneed-036 ask A): arrivals are recorded at DELIVERY
// into the inbox, consumption separately at recv — so a message the body never
// reads is still on the trace, distinguishably.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unconsumed_arrival_is_recorded_at_delivery_and_noted() {
    let recorder = Recorder::fake();
    let wrapped = with_all_contracts(
        sim(),
        recorder,
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        true,
    );
    let net = wrapped.network.clone();

    let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
    let b = net.bind_udp(opts("10.0.0.2:5060", 64)).await.unwrap();
    a.send_to(b"ACK sip:x SIP/2.0\r\n", b.local_addr()).await.unwrap();
    net.await_in_flight(Duration::from_secs(1)).await;
    // b NEVER calls recv().

    let snapshot = wrapped.recording.channel().snapshot();
    assert!(
        snapshot.iter().any(|s| matches!(
            &s.event,
            SignalingNetworkEvent::RecvItem {
                disposition: sip_net::RecvDisposition::Delivered,
                ..
            }
        )),
        "arrival must be recorded at delivery, without any recv()"
    );
    assert!(
        !snapshot
            .iter()
            .any(|s| matches!(&s.event, SignalingNetworkEvent::RecvConsumed { .. })),
        "no consumption marker without a recv()"
    );

    let entries = sip_net::to_sip_entries(&snapshot);
    let e = entries.iter().find(|e| e.raw.starts_with(b"ACK")).unwrap();
    assert!(e.delivered, "the wire fact is delivery");
    assert_eq!(e.recv_note, Some(sip_net::RecvNote::Unconsumed));

    // The audit view keeps the arrival (that is the whole point).
    assert!(snapshot
        .iter()
        .filter(|s| matches!(&s.event, SignalingNetworkEvent::RecvItem { .. }))
        .all(|s| sip_net::audit_visible_event(&s.event)));

    drop(b);
    drop(a);
}

#[tokio::test]
async fn consumed_arrival_pairs_with_its_consumption_marker() {
    let recorder = Recorder::fake();
    let wrapped = with_all_contracts(
        sim(),
        recorder,
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        true,
    );
    let net = wrapped.network.clone();

    let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
    let b = net.bind_udp(opts("10.0.0.2:5060", 64)).await.unwrap();
    a.send_to(b"OPTIONS sip:x SIP/2.0\r\n", b.local_addr()).await.unwrap();
    let _ = timeout(Duration::from_secs(1), b.recv()).await.unwrap();

    let snapshot = wrapped.recording.channel().snapshot();
    let item_seq = snapshot
        .iter()
        .find(|s| matches!(&s.event, SignalingNetworkEvent::RecvItem { .. }))
        .expect("arrival recorded")
        .seq;
    let consumed_seq = snapshot
        .iter()
        .find(|s| matches!(&s.event, SignalingNetworkEvent::RecvConsumed { .. }))
        .expect("consumption recorded")
        .seq;
    assert!(item_seq < consumed_seq, "arrival sequences before consumption");

    let entries = sip_net::to_sip_entries(&snapshot);
    let e = entries.iter().find(|e| e.raw.starts_with(b"OPTIONS")).unwrap();
    assert_eq!(e.recv_note, None, "a consumed message carries no note");
}

#[tokio::test]
async fn overflow_arrival_is_recorded_with_its_disposition() {
    let recorder = Recorder::fake();
    let wrapped = with_all_contracts(
        sim(),
        recorder,
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        true,
    );
    let net = wrapped.network.clone();

    let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
    let b = net.bind_udp(opts("10.0.0.2:5060", 1)).await.unwrap();
    a.send_to(b"NOTIFY one SIP/2.0\r\n", b.local_addr()).await.unwrap();
    net.await_in_flight(Duration::from_secs(1)).await;
    a.send_to(b"NOTIFY two SIP/2.0\r\n", b.local_addr()).await.unwrap();
    net.await_in_flight(Duration::from_secs(1)).await;

    let snapshot = wrapped.recording.channel().snapshot();
    let dispositions: Vec<_> = snapshot
        .iter()
        .filter_map(|s| match &s.event {
            SignalingNetworkEvent::RecvItem { disposition, .. } => Some(*disposition),
            _ => None,
        })
        .collect();
    assert_eq!(
        dispositions,
        vec![
            sip_net::RecvDisposition::Delivered,
            sip_net::RecvDisposition::InboxOverflow
        ],
        "the overflowed datagram is still a recorded arrival"
    );

    let entries = sip_net::to_sip_entries(&snapshot);
    let two = entries.iter().find(|e| e.raw.starts_with(b"NOTIFY two")).unwrap();
    assert_eq!(two.recv_note, Some(sip_net::RecvNote::InboxOverflow));
    assert!(two.delivered, "it arrived on the wire — overflow is inbox-side");

    // Overflow arrivals stay audit-visible (true wire), unlike modeled loss.
    assert!(sip_net::audit_visible_event(
        &snapshot
            .iter()
            .filter(|s| matches!(&s.event, SignalingNetworkEvent::RecvItem { .. }))
            .last()
            .unwrap()
            .event
    ));
}

#[test]
fn audit_view_hides_consumption_markers_and_modeled_loss() {
    use sip_net::{RecvDisposition, UdpPacket};
    let pkt = UdpPacket {
        raw: b"BYE sip:x SIP/2.0\r\n".to_vec(),
        src: "10.0.0.1:5060".parse().unwrap(),
        arrival_ms: 7,
    };
    let item = |disposition| SignalingNetworkEvent::RecvItem {
        bind_key: "10.0.0.2:5060".to_string(),
        packet: pkt.clone(),
        disposition,
    };
    assert!(sip_net::audit_visible_event(&item(RecvDisposition::Delivered)));
    assert!(sip_net::audit_visible_event(&item(RecvDisposition::InboxOverflow)));
    assert!(!sip_net::audit_visible_event(&item(RecvDisposition::InboxClosed)));
    assert!(!sip_net::audit_visible_event(&item(RecvDisposition::LossModel)));
    assert!(!sip_net::audit_visible_event(&item(RecvDisposition::AbsorbedRetransmit)));
    assert!(!sip_net::audit_visible_event(&SignalingNetworkEvent::RecvConsumed {
        bind_key: "10.0.0.2:5060".to_string(),
        packet: pkt,
    }));
}
