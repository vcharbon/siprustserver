//! **A returned primary owns the INVITE its takeover copy dialled.** The
//! primary dies with its new call undecided; the backup takes the call over on
//! the departure, asks the decision again and dials the callee. The primary
//! restarts and reclaims the call while the callee rings. Every leg's cookie
//! names the call's primary and backup, whichever of the two dialled it
//! (ADR-0055 §8), so once the primary is routable the callee's INVITE
//! responses and in-dialog requests reach the primary alone. The copy stands
//! down on the reclaim write even while its INVITE awaits a final, and the
//! primary takes that INVITE's client transaction over from the call record
//! (ADR-0047 D7): one owner, one 2xx sender, one CDR.
//!
//! ```text
//!   alice ─INVITE─▶ primary (decision in flight)          backup: the born call
//!                   ✗ primary killed, departure announced
//!                   backup asks again, dials bob ─INVITE─▶ bob ─180─▶ alice
//!                   primary restarts, reclaims, the copy stands down
//!   bob answers ─200─▶ primary ─200─▶ alice ─ACK─▶ primary ─ACK─▶ bob
//! ```
//!
//! Timing is the axis: the callee answers after the reclaim write applied at
//! the backup (the ending varies: the callee's BYE; the caller's re-INVITE
//! then the callee's BYE; the callee's REFER to a target that refuses), before
//! the primary is routable (the 2xx reaches the copy, the meeting follows), or
//! after the primary is routable but before its reclaim write is applied at
//! the backup. The copy also dials a new leg on an established call, a
//! transfer target, and the primary returns while that leg rings.
//!
//! Beyond the failover sweep: which node each answer reaches and which sends
//! each 2xx, ACK, BYE and CANCEL, and the caller's 2xx copies on one ladder.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::{default_call_refer, route_to_processing_refer};
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use failover_harness::{FailoverHarness, ProxySut, ReplicatedB2buaSut, WorkerHealth};
use scenario_harness::agent::{Agent, Dialog};
use scenario_harness::{ClientInvite, ServerTxn};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::departure_support::{bring_up_deciding, kill_announced, placed, CountedDecision};
use crate::inflight_resend::{settle_to_one_cdr, split_mut, REANSWER, REOFFER};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const CHARLIE: &str = "127.0.0.1:5090";

/// How long the routing decision takes to answer: past the kill.
const DECISION_DELAY: Duration = Duration::from_secs(1);
/// RFC 3261 §17.1.1.1 T1 and T2: the 2xx retransmission ladder (§13.3.1.4).
const T1_MS: u64 = 500;
const T2_MS: u64 = 4_000;

/// The cluster and the caller's new call, ringing the callee.
struct Ringing {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    charlie: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
    pri_ord: String,
    call_ref: String,
    call: ClientInvite,
    at_bob: ServerTxn,
    consults: Arc<AtomicUsize>,
}

impl Ringing {
    fn primary(&self) -> &ReplicatedB2buaSut {
        if self.pri_ord == "b1" {
            &self.w_b1
        } else {
            &self.w_b2
        }
    }

    fn copy(&self) -> &ReplicatedB2buaSut {
        if self.pri_ord == "b1" {
            &self.w_b2
        } else {
            &self.w_b1
        }
    }

    /// The SIP port of the call's primary, every incarnation alike.
    fn primary_port(&self) -> u16 {
        port_of(&self.pri_ord)
    }

    /// The SIP port of the backup that served the takeover copy.
    fn copy_port(&self) -> u16 {
        port_of(if self.pri_ord == "b1" { "b2" } else { "b1" })
    }
}

fn port_of(ordinal: &str) -> u16 {
    if ordinal == "b1" {
        5091
    } else {
        5092
    }
}

/// Routes every call to bob, a REFER processed locally and authorized by
/// the transfer instruction it carries.
fn routing_engine() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| NewCallResponse::Route(route_to_processing_refer("127.0.0.1", 5070)))
            .on_refer(default_call_refer)
            .build(),
    )
}

/// The new call's primary dies with the call undecided at its backup; the
/// backup takes the call over on the departure, asks the decision again and
/// dials the callee, who rings.
async fn ringing_at_the_copy(name: &str) -> Ringing {
    placed_call(name, true).await
}

