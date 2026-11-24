//! A request that names no call here, every lookup having answered, is
//! answered as a stateless UAS answers it (RFC 3261 §8.2.7) and leaves no
//! state behind: no server transaction outlives the answer, so a flood of such
//! requests holds no Timer J and counts nothing against new-call admission. A
//! copy of the request finds no transaction, reaches the router again and
//! draws the same answer under the same To-tag.
//!
//! Each shape's state is read about 1 s after its request left, long before
//! Timer J (64·T1 = 32 s) or Timer I (T4 = 5 s) would end a transaction. The
//! answers are read on the recorded wire: a UA's stack absorbs a repeated
//! final.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua::config::Ceilings;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::{Agent, Harness};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipResponse};

/// Long enough for the answer to come back, far short of Timer I and Timer J.
const BEFORE_ANY_TIMER: Duration = Duration::from_millis(1000);
/// The answer's round trip (two transits of 100 ms), short of Timer G's first
/// re-send of an INVITE's final (T1 = 500 ms, RFC 3261 §17.2.1).
const BEFORE_TIMER_G: Duration = Duration::from_millis(300);

/// The caller's offer in the late-PRACK call.
const LATE_PRACK_OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// A request written by hand: the ones a stack keeps for a dialog it holds
/// would never be sent for one it does not.
struct Stray {
    method: &'static str,
    ruri: String,
    /// The token the request's Call-ID, From-tag and branch derive from: one
    /// per request, so no two share a transaction or a Call-ID.
    call: String,
    to_tag: Option<&'static str>,
    /// Header lines the method requires (RFC 3262 §7.1, RFC 3515 §2.4.1,
    /// RFC 6665 §8.1.1).
    extra: &'static [&'static str],
}

impl Stray {
    /// A request of `method`, CSeq 1, on the Call-ID `call` names, to the
    /// B2BUA's bare address: its Request-URI carries no `callRef`.
    fn new(method: &'static str, b2bua: SocketAddr, call: &str) -> Self {
        Stray {
            method,
            ruri: format!("sip:bob@{b2bua}"),
            call: call.to_string(),
            to_tag: None,
            extra: &[],
        }
    }

    fn tagged(mut self, tag: &'static str) -> Self {
        self.to_tag = Some(tag);
        self
    }

    fn with(mut self, extra: &'static [&'static str]) -> Self {
        self.extra = extra;
        self
    }

    fn ruri(mut self, ruri: String) -> Self {
        self.ruri = ruri;
        self
    }

    /// The datagram `from` sends.
    fn wire(&self, from: SocketAddr) -> Vec<u8> {
        let to_tag = self.to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
        let extra: String = self.extra.iter().map(|line| format!("{line}\r\n")).collect();
        format!(
            "{method} {ruri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {from};branch=z9hG4bK-{call}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@192.0.2.10>;tag=a-{call}\r\n\
             To: <sip:bob@192.0.2.20>{to_tag}\r\n\
             Call-ID: {call}@192.0.2.10\r\n\
             CSeq: 1 {method}\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n",
            method = self.method,
            ruri = self.ruri,
            call = self.call,
        )
        .into_bytes()
    }

    /// The ACK of this INVITE's non-2xx final `answer` (§17.1.1.3): its
    /// Request-URI, branch, Call-ID, From and CSeq number, the final's To.
    fn ack_of(&self, from: SocketAddr, answer: &SipResponse) -> Vec<u8> {
        let to_tag = answer.to().tag().expect("a final carries a To-tag");
        format!(
            "ACK {ruri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {from};branch=z9hG4bK-{call}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@192.0.2.10>;tag=a-{call}\r\n\
             To: <sip:bob@192.0.2.20>;tag={to_tag}\r\n\
             Call-ID: {call}@192.0.2.10\r\n\
             CSeq: 1 ACK\r\n\
             Content-Length: 0\r\n\r\n",
            ruri = self.ruri,
            call = self.call,
        )
        .into_bytes()
    }
}

/// `alice` sends `stray` to `b2bua` and reads, after [`BEFORE_ANY_TIMER`],
/// every response `b2bua` sent her on the recorded wire since, whose CSeq
/// names its method: the wire, not her queue, so a repeated final her stack
/// absorbs is counted too.
async fn send(h: &Harness, alice: &Agent, b2bua: SocketAddr, stray: &Stray) -> Vec<SipResponse> {
    let mark = h.wire_entries().len();
    alice.try_send_datagram(&stray.wire(alice.addr()), b2bua).await.expect("the request leaves");
    h.advance(BEFORE_ANY_TIMER).await;
    alice.drain().await;
    answers_since(h, alice, b2bua, mark, stray.method)
}

