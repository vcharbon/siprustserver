use super::*;
use sip_net::{BindUdpOpts, RealSignalingNetwork, SignalingNetwork};
use std::net::SocketAddr;
use tokio::net::UdpSocket as TokioUdp;
use tokio::time::{timeout, Duration as TokioDuration};

async fn txn_rig() -> (Arc<CallTxns>, TokioUdp, SocketAddr) {
    txn_rig_with(None).await
}

async fn txn_rig_with(taps: Option<Arc<Taps>>) -> (Arc<CallTxns>, TokioUdp, SocketAddr) {
    // The engine sends through the same fabric seam the mux binds on.
    let endpoint: Arc<dyn UdpEndpoint> = Arc::from(
        RealSignalingNetwork::new()
            .bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 64))
            .await
            .unwrap(),
    );
    let peer = TokioUdp::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let txns = Arc::new(CallTxns::new(
        endpoint,
        Arc::new(DropModel::new(0.0, 1, None)),
        Arc::new(MuxStats::new(4)),
        taps,
    ));
    (txns, peer, peer_addr)
}

async fn recv_one(peer: &TokioUdp, window_ms: u64) -> Option<String> {
    let mut buf = vec![0u8; 2048];
    match timeout(TokioDuration::from_millis(window_ms), peer.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(String::from_utf8_lossy(&buf[..n]).to_string()),
        _ => None,
    }
}

fn update_req(branch: &str) -> Vec<u8> {
    format!(
        "UPDATE sip:b@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: 2 UPDATE\r\n\r\n"
    )
    .into_bytes()
}

fn resp(status: u16, branch: &str, cseq: &str, extra: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: {cseq}\r\n{extra}\r\n"
    )
    .into_bytes()
}

fn prack_req(branch: &str, rack: &str) -> Vec<u8> {
    format!(
        "PRACK sip:b@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5061;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n\r\n"
    )
    .into_bytes()
}

/// Timer E is METHOD-GENERIC: an outbound UPDATE (a non-INVITE the engine has
/// no per-method code for) is retransmitted after ~T1 and the resender stops
/// on its final response; a duplicate of that response is then absorbed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow lane: loadgen"]
async fn calltxns_timer_e_is_method_generic_for_update() {
    let (txns, peer, peer_addr) = txn_rig().await;

    txns.on_outbound(&update_req("z9hG4bK-u1"), peer_addr);
    let got = recv_one(&peer, 2_000).await.expect("Timer E retransmit of the UPDATE");
    assert!(got.starts_with("UPDATE "), "retransmit is the UPDATE: {got}");

    // The 200 (UPDATE) stops the resender…
    let ok = resp(200, "z9hG4bK-u1", "2 UPDATE", "");
    assert!(txns.on_inbound(&ok, peer_addr), "first 200 (UPDATE) is delivered");
    // …and its duplicate is absorbed (method-generic dedup).
    assert!(!txns.on_inbound(&ok, peer_addr), "duplicate 200 (UPDATE) absorbed");
    assert!(
        recv_one(&peer, 1_500).await.is_none(),
        "UPDATE resender must stop on the final response"
    );
    txns.shutdown();
}

/// Duplicate absorption + reactive re-answer are method-generic: a
/// retransmitted inbound PRACK is absorbed and our recorded 200 (PRACK) is
/// re-sent (the peer's copy was evidently lost).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow lane: loadgen"]
async fn calltxns_absorbs_duplicate_prack_and_reanswers() {
    let (txns, peer, peer_addr) = txn_rig().await;

    let prack = prack_req("z9hG4bK-p1", "1 1 INVITE");
    assert!(txns.on_inbound(&prack, peer_addr), "first PRACK is delivered");
    txns.on_outbound(&resp(200, "z9hG4bK-p1", "2 PRACK", ""), peer_addr);

    assert!(!txns.on_inbound(&prack, peer_addr), "duplicate PRACK absorbed");
    let got = recv_one(&peer, 1_000).await.expect("reactive re-answer to the dup PRACK");
    assert!(got.starts_with("SIP/2.0 200"), "re-answer is our 200 (PRACK): {got}");
    txns.shutdown();
}