/// The new call dialled by its primary, which lives: the callee rings.
async fn ringing_at_the_primary(name: &str) -> Ringing {
    placed_call(name, false).await
}

/// The caller's new call placed and ringing the callee; its primary killed
/// with the decision in flight when `killed`.
async fn placed_call(name: &str, killed: bool) -> Ringing {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let charlie = fh.agent("charlie", CHARLIE).await.transfer_target();
    let (decision, consults) = CountedDecision::counting(routing_engine(), DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    if killed {
        let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
        kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    let mut at_bob = bob.receive("INVITE").await;
    let asked = if killed { 2 } else { 1 };
    assert_eq!(consults.load(Ordering::SeqCst), asked, "who dials asked the decision");
    at_bob.respond(180, "Ringing").await;
    call.expect(180).await;
    Ringing {
        fh,
        alice,
        bob,
        charlie,
        proxy,
        w_b1,
        w_b2,
        pri_ord,
        call_ref,
        call,
        at_bob,
        consults,
    }
}

/// The primary restarts empty at a new address and re-enters the
/// membership; the proxy still reads it dead.
async fn restart(r: &mut Ringing) {
    let pri_ord = r.pri_ord.clone();
    let (primary, survivor) = split_mut(&pri_ord, &mut r.w_b1, &mut r.w_b2);
    r.fh.mark(&pri_ord, None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = primary.reboot().await;
    r.proxy.set_address(&pri_ord, new_addr);
    r.fh.note_worker_rebound(&pri_ord, new_addr);
    survivor.simulate_peer_added(&pri_ord);
}

/// The restarted primary recovers and becomes ready.
async fn ready(r: &mut Ringing) {
    let ready =
        r.fh.pump_until(Duration::from_millis(10), Duration::from_secs(60), async || {
            r.primary().is_ready()
        })
        .await;
    assert!(ready, "the restarted primary became ready");
}

/// The primary restarts, recovers, reclaims the call and is routable.
async fn primary_returns(r: &mut Ringing) {
    restart(r).await;
    ready(r).await;
    r.proxy.set_health(&r.pri_ord, WorkerHealth::Alive);
}

/// The copy stands down on the primary's write, an INVITE it dialled awaiting
/// its final or not; the primary serves the call.
async fn copy_stands_down(r: &Ringing) {
    let stood_down =
        r.fh.pump_until(Duration::from_millis(100), Duration::from_secs(5), async || {
            !r.copy().serves(&r.call_ref)
        })
        .await;
    assert!(stood_down, "the copy stands down on the primary's write");
    assert!(r.primary().serves(&r.call_ref), "the returned primary serves the call");
}

/// One message a worker sent the proxy.
struct Sent {
    port: u16,
    line: String,
    call_id: String,
    method: String,
    body: Vec<u8>,
    at_ms: u64,
}

/// Every message a worker sent the proxy, in wire order.
fn worker_sends(fh: &FailoverHarness, proxy: SocketAddr) -> Vec<Sent> {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.to == proxy && [5091, 5092].contains(&e.from.port()))
        .filter_map(|e| {
            let (call_id, method, body) = match parser.parse(&e.raw).ok()? {
                SipMessage::Request(r) => {
                    (r.call_id().to_string(), r.cseq().method().to_string(), r.body().to_vec())
                }
                SipMessage::Response(r) => {
                    (r.call_id().to_string(), r.cseq().method().to_string(), r.body().to_vec())
                }
            };
            Some(Sent {
                port: e.from.port(),
                line: e.start_line(),
                call_id,
                method,
                body,
                at_ms: e.sent_ms,
            })
        })
        .collect()
}

/// The ports of the workers that sent a message opening with `prefix` on
/// `call_id` (`None`: any) for CSeq `method`, each once in wire order: a
/// message repeated counts once until another worker sends one.
fn senders(r: &Ringing, prefix: &str, call_id: Option<&str>, method: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = worker_sends(&r.fh, r.proxy.addr())
        .into_iter()
        .filter(|s| s.line.starts_with(prefix) && s.method == method)
        .filter(|s| call_id.is_none_or(|c| s.call_id == c))
        .map(|s| s.port)
        .collect();
    ports.dedup();
    ports
}

/// The caller's Call-ID, the a-leg's.
fn caller_call_id(r: &Ringing) -> String {
    r.call.call_id()
}

/// How many 200 responses to an INVITE the proxy delivered to `worker`'s port.
fn invite_200s_delivered_to(r: &Ringing, worker_port: u16) -> usize {
    let proxy = r.proxy.addr();
    let parser = CustomParser::new();
    r.fh.sip_entries()
        .into_iter()
        .filter(|e| e.from == proxy && e.to.port() == worker_port)
        .filter(|e| match parser.parse(&e.raw) {
            Ok(SipMessage::Response(resp)) => {
                resp.status() == 200 && resp.cseq().method().as_str() == "INVITE"
            }
            _ => false,
        })
        .count()
}

/// How many `status` responses to a `method` request the proxy delivered to
/// `worker_port`.
fn responses_delivered_to(r: &Ringing, worker_port: u16, status: u16, method: &str) -> usize {
    let proxy = r.proxy.addr();
    let parser = CustomParser::new();
    r.fh.sip_entries()
        .into_iter()
        .filter(|e| e.from == proxy && e.to.port() == worker_port)
        .filter(|e| match parser.parse(&e.raw) {
            Ok(SipMessage::Response(resp)) => {
                resp.status() == status && resp.cseq().method().as_str() == method
            }
            _ => false,
        })
        .count()
}

/// The most copies of one 2xx the RFC 3261 §13.3.1.4 ladder sends from
/// `first_ms` through `last_ms`: the first, then one at each interval,
/// doubling from T1 up to T2.
fn ladder_bound(first_ms: u64, last_ms: u64) -> usize {
    let (mut at, mut interval, mut copies) = (first_ms, T1_MS, 1);
    loop {
        at += interval;
        if at > last_ms {
            return copies;
        }
        copies += 1;
        interval = (interval * 2).min(T2_MS);
    }
}

/// The callee's answer reached the primary alone, and the caller heard it
/// from the primary alone; the callee's 2xx is ACKed by the primary.
fn answered_by_the_primary_alone(r: &Ringing) {
    let caller = caller_call_id(r);
    assert_eq!(
        invite_200s_delivered_to(r, r.copy_port()),
        0,
        "no 200 to the INVITE reaches the copy: the callee's cookie names the primary"
    );
    assert_eq!(
        senders(r, "SIP/2.0 200", Some(&caller), "INVITE"),
        [r.primary_port()],
        "the caller's 200 is sent by the primary alone"
    );
    assert_eq!(
        senders(r, "ACK sip:bob@", None, "ACK"),
        [r.primary_port()],
        "the primary ACKs the callee's 2xx"
    );
}

/// The callee answers; the caller takes the 200 and ACKs it; the callee
/// takes the ACK. Returns both parties' dialogs.
async fn answer(r: &mut Ringing) -> (Dialog, Dialog) {
    r.at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    r.call.expect(200).await;
    let dialog = r.call.ack().await;
    r.bob.receive("ACK").await;
    (dialog, r.at_bob.dialog())
}

/// The cluster settles to the call's one CDR, every party quiet, nothing of
/// the call left on either node, and the trace RFC clean.
async fn settle(r: Ringing, name: &str) {
    r.fh.advance(Duration::from_secs(40)).await;
    let peers = [&r.alice, &r.bob, &r.charlie];
    settle_to_one_cdr(&r.fh, &peers, &[&r.w_b1, &r.w_b2], &r.call_ref).await;
    r.fh.assert_sip_rfc_clean(name);
    drop(r.proxy);
}

/// The callee's BYE reaches the primary, which BYEs the caller.
async fn callee_hangs_up(r: &Ringing, bob_dialog: &mut Dialog) {
    let mut bye = bob_dialog.bye().await;
    r.alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    assert_eq!(
        senders(r, "BYE sip:alice@", None, "BYE"),
        [r.primary_port()],
        "the callee's BYE reaches the primary, which relays it"
    );
}

/// The callee answers after the reclaim write applied at the backup, and
/// hangs up: every message of the call reaches the primary.
#[tokio::test(start_paused = true)]
async fn a_copys_dialled_invite_answered_after_the_reclaim_is_answered_by_the_primary() {
    let name = "copys-dialled-invite-answered-after-the-reclaim";
    let mut r = ringing_at_the_copy(name).await;
    primary_returns(&mut r).await;
    copy_stands_down(&r).await;

    let (_dialog, mut bob_dialog) = answer(&mut r).await;
    answered_by_the_primary_alone(&r);
    callee_hangs_up(&r, &mut bob_dialog).await;
    assert_eq!(r.consults.load(Ordering::SeqCst), 2, "the reclaim asks no decision");
    settle(r, name).await;
}

/// The same, the caller then renegotiates before the callee hangs up: the
/// caller's re-INVITE reaches the primary, which relays it to the callee.
#[tokio::test(start_paused = true)]
async fn a_copys_dialled_invite_answered_after_the_reclaim_renegotiates_through_the_primary() {
    let name = "copys-dialled-invite-renegotiated-after-the-reclaim";
    let mut r = ringing_at_the_copy(name).await;
    primary_returns(&mut r).await;
    copy_stands_down(&r).await;

    let (mut dialog, mut bob_dialog) = answer(&mut r).await;
    answered_by_the_primary_alone(&r);
    let mut round = dialog.send_request(InDialogMethod::Invite).with_sdp(REOFFER).send().await;
    let mut at_bob = r.bob.receive("INVITE").await;
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    let answered = round.expect(200).await;
    dialog.ack_for(answered.cseq().seq(), None).await;
    r.bob.receive_ack_of(at_bob.request().cseq().seq()).await;
    let relayed: Vec<u16> = worker_sends(&r.fh, r.proxy.addr())
        .into_iter()
        .filter(|s| s.line.starts_with("INVITE sip:bob@") && s.body == REOFFER.as_bytes())
        .map(|s| s.port)
        .collect();
    assert!(
        !relayed.is_empty() && relayed.iter().all(|&p| p == r.primary_port()),
        "the caller's re-INVITE is relayed by the primary alone: {relayed:?}"
    );
    callee_hangs_up(&r, &mut bob_dialog).await;
    settle(r, name).await;
}

/// The same, the callee then transfers the caller to a target that refuses:
/// the callee's REFER reaches the primary, which dials the target, ACKs its
/// 480 and tells the callee; the call lives on to the caller's BYE.
#[tokio::test(start_paused = true)]
async fn a_copys_dialled_invite_answered_after_the_reclaim_transfers_through_the_primary() {
    let name = "copys-dialled-invite-refer-after-the-reclaim";
    let mut r = ringing_at_the_copy(name).await;
    primary_returns(&mut r).await;
    copy_stands_down(&r).await;

    let (mut dialog, mut bob_dialog) = answer(&mut r).await;
    answered_by_the_primary_alone(&r);
    refer_to_charlie(&mut bob_dialog).await;
    r.bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut at_charlie = r.charlie.receive("INVITE").await;
    at_charlie.respond(480, "Temporarily Unavailable").await;
    r.charlie.receive("ACK").await;
    r.bob.receive("NOTIFY").await.respond(200, "OK").await;
    assert_eq!(
        senders(&r, "INVITE sip:charlie@", None, "INVITE"),
        [r.primary_port()],
        "the primary dials the transfer target"
    );
    assert_eq!(
        senders(&r, "ACK sip:charlie@", None, "ACK"),
        [r.primary_port()],
        "the primary ACKs the target's refusal"
    );
    let mut bye = dialog.bye().await;
    r.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle(r, name).await;
}

/// The callee's REFER to charlie, accepted.
async fn refer_to_charlie(bob_dialog: &mut Dialog) {
    let api = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{}}}}}"#,
        CHARLIE.rsplit(':').next().unwrap()
    );
    bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@{CHARLIE}>"))
        .with_header("X-Api-Call", &api)
        .send()
        .await
        .expect(202)
        .await;
}