/// The responses `b2bua` sent `alice` on the recorded wire past entry `mark`
/// whose CSeq names `method`.
fn answers_since(
    h: &Harness,
    alice: &Agent,
    b2bua: SocketAddr,
    mark: usize,
    method: &str,
) -> Vec<SipResponse> {
    h.wire_entries()
        .into_iter()
        .skip(mark)
        .filter(|e| e.from == b2bua && e.to == alice.addr())
        .filter_map(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Response(r)) if r.cseq().method().as_str() == method => Some(r),
            _ => None,
        })
        .collect()
}

fn statuses(responses: &[SipResponse]) -> Vec<u16> {
    responses.iter().map(SipResponse::status).collect()
}

fn to_tag(response: &SipResponse) -> String {
    response.to().tag().expect("a final carries a To-tag").to_string()
}

/// The server transactions live in `b2bua`: what new-call admission counts.
fn live_transactions(b2bua: &B2buaSut) -> usize {
    b2bua.txn_metrics().active_transactions()
}

/// A tagged BYE naming no dialog draws its 481, and no server transaction is
/// left once it left: with a transaction ceiling of 1, the next new call is
/// admitted and set up. Its copy, sent before Timer J, draws the same 481
/// under the same To-tag (§8.2.7) and is counted again.
#[tokio::test(start_paused = true)]
async fn a_bye_naming_no_dialog_leaves_no_transaction_and_counts_against_no_new_call() {
    let s = B2buaScene::with_b2bua("b2bua-stateless-no-call-bye", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.capacity.transactions = Ceilings { normal: Some(1), emergency: None };
        })
    })
    .await;
    s.h.allow_violation("mid-dialog-tags", "a BYE in a dialog no party holds is under test");
    assert_eq!(live_transactions(&s.b2bua), 0, "no transaction before the BYE");

    let bye = Stray::new("BYE", s.b2bua.addr, "no-dialog-bye").tagged("b-gone");
    let first = send(&s.h, &s.alice, s.b2bua.addr, &bye).await;
    assert_eq!(statuses(&first), vec![481], "RFC 3261 §12.2.2, §15.1.2");
    assert_eq!(to_tag(&first[0]), "b-gone", "the 481 echoes the To-tag");
    assert_eq!(
        live_transactions(&s.b2bua),
        0,
        "the 481 left statelessly (§8.2.7): no transaction holds Timer J"
    );

    let copy = send(&s.h, &s.alice, s.b2bua.addr, &bye).await;
    assert_eq!(statuses(&copy), vec![481], "the copy draws the same answer");
    assert_eq!(to_tag(&copy[0]), "b-gone");
    assert_eq!(
        s.b2bua.metrics().unroutable_refused_of("BYE", 481),
        2,
        "no transaction absorbed the copy: it reached the router and is counted again"
    );
    assert_eq!(live_transactions(&s.b2bua), 0, "the copy left nothing either");

    let mut dialog = s.establish().await;
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    let _ = s.finish().await;
}

