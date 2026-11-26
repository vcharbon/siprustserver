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
        reliable: false,
        to_tag: String::new(),
        call_id: "loss-1@h".to_string(),
        from_tag: "a1".to_string(),
        cseq: 1,
        arrived_before: vec![],
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

/// The 200 the peer sends to the caller's `method` request `cseq`.
fn ok(cseq: u32, method: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP {CALLER};branch=z9hG4bK-l{cseq}{method}\r\n\
         Call-ID: loss-1@h\r\nFrom: <sip:alice@h>;tag=a1\r\nTo: <sip:bob@h>;tag=b1\r\n\
         CSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A drop is noted with the responses the mux delivered to the endpoint
/// before it, in the network's order: the CANCEL's 200 delivered, then the
/// INVITE's 200 dropped in the same burst, is noted as lost after that 200
/// while the caller has yet to read it. The note leads the caller's reading
/// of the 200, so only the note's own record orders the two.
#[tokio::test(start_paused = true)]
async fn a_drop_is_noted_after_the_responses_delivered_before_it() {
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
    let answer_lost = crate::TargetedDrop::final_response("INVITE", 1, crate::DropDir::Inbound);
    let lossy = core
        .network_tuned(CallRouting::new("lgLOSS"), 0.0, false, 7, Some(answer_lost))
        .with_loss_tap(crate::mux::notes_tap(notes.clone()));
    let bob = sim.bind_udp(BindUdpOpts::new(peer, 64)).await.unwrap();
    let alice = lossy.bind_udp(BindUdpOpts::new(caller, 64)).await.unwrap();
    alice.send_to(&invite(), peer).await.unwrap();
    bob.send_to(&ok(1, "CANCEL"), caller).await.unwrap();
    bob.send_to(&ok(1, "INVITE"), caller).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let lost = notes.all();
    let [lost] = lost.as_slice() else { panic!("one drop: {lost:?}") };
    assert_eq!((lost.status, lost.cseq_method.as_str()), (Some(200), "INVITE"));
    assert_eq!(lost.arrived_before, vec![(200, "CANCEL".to_string())]);
    let read = alice.recv().await.expect("the CANCEL's 200, still unread");
    assert_eq!(read.raw[..], ok(1, "CANCEL")[..]);
}

/// The ACK alice sends for her INVITE's final, carrying the call's key.
fn ack() -> Vec<u8> {
    format!(
        "ACK sip:bob@{PEER} SIP/2.0\r\nMax-Forwards: 70\r\n\
         Via: SIP/2.0/UDP {CALLER};branch=z9hG4bK-l1\r\nX-Loadgen-Id: lgLOSS\r\n\
         Call-ID: loss-1@h\r\nFrom: <sip:alice@h>;tag=a1\r\nTo: <sip:bob@h>;tag=b1\r\nCSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The departure tap of a call `network` builds, and the methods or statuses
/// it saw leave, in order.
fn departures(network: crate::mux::MuxNetwork) -> (crate::mux::MuxNetwork, DepartureLog) {
    let log = DepartureLog::default();
    let noted = log.clone();
    let tap: crate::mux::DepartureTap = Arc::new(move |raw| {
        let id = sip_message::sniff::identity(raw);
        let what = id.method.map(str::to_string).or(id.status.map(|s| s.to_string()));
        noted.lock().unwrap().push(what.unwrap_or_default());
    });
    (network.with_departure_tap(tap), log)
}

type DepartureLog = Arc<std::sync::Mutex<Vec<String>>>;

/// A send the loss model drops never departs; the engine's Timer A re-send
/// of it does, when it leaves.
#[tokio::test(start_paused = true)]
async fn a_dropped_send_never_departs_and_its_resend_does() {
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
    let first_lost = crate::TargetedDrop::request("INVITE", 1, crate::DropDir::Outbound);
    let (lossy, log) =
        departures(core.network_tuned(CallRouting::new("lgLOSS"), 0.0, true, 7, Some(first_lost)));
    let alice = lossy.bind_udp(BindUdpOpts::new(caller, 64)).await.unwrap();
    alice.send_to(&invite(), peer).await.unwrap();
    assert!(log.lock().unwrap().is_empty(), "the dropped first copy never left");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(*log.lock().unwrap(), ["INVITE"], "the Timer A re-send left");
}

/// A datagram the loss model holds departs when its hold ends: an ACK held,
/// a request sent after it leaves first.
#[tokio::test(start_paused = true)]
async fn a_held_send_departs_when_its_hold_ends() {
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
    let held = crate::mux::NetFaults {
        dup_rate: 0.0,
        late_rate: 1.0,
        late_max: Duration::from_millis(300),
        echo_rate: 0.0,
    };
    let tuned = core.network_tuned(CallRouting::new("lgLOSS"), 0.0, false, 7, None);
    let (lossy, log) = departures(tuned.with_faults(held));
    let alice = lossy.bind_udp(BindUdpOpts::new(caller, 64)).await.unwrap();
    alice.send_to(&ack(), peer).await.unwrap();
    alice.send_to(&invite(), peer).await.unwrap();
    assert_eq!(*log.lock().unwrap(), ["INVITE"], "the held ACK has not left yet");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(*log.lock().unwrap(), ["INVITE", "ACK"], "the ACK left when its hold ended");
}