/// RFC 3262 §3: a RELIABLE provisional we send (Require:100rel + RSeq) is
/// retransmitted until the matching PRACK (RAck response-num = its RSeq)
/// arrives — otherwise a dropped reliable 183 is unrecoverable (the
/// peer's INVITE resender already stopped on the 100 Trying). A plain 18x
/// stays best-effort (never proactively retransmitted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow lane: loadgen"]
async fn calltxns_retransmits_reliable_1xx_until_prack() {
    let (txns, peer, peer_addr) = txn_rig().await;

    // A plain 180 arms NO resender (best-effort by design).
    txns.on_outbound(&resp(180, "z9hG4bK-i1", "1 INVITE", ""), peer_addr);
    assert!(
        recv_one(&peer, 1_200).await.is_none(),
        "a non-reliable 18x must not be proactively retransmitted"
    );

    // A reliable 183 IS retransmitted until its PRACK.
    let r183 = resp(183, "z9hG4bK-i1", "1 INVITE", "Require: 100rel\r\nRSeq: 1\r\n");
    txns.on_outbound(&r183, peer_addr);
    let got = recv_one(&peer, 2_000).await.expect("reliable 183 retransmit");
    assert!(got.starts_with("SIP/2.0 183"), "retransmit is the reliable 183: {got}");

    // The matching PRACK stops it (and is delivered to the app).
    assert!(txns.on_inbound(&prack_req("z9hG4bK-p2", "1 1 INVITE"), peer_addr));
    assert!(
        recv_one(&peer, 1_500).await.is_none(),
        "reliable-183 resender must stop on the matching PRACK"
    );
    txns.shutdown();
}

/// A response with an explicit To-tag (a forking UAS emits several 18x with
/// DISTINCT To-tags on one INVITE branch, RFC 3261 §12.1.2).
fn resp_tag(status: u16, branch: &str, cseq: &str, to_tag: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag={to_tag}\r\nCSeq: {cseq}\r\n\r\n"
    )
    .into_bytes()
}

/// TRUE FORKING — two 18x with DISTINCT To-tags on ONE INVITE branch must
/// BOTH be delivered to the body (each is a separate early dialog, §12.1.2),
/// never collapsed as a retransmit. A same-tag repeat still dedups. The
/// discriminator is `(branch, status, To-tag)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow lane: loadgen"]
async fn calltxns_distinct_fork_tags_are_not_deduped() {
    let (txns, _peer, peer_addr) = txn_rig().await;

    // Fork 1's 180 (tag=f1) — delivered.
    assert!(
        txns.on_inbound(&resp_tag(180, "z9hG4bK-i1", "1 INVITE", "f1"), peer_addr),
        "the first fork's 180 is delivered"
    );
    // Fork 2's 180 (tag=f2) on the SAME branch — a DISTINCT early dialog, so
    // it must be delivered too (not absorbed as a retransmit).
    assert!(
        txns.on_inbound(&resp_tag(180, "z9hG4bK-i1", "1 INVITE", "f2"), peer_addr),
        "a distinct-tag fork's 180 must NOT be deduped as a retransmit"
    );
    // Fork 1's 180 again (tag=f1) — a genuine retransmit, absorbed.
    assert!(
        !txns.on_inbound(&resp_tag(180, "z9hG4bK-i1", "1 INVITE", "f1"), peer_addr),
        "a same-tag repeat is a true retransmit and IS deduped"
    );
    // A losing fork's LATE 200 (tag=f2) is also delivered (the winner is f1,
    // but the body must see f2's 200 to ACK+BYE it, §13.2.2.4).
    assert!(
        txns.on_inbound(&resp_tag(200, "z9hG4bK-i1", "1 INVITE", "f2"), peer_addr),
        "a losing fork's late 200 (distinct tag) reaches the body for ACK+BYE"
    );
    txns.shutdown();
}

fn ack_req(branch: &str, cseq: u32) -> Vec<u8> {
    format!(
        "ACK sip:b@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: {cseq} ACK\r\n\r\n"
    )
    .into_bytes()
}