/// A tagged request of every non-INVITE method a dialog carries, naming no
/// dialog, draws 481 (§12.2.2) and leaves no transaction.
#[tokio::test(start_paused = true)]
async fn a_tagged_request_of_any_method_naming_no_dialog_leaves_no_transaction() {
    let s = B2buaScene::new("b2bua-stateless-no-call-tagged").await;
    s.h.allow_violation("mid-dialog-tags", "requests in a dialog no party holds are under test");
    s.h.allow_violation(
        "no-to-tag-on-initial-request",
        "a tagged request in a dialog no party holds is under test",
    );
    let shapes: [(&'static str, &'static [&'static str]); 6] = [
        ("INFO", &[]),
        ("UPDATE", &["Contact: <sip:alice@192.0.2.10>"]),
        (
            "NOTIFY",
            &["Contact: <sip:alice@192.0.2.10>", "Event: refer", "Subscription-State: active"],
        ),
        ("MESSAGE", &[]),
        ("REFER", &["Contact: <sip:alice@192.0.2.10>", "Refer-To: <sip:carol@192.0.2.30>"]),
        ("PRACK", &["RAck: 1 1 INVITE"]),
    ];

    for (method, extra) in shapes {
        let call = format!("no-dialog-{}", method.to_ascii_lowercase());
        let stray = Stray::new(method, s.b2bua.addr, &call).tagged("b-gone").with(extra);
        let answer = send(&s.h, &s.alice, s.b2bua.addr, &stray).await;
        assert_eq!(statuses(&answer), vec![481], "{method}: RFC 3261 §12.2.2");
        assert_eq!(
            live_transactions(&s.b2bua),
            0,
            "{method}: the 481 left statelessly (§8.2.7): no transaction holds Timer J"
        );
    }

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// A CANCEL matching no INVITE draws 481 (§9.2) and leaves no transaction; its
/// copy draws the same 481 under the same To-tag.
#[tokio::test(start_paused = true)]
async fn a_cancel_matching_no_invite_leaves_no_transaction() {
    let s = B2buaScene::new("b2bua-stateless-no-call-cancel").await;
    let cancel = Stray::new("CANCEL", s.b2bua.addr, "no-invite-cancel");

    let first = send(&s.h, &s.alice, s.b2bua.addr, &cancel).await;
    assert_eq!(statuses(&first), vec![481], "RFC 3261 §9.2");
    assert_eq!(
        live_transactions(&s.b2bua),
        0,
        "the 481 left statelessly (§8.2.7): no transaction holds Timer J"
    );

    let copy = send(&s.h, &s.alice, s.b2bua.addr, &cancel).await;
    assert_eq!(statuses(&copy), vec![481]);
    assert_eq!(to_tag(&copy[0]), to_tag(&first[0]), "the copy draws the same To-tag (§8.2.7)");
    assert_eq!(live_transactions(&s.b2bua), 0);

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// An untagged request other than OPTIONS naming no call draws its answer, 481
/// for a BYE (§15.1.2), 405 for a method outside the node's `Allow` (§8.2.1),
/// under a To-tag this node chose, and leaves no transaction. Each copy draws
/// the same answer under the same To-tag: the tag derives from the request
/// (§8.2.7).
#[tokio::test(start_paused = true)]
async fn an_untagged_request_naming_no_call_leaves_no_transaction() {
    let s = B2buaScene::new("b2bua-stateless-no-call-untagged").await;
    s.h.allow_violation(
        "no-bye-outside-or-early-dialog",
        "a BYE naming no dialog is the deviation under test (RFC 3261 §15.1.2)",
    );

    for (method, code) in [("BYE", 481), ("MESSAGE", 405)] {
        let call = format!("no-call-untagged-{}", method.to_ascii_lowercase());
        let stray = Stray::new(method, s.b2bua.addr, &call);
        let first = send(&s.h, &s.alice, s.b2bua.addr, &stray).await;
        assert_eq!(statuses(&first), vec![code], "{method}");
        assert_eq!(
            live_transactions(&s.b2bua),
            0,
            "{method}: the {code} left statelessly (§8.2.7): no transaction holds Timer J"
        );

        let copy = send(&s.h, &s.alice, s.b2bua.addr, &stray).await;
        assert_eq!(statuses(&copy), vec![code], "{method}: the copy draws the same answer");
        assert_eq!(to_tag(&copy[0]), to_tag(&first[0]), "{method}: the same To-tag (§8.2.7)");
        assert_eq!(live_transactions(&s.b2bua), 0, "{method}: the copy left nothing either");
    }

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// A BYE whose Request-URI `callRef` names no live call takes the per-call
/// path: its 481 leaves statelessly too, and the path leaves no transaction
/// and no call behind.
#[tokio::test(start_paused = true)]
async fn a_bye_whose_call_ref_names_no_call_leaves_no_transaction() {
    let s = B2buaScene::new("b2bua-stateless-no-call-keyed").await;
    s.h.allow_violation("mid-dialog-tags", "a BYE in a dialog no party holds is under test");
    let keyed = format!("sip:bob@{};callRef=w0%7cnever%7cminted", s.b2bua.addr);
    let bye = Stray::new("BYE", s.b2bua.addr, "no-call-keyed").tagged("b-gone").ruri(keyed);

    let answer = send(&s.h, &s.alice, s.b2bua.addr, &bye).await;
    assert_eq!(statuses(&answer), vec![481], "RFC 3261 §12.2.2");
    assert_eq!(
        live_transactions(&s.b2bua),
        0,
        "the 481 left statelessly (§8.2.7): no transaction holds Timer J"
    );

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// A tagged INVITE naming no dialog, answered before any provisional: its 481
/// leaves with no 100 before it and no transaction behind, and its ACK, which
/// no transaction absorbs, reaches the router and is dropped there.
#[tokio::test(start_paused = true)]
async fn a_tagged_invite_naming_no_dialog_answered_before_its_100_leaves_no_transaction() {
    let s = B2buaScene::new("b2bua-stateless-no-call-invite").await;
    s.h.allow_violation("mid-dialog-tags", "a re-INVITE in a dialog no party holds is under test");
    s.h.allow_violation(
        "no-to-tag-on-initial-request",
        "a tagged INVITE in a dialog no party holds is under test",
    );
    let invite = Stray::new("INVITE", s.b2bua.addr, "no-dialog-invite")
        .tagged("b-gone")
        .with(&["Contact: <sip:alice@192.0.2.10>"]);

    let mark = s.h.wire_entries().len();
    let wire = invite.wire(s.alice.addr());
    s.alice.try_send_datagram(&wire, s.b2bua.addr).await.expect("the INVITE leaves");
    s.h.advance(BEFORE_TIMER_G).await;
    let answer = answers_since(&s.h, &s.alice, s.b2bua.addr, mark, "INVITE");
    assert_eq!(statuses(&answer), vec![481], "no 100 before the 481 (§8.2.7), RFC 3261 §12.2.2");
    let ack = invite.ack_of(s.alice.addr(), &answer[0]);
    s.alice.try_send_datagram(&ack, s.b2bua.addr).await.expect("the ACK leaves");
    s.h.advance(BEFORE_ANY_TIMER).await;
    s.alice.drain().await;
    assert_eq!(
        statuses(&answers_since(&s.h, &s.alice, s.b2bua.addr, mark, "INVITE")),
        vec![481],
        "the 481 leaves once (§8.2.7)"
    );
    assert_eq!(
        live_transactions(&s.b2bua),
        0,
        "the 481 left statelessly (§8.2.7): no transaction waits on Timer I"
    );
    assert_eq!(
        s.b2bua.metrics().unroutable_dropped_of("ACK"),
        1,
        "no transaction absorbed the ACK: it reached the router, which drops it"
    );

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// A late PRACK naming a reliable provisional its released call showed
/// (RFC 3262 §3) draws 200, and its first arrival takes the provisional off
/// the book: no stateless answer could repeat it. It keeps its transaction,
/// so its copy draws the same 200 from there and never reaches the router.
#[tokio::test(start_paused = true)]
async fn a_late_pracks_copy_draws_its_200_again_from_its_transaction() {
    let h = Harness::with_transit_delay("b2bua-stateless-no-call-late-prack", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", bob.addr().port()).start(&h, "b2bua", &at).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(LATE_PRACK_OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_header("Require", "100rel").with_header("RSeq", "4711").await;
    let ringing = call.expect(180).await;
    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    call.expect(486).await;
    h.advance(Duration::from_millis(70)).await;

    let mark = h.wire_entries().len();
    let (mut late, prack) =
        call.try_prack_with_request(&ringing).await.expect("alice PRACKs the reliable 180");
    late.expect(200).await;
    alice.try_send_datagram(prack.image(), b2bua.addr).await.expect("the copy leaves");
    h.advance(BEFORE_ANY_TIMER).await;
    alice.drain().await;
    let answers = answers_since(&h, &alice, b2bua.addr, mark, "PRACK");
    assert_eq!(statuses(&answers), vec![200, 200], "the copy draws the same 200");
    assert_eq!(
        b2bua.metrics().late_prack_answered_total(),
        1,
        "the copy never reached the router: its transaction answered it"
    );

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A request that names a live call keeps its transaction: alice's INFO is
/// relayed and answered, and her copy of it, sent before Timer J, draws the
/// cached 200 from that transaction (§17.2.2) and never reaches the call:
/// bob is sent the INFO once.
#[tokio::test(start_paused = true)]
async fn a_request_naming_a_live_call_keeps_its_transaction() {
    let s = B2buaScene::new("b2bua-stateless-live-call-info").await;
    let mut dialog = s.establish().await;

    let mark = s.h.wire_entries().len();
    let mut info = dialog.send_request(InDialogMethod::Info).send().await;
    s.bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    let sent =
        s.h.wire_entries().into_iter().skip(mark).find(|e| {
            e.from == s.alice.addr() && e.to == s.b2bua.addr && e.raw.starts_with(b"INFO ")
        });
    let sent = sent.expect("alice's INFO is on the wire");
    s.alice.try_send_datagram(&sent.raw, s.b2bua.addr).await.expect("the copy leaves");
    s.h.advance(BEFORE_ANY_TIMER).await;
    s.alice.drain().await;
    s.bob.drain().await;

    let answers = answers_since(&s.h, &s.alice, s.b2bua.addr, mark, "INFO");
    assert_eq!(statuses(&answers), vec![200, 200], "the copy draws the cached 200");
    let relayed = s.h.wire_entries().into_iter().skip(mark);
    let relayed = relayed.filter(|e| e.to == s.bob.addr() && e.raw.starts_with(b"INFO "));
    assert_eq!(relayed.count(), 1, "the copy never reached the call: bob is sent one INFO");

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    let _ = s.finish().await;
}
