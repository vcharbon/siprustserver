//! The overtaken-request fact and the failures it excuses, read from a
//! recording of a simulated fabric between the peer `S` (192.0.2.1) and the
//! UA `U` (192.0.2.2), which loses the peer's first datagram when asked.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{Recorder, RunContext};
use scenario_harness::actor::FailedAt;
use scenario_harness::StepError;
use sip_net::BindUdpOpts;
use sip_net_testkit::{with_all_contracts, Fate, ScopedAuditOptions, SimulatedSignalingNetwork};

use super::{excuses, overtaken_requests, Overtaken};

const PEER: &str = "192.0.2.1:5060";
const UA: &str = "192.0.2.2:5060";

/// A request of the peer `S` on the dialog `S`/`U`, its branch named by its
/// CSeq, or by `branch` for a new transaction reusing it.
fn request_on(method: &str, cseq: u32, branch: &str) -> String {
    format!(
        "{method} sip:u@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP {PEER};branch=z9hG4bK-{branch}\r\n\
         From: <sip:s@host>;tag=S\r\nTo: <sip:u@host>;tag=U\r\nCall-ID: c1@host\r\n\
         CSeq: {cseq} {method}\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    )
}

fn request(method: &str, cseq: u32) -> String {
    request_on(method, cseq, &cseq.to_string())
}

/// The UA `U`'s `status` to the request numbered `cseq` of `method` and
/// `branch`.
fn answer_on(status: u16, method: &str, cseq: u32, branch: &str) -> String {
    format!(
        "SIP/2.0 {status} Reason\r\nVia: SIP/2.0/UDP {PEER};branch=z9hG4bK-{branch}\r\n\
         From: <sip:s@host>;tag=S\r\nTo: <sip:u@host>;tag=U\r\nCall-ID: c1@host\r\n\
         CSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
    )
}

fn answer(status: u16, method: &str, cseq: u32) -> String {
    answer_on(status, method, cseq, &cseq.to_string())
}

/// One datagram: the peer's request, or the UA's answer.
enum Sent {
    Peer(String),
    Ua(String),
}

/// The overtaken requests after `sent`, on a fabric that loses the peer's
/// first datagram when `lose`.
async fn overtaken_after(lose: bool, sent: &[Sent]) -> Vec<Overtaken> {
    let peer: SocketAddr = PEER.parse().unwrap();
    let first = AtomicBool::new(lose);
    let fabric = SimulatedSignalingNetwork::new(1).with_send_fate(Arc::new(move |src, _, _| {
        if src == peer && first.swap(false, Ordering::SeqCst) {
            Fate::Lose
        } else {
            Fate::Deliver
        }
    }));
    let wrapped = with_all_contracts(
        Arc::new(fabric),
        Recorder::fake(),
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        true,
    );
    let net = wrapped.network.clone();
    let s = net.bind_udp(BindUdpOpts::new(peer, 64)).await.unwrap();
    let u = net.bind_udp(BindUdpOpts::new(UA.parse().unwrap(), 64)).await.unwrap();
    for datagram in sent {
        match datagram {
            Sent::Peer(raw) => s.send_to(raw.as_bytes(), u.local_addr()).await.unwrap(),
            Sent::Ua(raw) => u.send_to(raw.as_bytes(), s.local_addr()).await.unwrap(),
        }
        net.await_in_flight(Duration::from_secs(1)).await;
    }
    overtaken_requests(&wrapped.recording.channel().snapshot())
}

/// The `method` numbered 2 lost, the UPDATE numbered 3 overtaking it, the
/// retransmission refused 500 (RFC 3261 §12.2.2), then the UA's
/// `invite_final` to the INVITE numbered 1.
fn overtaken_by_an_update(method: &str, invite_final: u16) -> [Sent; 6] {
    [
        Sent::Peer(request(method, 2)),
        Sent::Peer(request("UPDATE", 3)),
        Sent::Ua(answer(200, "UPDATE", 3)),
        Sent::Peer(request(method, 2)),
        Sent::Ua(answer(500, method, 2)),
        Sent::Ua(answer(invite_final, "INVITE", 1)),
    ]
}

/// The overtaken PRACK whose INVITE the UA then gave up 504.
async fn prack_given_up() -> Vec<Overtaken> {
    overtaken_after(true, &overtaken_by_an_update("PRACK", 504)).await
}

/// The roles of the recording: `s` is the peer, `u` the UA, `x` a party off
/// their leg; anything else is a barrier.
fn party(role: &str) -> Option<SocketAddr> {
    match role {
        "s" => Some(PEER.parse().unwrap()),
        "u" => Some(UA.parse().unwrap()),
        "x" => Some("192.0.2.3:5060".parse().unwrap()),
        _ => None,
    }
}

/// The failure of `who` at its goal `kind`, the `established` barrier
/// pending.
fn at_goal(who: &'static str, kind: &'static str) -> FailedAt {
    FailedAt { who, goal: Some((kind, 0)), barrier: "established" }
}

fn wrong_status(who: &str, got: u16) -> StepError {
    StepError::WrongStatus {
        who: who.to_string(),
        expected: 200,
        got,
        reason: "Reason".to_string(),
    }
}

fn barrier_timeout(barrier: &str) -> StepError {
    StepError::timeout(barrier, None, format!("the call state to reach {barrier}"))
}

/// The overtaken request's 500 reached its sender; the INVITE's 504 after an
/// overtaken PRACK is its give-up, and a 2xx to it shows the call set up.
#[tokio::test(start_paused = true)]
async fn a_500_to_an_overtaken_lost_request_holds() {
    let overtaken = prack_given_up().await;
    assert_eq!(overtaken.len(), 1, "{overtaken:?}");
    assert_eq!(overtaken[0].method, "PRACK");
    let peer: SocketAddr = PEER.parse().unwrap();
    assert_eq!(overtaken[0].refused, [peer]);
    assert_eq!(overtaken[0].invite_failed, [peer], "the UA's 504 to the INVITE");
    assert!(!overtaken[0].answered);
    let info = overtaken_after(true, &overtaken_by_an_update("INFO", 504)).await;
    assert!(info[0].invite_failed.is_empty(), "an INFO acknowledges no provisional");
    let answered = overtaken_after(true, &overtaken_by_an_update("PRACK", 200)).await;
    assert!(answered[0].answered && answered[0].invite_failed.is_empty(), "{answered:?}");
}

/// Nothing lost, nothing overtaking the lost request, a final other than
/// 500 to it, or a 500 to a new transaction reusing its number: the fact
/// does not hold.
#[tokio::test(start_paused = true)]
async fn any_other_500_or_loss_does_not_hold() {
    assert!(overtaken_after(false, &overtaken_by_an_update("PRACK", 504)).await.is_empty());
    let alone = [
        Sent::Peer(request("PRACK", 2)),
        Sent::Peer(request("PRACK", 2)),
        Sent::Ua(answer(500, "PRACK", 2)),
    ];
    assert!(overtaken_after(true, &alone).await.is_empty(), "not overtaken");
    let other = [
        Sent::Peer(request("PRACK", 2)),
        Sent::Peer(request("UPDATE", 3)),
        Sent::Ua(answer(500, "UPDATE", 3)),
    ];
    assert!(overtaken_after(true, &other).await.is_empty(), "a 500 to another request");
    let reused = [
        Sent::Peer(request("INFO", 2)),
        Sent::Peer(request("UPDATE", 3)),
        Sent::Peer(request_on("INFO", 2, "reused")),
        Sent::Ua(answer_on(500, "INFO", 2, "reused")),
    ];
    assert!(overtaken_after(true, &reused).await.is_empty(), "a 500 to a new transaction");
}

/// The overtaken PRACK's own 500, the INVITE's 5xx when its reliable
/// provisional went unacknowledged, and a barrier's timeout on a call its
/// leg never set up, whichever wait gave up on it, are excused.
#[tokio::test(start_paused = true)]
async fn the_overtaken_requests_transaction_and_its_consequence_on_its_leg_are_excused() {
    let overtaken = prack_given_up().await;
    assert!(excuses(&overtaken, &at_goal("s", "early_request"), &wrong_status("s", 500), party));
    assert!(excuses(&overtaken, &at_goal("s", "invite"), &wrong_status("s", 504), party));
    let held = barrier_timeout("early_exchanged");
    assert!(excuses(&overtaken, &at_goal("u", "final"), &held, party), "a party's wait");
    assert!(excuses(&overtaken, &at_goal("x", "bye"), &barrier_timeout("merged"), party));
    let barrier = FailedAt::at_barrier("established");
    assert!(excuses(&overtaken, &barrier, &barrier_timeout("established"), party), "its own");
}

/// An unrelated failure stays one: another status, another cause, a 500 to
/// a party the refusal did not reach, a barrier's timeout on a call the
/// overtaken request's leg set up, and a 5xx to the INVITE when the
/// overtaken request is no PRACK.
#[tokio::test(start_paused = true)]
async fn a_failure_unrelated_to_the_overtaken_request_stays_a_failure() {
    let overtaken = prack_given_up().await;
    let at = at_goal("s", "bye");
    assert!(!excuses(&overtaken, &at, &wrong_status("s", 481), party), "another status");
    let method = StepError::WrongMethod {
        who: "s".to_string(),
        expected: "BYE".to_string(),
        got: "INFO".to_string(),
    };
    assert!(!excuses(&overtaken, &at, &method, party), "another cause");
    let own_timeout = StepError::timeout("s", None, "BYE");
    assert!(!excuses(&overtaken, &at, &own_timeout, party), "the party's own wait");
    assert!(!excuses(&overtaken, &at_goal("u", "bye"), &wrong_status("u", 500), party));
    assert!(!excuses(&overtaken, &at_goal("x", "bye"), &wrong_status("x", 500), party));
    let answered = overtaken_after(true, &overtaken_by_an_update("PRACK", 200)).await;
    let barrier = FailedAt::at_barrier("merged");
    assert!(!excuses(&answered, &barrier, &barrier_timeout("merged"), party), "set up");
    let info = overtaken_after(true, &overtaken_by_an_update("INFO", 504)).await;
    assert!(!excuses(&info, &at_goal("s", "invite"), &wrong_status("s", 504), party));
    assert!(!excuses(&[], &at_goal("s", "early_request"), &wrong_status("s", 500), party));
}

/// The barrier's timeouts, the controller's own and a party's wait, on an
/// answered call.
fn barrier_timeouts_excused(overtaken: &[Overtaken]) -> [bool; 2] {
    let own = excuses(
        overtaken,
        &FailedAt::at_barrier("transferred"),
        &barrier_timeout("transferred"),
        party,
    );
    let waited = excuses(overtaken, &at_goal("s", "bye"), &barrier_timeout("transferred"), party);
    [own, waited]
}

/// A REFER, or an INVITE inside its dialog, overtaken on an answered call and
/// never made good: the call state waited for behind it never comes, and the
/// barrier's timeout is excused, whichever wait gave up; the 2xx to the
/// INVITE that set the call up, sent after the refusal, is no later INVITE.
#[tokio::test(start_paused = true)]
async fn a_barrier_behind_an_overtaken_refer_or_reinvite_left_undone_is_excused() {
    for method in ["REFER", "INVITE"] {
        let overtaken = overtaken_after(true, &overtaken_by_an_update(method, 200)).await;
        assert!(overtaken[0].answered && overtaken[0].left_undone, "{overtaken:?}");
        assert_eq!(barrier_timeouts_excused(&overtaken), [true; 2], "{method}");
    }
}

/// A REFER or an in-dialog INVITE a later one of its method on its dialog
/// made good, and any other overtaken request on an answered call, leave a
/// barrier's timeout a failure.
#[tokio::test(start_paused = true)]
async fn a_barrier_behind_an_overtaken_request_made_good_or_of_another_method_stays_a_failure() {
    for method in ["REFER", "INVITE"] {
        let mut sent = Vec::from(overtaken_by_an_update(method, 200));
        let success = if method == "REFER" { 202 } else { 200 };
        sent.extend([Sent::Peer(request(method, 4)), Sent::Ua(answer(success, method, 4))]);
        let overtaken = overtaken_after(true, &sent).await;
        assert!(!overtaken[0].left_undone, "{overtaken:?}");
        assert_eq!(barrier_timeouts_excused(&overtaken), [false; 2], "{method} made good");
    }
    for method in ["INFO", "UPDATE", "PRACK"] {
        let overtaken = overtaken_after(true, &overtaken_by_an_update(method, 200)).await;
        assert_eq!(barrier_timeouts_excused(&overtaken), [false; 2], "{method}");
    }
}