/// Always-display re-emission: the send tap sees
/// EVERY recovery frame the engine puts on the wire, tagged by kind — the
/// proactive Timer A/E/G retransmit, the reactive re-ACK on a duplicate 2xx,
/// and the reactive re-answer to a duplicate request. Without it these hit the
/// socket below the recording layer and never reach the ladder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow lane: loadgen"]
async fn calltxns_sendtap_records_every_reemission() {
    let (txns, peer, peer_addr) = txn_rig().await;
    let seen = Arc::new(Mutex::new(Vec::<ReEmitKind>::new()));
    let sink = seen.clone();
    txns.set_sendtap(Arc::new(move |_raw, _dst, kind| {
        sink.lock().unwrap().push(kind);
    }));

    // (1) Proactive Timer-E retransmit of an outbound UPDATE, then stop it.
    txns.on_outbound(&update_req("z9hG4bK-u1"), peer_addr);
    recv_one(&peer, 2_000).await.expect("Timer E retransmit of the UPDATE");
    assert!(txns.on_inbound(&resp(200, "z9hG4bK-u1", "2 UPDATE", ""), peer_addr));

    // (2) Reactive re-answer to a duplicate request (our 200 was lost).
    let prack = prack_req("z9hG4bK-p1", "1 1 INVITE");
    assert!(txns.on_inbound(&prack, peer_addr));
    txns.on_outbound(&resp(200, "z9hG4bK-p1", "2 PRACK", ""), peer_addr);
    assert!(!txns.on_inbound(&prack, peer_addr), "dup PRACK absorbed → re-answer");

    // (3) Reactive re-ACK on a duplicate INVITE 2xx (our ACK was lost).
    let ok_inv = resp(200, "z9hG4bK-i9", "1 INVITE", "");
    assert!(txns.on_inbound(&ok_inv, peer_addr), "first 200 (INVITE) delivered");
    txns.on_outbound(&ack_req("z9hG4bK-a9", 1), peer_addr);
    assert!(!txns.on_inbound(&ok_inv, peer_addr), "dup 200 (INVITE) absorbed → re-ACK");

    let _ = recv_one(&peer, 200).await; // drain stragglers
    let kinds = seen.lock().unwrap().clone();
    assert!(kinds.contains(&ReEmitKind::Retransmit), "proactive retransmit tapped: {kinds:?}");
    assert!(kinds.contains(&ReEmitKind::ReAnswer), "reactive re-answer tapped: {kinds:?}");
    assert!(kinds.contains(&ReEmitKind::ReAck), "reactive re-ACK tapped: {kinds:?}");
    txns.shutdown();
}

/// Every re-send the engine makes reaches the call's tally through its
/// endpoint's taps: the proactive Timer E retransmit and the reactive re-ACK.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow lane: loadgen"]
async fn calltxns_counts_every_resend_on_the_tally() {
    use crate::mux::tally::CallTally;
    use sip_message::sniff::identity;

    let tally = Arc::new(CallTally::new("alice"));
    let taps = Taps::new(None, Some(tally.clone()), None, None);
    let (txns, peer, peer_addr) = txn_rig_with(taps).await;
    let (update, ack) = (update_req("z9hG4bK-u1"), ack_req("z9hG4bK-a9", 1));
    let now = tokio::time::Instant::now();
    tally.on_datagram(None, super::super::taps::Dir::Out, &identity(&update), now);
    tally.on_datagram(None, super::super::taps::Dir::Out, &identity(&ack), now);

    txns.on_outbound(&update, peer_addr);
    recv_one(&peer, 2_000).await.expect("Timer E retransmit of the UPDATE");
    assert!(txns.on_inbound(&resp(200, "z9hG4bK-u1", "2 UPDATE", ""), peer_addr));
    let ok_inv = resp(200, "z9hG4bK-i9", "1 INVITE", "");
    assert!(txns.on_inbound(&ok_inv, peer_addr));
    txns.on_outbound(&ack, peer_addr);
    assert!(!txns.on_inbound(&ok_inv, peer_addr), "dup 200 (INVITE) absorbed → re-ACK");
    txns.shutdown();

    let m = tally.finish();
    let resent = |label: &str| m.arrows.iter().find(|(k, _)| k.label == label).unwrap().1.resent;
    assert!(resent("UPDATE") >= 1, "the proactive retransmit: {m:?}");
    assert_eq!(resent("ACK"), 1, "the reactive re-ACK: {m:?}");
}