/// The callee answers while the returned primary is not routable: recovered
/// and ready itself, its endpoint not, it holds the call parked, and the
/// proxy reads it dead, so the 2xx reaches the copy, which answers the
/// caller. The caller's ACK is late. The primary meets the copy's answered
/// write and turns routable. One node sends the caller's 2xx at a time, every
/// copy on one RFC 3261 §13.3.1.4 ladder from the first, never an extra
/// immediate send at the meeting or at the arming; the primary owns the call
/// afterwards.
#[tokio::test(start_paused = true)]
async fn a_copys_answer_before_the_primary_is_routable_is_met_without_an_extra_2xx() {
    let name = "copys-dialled-invite-answered-before-routable";
    let mut r = ringing_at_the_copy(name).await;
    let pri = r.pri_ord.clone();
    restart(&mut r).await;
    r.fh.flap_not_ready(&pri);
    ready(&mut r).await;
    r.fh.advance(Duration::from_millis(500)).await;
    assert!(r.primary().parks(&r.call_ref), "the returned primary parks the call, unarmed");

    r.at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    r.call.expect(200).await;
    let caller = caller_call_id(&r);
    assert_eq!(
        senders(&r, "SIP/2.0 200", Some(&caller), "INVITE"),
        [r.copy_port()],
        "the copy answers the caller while the primary is not routable"
    );

    // The caller's ACK is held while the primary meets the copy's answer and
    // turns routable. The test relies on the paused runtime draining every
    // task an advance wakes, in order, before the advance returns: the copy's
    // answered write reaches the primary inside this advance, and the arming
    // and the meeting inside the next.
    r.fh.advance(Duration::from_secs(1)).await;
    r.fh.flap_ready(&pri);
    r.proxy.set_health(&pri, WorkerHealth::Alive);
    r.fh.advance(Duration::from_secs(2)).await;
    let mut dialog = r.call.ack().await;
    r.bob.receive("ACK").await;
    let acked_at = r.fh.now_ms() as u64;

    let twos: Vec<Sent> = worker_sends(&r.fh, r.proxy.addr())
        .into_iter()
        .filter(|s| s.line.starts_with("SIP/2.0 200") && s.method == "INVITE")
        .filter(|s| s.call_id == caller && s.at_ms <= acked_at)
        .collect();
    let first = twos.first().expect("the caller's 200").at_ms;
    let wire: Vec<(u16, u64)> = twos.iter().map(|s| (s.port, s.at_ms)).collect();
    let mut by_sender: Vec<u16> = twos.iter().map(|s| s.port).collect();
    by_sender.dedup();
    assert!(by_sender.len() <= 2, "one 2xx sender at a time, never interleaved: {wire:?}");
    assert!(
        twos.len() <= ladder_bound(first, acked_at),
        "the caller's 2xx copies stay on one ladder from {first} to {acked_at} ms: {wire:?}"
    );
    copy_stands_down(&r).await;
    let mut bye = dialog.bye().await;
    r.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    assert_eq!(
        senders(&r, "BYE sip:bob@", None, "BYE"),
        [r.primary_port()],
        "the primary owns the call afterwards"
    );
    settle(r, name).await;
}

