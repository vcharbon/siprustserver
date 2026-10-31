//! **A leg the copy dials while its live primary is misread dead.** The call
//! is established at its primary; the proxy misreads the primary dead while it
//! runs. The callee's REFER, routed to the backup, takes the call over there,
//! and the copy dials the transfer target. The primary, which never stopped,
//! meets that dial and takes its INVITE over (ADR-0047 D7); the leg's cookie
//! names the primary (ADR-0055 §8).
//!
//! - The probe reads the primary alive again while the target rings: the
//!   target's answer reaches the primary alone, which ACKs it and completes
//!   the transfer: one owner, one CDR.
//! - The target answers during the misread: its 2xx reaches the backup, which
//!   takes the call over again, ACKs it and completes the transfer; the
//!   primary meets that answer and holds no INVITE of the dial open, so the
//!   call lives on past that INVITE's bound to the caller's BYE.
//!
//! Beyond the failover sweep: a misread primary, which no sweep fault makes.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::{default_call_refer, route_to_processing_refer};
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use failover_harness::{FailoverHarness, ProxySut, ReplicatedB2buaSut, WorkerHealth};
use scenario_harness::agent::{Agent, Dialog};
use scenario_harness::ServerTxn;
use sip_message::generators::InDialogMethod;

use crate::departure_support::{bring_up_deciding, placed, CountedDecision};
use crate::inflight_resend::settle_to_one_cdr;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const CHARLIE: &str = "127.0.0.1:5090";
/// The longest an INVITE client transaction lives (`invite_txn_timeout_sec`'s
/// default): a seed of one is bounded by it from the seed.
const INVITE_BOUND: Duration = Duration::from_secs(158);

/// The SIP port of a worker, every incarnation alike.
fn port_of(ordinal: &str) -> u16 {
    if ordinal == "b1" {
        5091
    } else {
        5092
    }
}

fn routing_engine() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| NewCallResponse::Route(route_to_processing_refer("127.0.0.1", 5070)))
            .on_refer(default_call_refer)
            .build(),
    )
}

/// The established call, its primary misread dead, the transfer target the
/// copy dialled ringing.
struct Misread {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    charlie: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
    pri_ord: String,
    call_ref: String,
    dialog: Dialog,
    at_charlie: ServerTxn,
}

impl Misread {
    fn primary_port(&self) -> u16 {
        port_of(&self.pri_ord)
    }

    fn copy_port(&self) -> u16 {
        port_of(if self.pri_ord == "b1" { "b2" } else { "b1" })
    }

    /// The ports of the workers that sent a message opening with `prefix`, each
    /// once in wire order, and the instant of each.
    fn sends(&self, prefix: &str) -> Vec<(u16, u64)> {
        let proxy = self.proxy.addr();
        self.fh
            .sip_entries()
            .into_iter()
            .filter(|e| e.to == proxy && [5091, 5092].contains(&e.from.port()))
            .filter(|e| e.start_line().starts_with(prefix))
            .map(|e| (e.from.port(), e.sent_ms))
            .collect()
    }

    fn senders(&self, prefix: &str) -> Vec<u16> {
        let mut ports: Vec<u16> = self.sends(prefix).into_iter().map(|(p, _)| p).collect();
        ports.dedup();
        ports
    }
}

