//! A recorded call's sends through the mux endpoint, as its recording holds
//! them.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{Recorder, RunContext, TransportKind};
use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::{
    audit_wire_entries, to_sip_entries, RecordingSignalingNetwork, RecvNote, ScopedAuditOptions,
    SimulatedSignalingNetwork,
};

use crate::mux::{CallRouting, Correlation, EndpointSpec, MuxCore, Role};

const CALLER: &str = "127.0.0.1:5060";
const PEER: &str = "127.0.0.1:5070";

fn invite() -> Vec<u8> {
    format!(
        "INVITE sip:bob@{PEER} SIP/2.0\r\nMax-Forwards: 70\r\nContact: <sip:alice@{CALLER}>\r\n\
         Via: SIP/2.0/UDP {CALLER};branch=z9hG4bK-l1\r\nX-Loadgen-Id: lgLOSS\r\n\
         Call-ID: loss-1@h\r\nFrom: <sip:alice@h>;tag=a1\r\nTo: <sip:bob@h>\r\nCSeq: 1 INVITE\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A datagram the call's loss model drops on send never reaches its taker,
/// a peer outside the recording: the ladder and the audit's view of the wire
/// both hold it undelivered, lost to the loss model, as a drop on arrival is.
#[tokio::test(start_paused = true)]
async fn a_send_the_loss_model_drops_is_recorded_lost() {
    let (caller, peer): (SocketAddr, SocketAddr) = (CALLER.parse().unwrap(), PEER.parse().unwrap());
    let sim = SimulatedSignalingNetwork::new(1);
    let core = MuxCore::bind_on(
        &sim,
        vec![EndpointSpec { addr: caller, role: Role::Caller }],
        Correlation::header("X-Loadgen-Id"),
        64,
        4,
        Duration::from_secs(5),
        Clock::test_at(0),
    )
    .await
    .unwrap();
    let lossy = core.network_tuned(CallRouting::new("lgLOSS"), 1.0, false, 7, None);
    let recording = RecordingSignalingNetwork::new(
        Arc::new(lossy),
        Recorder::with_clock(TransportKind::Fake, Clock::test_at(0)),
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
    );
    let alice = recording.bind_udp(BindUdpOpts::new(caller, 64)).await.unwrap();
    alice.send_to(&invite(), peer).await.unwrap();

    let events = recording.channel().snapshot();
    for (view, entries) in
        [("ladder", to_sip_entries(&events)), ("audit", audit_wire_entries(&events))]
    {
        let [entry] = entries.as_slice() else { panic!("{view}: {} entries", entries.len()) };
        let seen = (entry.start_line(), entry.delivered, entry.recv_note);
        assert_eq!(
            seen,
            ("INVITE sip:bob@127.0.0.1:5070 SIP/2.0".to_string(), false, Some(RecvNote::LossModel)),
            "{view}: the dropped INVITE is undelivered, lost to the loss model"
        );
    }
}

/// Every datagram the loss model drops reaches the call's loss tap and its
/// recording, the retransmit engine's re-sends included: the INVITE dropped
/// on its first send and again on its Timer A re-send is noted twice, each
/// time leaving the caller.
#[tokio::test(start_paused = true)]
async fn each_drop_of_a_send_and_its_resend_is_noted_and_recorded() {
    let (caller, peer): (SocketAddr, SocketAddr) = (CALLER.parse().unwrap(), PEER.parse().unwrap());
    let sim = SimulatedSignalingNetwork::new(1);
    let core = MuxCore::bind_on(
        &sim,
        vec![EndpointSpec { addr: caller, role: Role::Caller }],
        Correlation::header("X-Loadgen-Id"),
        64,
        4,
        Duration::from_secs(5),
        Clock::test_at(0),
    )
    .await
    .unwrap();
    let notes = scenario_harness::realcall::LossNotes::default();
    let lossy = core
        .network_tuned(CallRouting::new("lgLOSS"), 1.0, true, 7, None)
        .with_loss_tap(crate::mux::notes_tap(notes.clone()));
    let recording = RecordingSignalingNetwork::new(
        Arc::new(lossy),
        Recorder::with_clock(TransportKind::Fake, Clock::test_at(0)),
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
    );
    let alice = recording.bind_udp(BindUdpOpts::new(caller, 64)).await.unwrap();
    alice.send_to(&invite(), peer).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    let lost = scenario_harness::realcall::LostMessage {
        leg: None,
        inbound: false,
        method: Some("INVITE".to_string()),
        status: None,
        cseq_method: "INVITE".to_string(),
        sdp: false,
        call_id: "loss-1@h".to_string(),
        from_tag: "a1".to_string(),
        cseq: 1,
    };
    assert_eq!(notes.all(), vec![lost.clone(), lost], "the first send and the Timer A re-send");
    let recorded = recording
        .channel()
        .snapshot()
        .iter()
        .filter(|s| matches!(s.event, sip_net_testkit::SignalingNetworkEvent::SendLost { .. }))
        .count();
    assert_eq!(recorded, 2, "both drops on the recording");
}