/// How late the restarted primary's writes reach the backup: past the
/// instant the primary is routable and the callee answers.
const RECLAIM_LAG: Duration = Duration::from_millis(1_000);

/// The callee answers once the primary is routable but before its reclaim
/// write is applied at the backup: the 2xx reaches the primary, whose parked
/// call it arms; the copy stands down when the write lands. The caller gets
/// its 200 from the primary alone.
#[tokio::test(start_paused = true)]
async fn a_copys_dialled_invite_answered_inside_the_reclaim_lag_is_answered_by_the_primary() {
    let name = "copys-dialled-invite-answered-inside-the-reclaim-lag";
    let mut r = ringing_at_the_copy(name).await;
    let (pri, bak) = (r.pri_ord.clone(), if r.pri_ord == "b1" { "b2" } else { "b1" });
    r.fh.delay(&pri, bak, RECLAIM_LAG.as_millis() as u64);
    primary_returns(&mut r).await;
    assert!(r.copy().serves(&r.call_ref), "the reclaim write has not reached the copy yet");

    // The paused runtime drains the 2xx's turn at the primary, and the parked
    // call's arming before it, before `answer` returns; the delayed reclaim
    // write is still on the link to the copy.
    let (_dialog, mut bob_dialog) = answer(&mut r).await;
    answered_by_the_primary_alone(&r);
    r.fh.heal(&pri, bak);
    copy_stands_down(&r).await;
    callee_hangs_up(&r, &mut bob_dialog).await;
    settle(r, name).await;
}