async fn transfer_target_rings_during_the_misread(name: &str) -> Misread {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let charlie = fh.agent("charlie", CHARLIE).await.transfer_target();
    let (decision, _consults) = CountedDecision::counting(routing_engine(), Duration::ZERO);
    let (proxy, w_b1, w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = at_bob.dialog();
    fh.advance(Duration::from_secs(1)).await;

    fh.mark("proxy", None, "misread", "the probe reads the live primary dead");
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;
    let api = r#"{"refer_key":"refer-allow-c","destination":{"host":"127.0.0.1","port":5090}}"#;
    bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@{CHARLIE}>"))
        .with_header("X-Api-Call", api)
        .send()
        .await
        .expect(202)
        .await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut at_charlie = charlie.receive_absorbing("INVITE", &["INVITE"]).await;
    at_charlie.respond(180, "Ringing").await;
    fh.advance(Duration::from_millis(500)).await;
    let m = Misread {
        fh,
        alice,
        bob,
        charlie,
        proxy,
        w_b1,
        w_b2,
        pri_ord,
        call_ref,
        dialog,
        at_charlie,
    };
    assert!(
        m.senders("INVITE sip:charlie@").contains(&m.copy_port()),
        "the copy dials the transfer target"
    );
    m
}

/// The target's 2xx taken, the parties are realigned and the callee told.
async fn realigned(m: &Misread) {
    m.charlie.receive("ACK").await;
    let mut c_realign = m.charlie.receive("INVITE").await;
    c_realign.respond(200, "OK").with_sdp(ANSWER).await;
    m.charlie.receive("ACK").await;
    let mut a_realign = m.alice.receive("INVITE").await;
    a_realign.respond(200, "OK").with_sdp(OFFER).await;
    m.alice.receive("ACK").await;
    m.bob.receive("NOTIFY").await.respond(200, "OK").await;
}

/// The caller hangs up; the cluster settles to the call's one CDR.
async fn hang_up_and_settle(mut m: Misread, name: &str) {
    let mut bye = m.dialog.bye().await;
    // A node that takes the call over sends again what its record says
    // left (RFC 3261 §17.1.2.2, §17.2.2): the parties absorb those copies.
    m.charlie.receive_tolerating("BYE", &["ACK", "INVITE"]).await.respond(200, "OK").await;
    m.bob.receive_tolerating("BYE", &["NOTIFY"]).await.respond(200, "OK").await;
    bye.expect(200).await;
    m.fh.advance(Duration::from_secs(40)).await;
    let peers = [&m.alice, &m.bob, &m.charlie];
    settle_to_one_cdr(&m.fh, &peers, &[&m.w_b1, &m.w_b2], &m.call_ref).await;
    m.fh.assert_sip_rfc_clean(name);
    drop(m.proxy);
}

#[tokio::test(start_paused = true)]
async fn a_dial_made_during_the_misread_is_answered_at_the_primary_after_the_flip() {
    let name = "copys-dial-during-the-misread-answered-after-the-flip";
    let mut m = transfer_target_rings_during_the_misread(name).await;
    m.fh.mark("proxy", None, "flip", "the probe reads the primary alive again");
    m.proxy.set_health(&m.pri_ord, WorkerHealth::Alive);
    m.fh.advance(Duration::from_millis(500)).await;
    let answered_at = m.fh.now_ms() as u64;
    m.at_charlie.respond(200, "OK").with_sdp(ANSWER).await;
    realigned(&m).await;
    let acks: Vec<u16> = m
        .sends("ACK sip:charlie@")
        .into_iter()
        .filter(|(_, at)| *at >= answered_at)
        .map(|(p, _)| p)
        .collect();
    assert!(
        !acks.is_empty() && acks.iter().all(|p| *p == m.primary_port()),
        "the primary ACKs the target's 2xx: {acks:?}"
    );
    assert!(m.senders("BYE").is_empty(), "no node ends the call on evidence");
    hang_up_and_settle(m, name).await;
}

#[tokio::test(start_paused = true)]
async fn a_dial_answered_during_the_misread_leaves_no_invite_open_at_the_primary() {
    let name = "copys-dial-answered-during-the-misread";
    let mut m = transfer_target_rings_during_the_misread(name).await;
    m.at_charlie.respond(200, "OK").with_sdp(ANSWER).await;
    realigned(&m).await;
    let acked_at = m.fh.now_ms() as u64;
    m.fh.mark("proxy", None, "flip", "the probe reads the primary alive again");
    m.proxy.set_health(&m.pri_ord, WorkerHealth::Alive);
    m.fh.advance(INVITE_BOUND + Duration::from_secs(10)).await;
    let late: Vec<u64> = m
        .sends("INVITE sip:charlie@")
        .into_iter()
        .filter(|(p, at)| *p == m.primary_port() && *at > acked_at)
        .map(|(_, at)| at)
        .collect();
    assert!(late.is_empty(), "no INVITE of the dial leaves the primary after its ACK: {late:?}");
    assert!(m.senders("BYE").is_empty(), "the call lives on past the INVITE's bound");
    hang_up_and_settle(m, name).await;
}