/// The call established, its primary dies and the callee's REFER reaches the
/// backup, whose copy takes the call over and dials the transfer target, who
/// rings. The copy's writes recorded, the primary returns.
async fn copy_dials_the_transfer_target(name: &str) -> (Ringing, Dialog, ServerTxn) {
    let mut r = ringing_at_the_primary(name).await;
    let (dialog, mut bob_dialog) = answer(&mut r).await;
    r.fh.advance(Duration::from_secs(1)).await;

    let pri_ord = r.pri_ord.clone();
    let (primary, survivor) = split_mut(&pri_ord, &mut r.w_b1, &mut r.w_b2);
    kill_announced(&mut r.fh, primary, survivor, &pri_ord, &r.proxy).await;
    refer_to_charlie(&mut bob_dialog).await;
    r.bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut at_charlie = r.charlie.receive("INVITE").await;
    at_charlie.respond(180, "Ringing").await;
    assert_eq!(
        senders(&r, "INVITE sip:charlie@", None, "INVITE"),
        [r.copy_port()],
        "the copy dials the transfer target"
    );
    r.fh.advance(Duration::from_millis(500)).await;

    primary_returns(&mut r).await;
    copy_stands_down(&r).await;
    (r, dialog, at_charlie)
}

/// The primary returns while the transfer target the copy dialled rings and
/// owns that INVITE: the caller hangs up, and the primary CANCELs the target,
/// ACKs its 487 and ends the callee's dialog.
#[tokio::test(start_paused = true)]
async fn a_transfer_target_the_copy_dialled_is_cancelled_by_the_returned_primary() {
    let name = "copys-dialled-transfer-target-cancelled-by-the-primary";
    let (r, mut dialog, mut at_charlie) = copy_dials_the_transfer_target(name).await;
    let mut bye = dialog.bye().await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    r.charlie.receive("CANCEL").await.respond(200, "OK").await;
    at_charlie.respond(487, "Request Terminated").await;
    r.charlie.receive("ACK").await;
    r.bob.receive_tolerating("BYE", &["NOTIFY"]).await.respond(200, "OK").await;
    assert_eq!(
        senders(&r, "CANCEL sip:charlie@", None, "CANCEL"),
        [r.primary_port()],
        "the returned primary CANCELs the target the copy dialled"
    );
    assert_eq!(
        senders(&r, "ACK sip:charlie@", None, "ACK"),
        [r.primary_port()],
        "the returned primary ACKs the target's 487"
    );
    assert_eq!(
        (responses_delivered_to(&r, r.primary_port(), 200, "CANCEL"), responses_delivered_to(&r, r.copy_port(), 200, "CANCEL")),
        (1, 0),
        "the CANCEL's 200 follows its Via's received and rport (RFC 3581) to the primary that sent it"
    );
    settle(r, name).await;
}

/// The same, the target answers after the primary returned: its 2xx reaches
/// the primary alone, which ACKs it, realigns the target and the caller, and
/// tells the callee; the caller then hangs up.
#[tokio::test(start_paused = true)]
async fn a_transfer_target_the_copy_dialled_answers_the_returned_primary() {
    let name = "copys-dialled-transfer-target-answers-the-primary";
    let (r, mut dialog, mut at_charlie) = copy_dials_the_transfer_target(name).await;
    at_charlie.respond(200, "OK").with_sdp(ANSWER).await;
    r.charlie.receive("ACK").await;
    assert_eq!(
        invite_200s_delivered_to(&r, r.copy_port()),
        0,
        "the target's 200 reaches the primary alone"
    );
    assert_eq!(
        senders(&r, "ACK sip:charlie@", None, "ACK"),
        [r.primary_port()],
        "the primary ACKs the target's 2xx"
    );
    let mut c_realign = r.charlie.receive("INVITE").await;
    c_realign.respond(200, "OK").with_sdp(ANSWER).await;
    r.charlie.receive("ACK").await;
    let mut a_realign = r.alice.receive("INVITE").await;
    a_realign.respond(200, "OK").with_sdp(OFFER).await;
    r.alice.receive("ACK").await;
    r.bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut bye = dialog.bye().await;
    r.charlie.receive("BYE").await.respond(200, "OK").await;
    r.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle(r, name).await;
}
