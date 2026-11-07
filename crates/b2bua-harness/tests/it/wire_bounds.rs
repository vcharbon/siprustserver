//! ADR-0001's bound on full calls through the front proxy and a worker: what a
//! party's first hop accepts completes its call whatever the hops add, and what
//! it refuses is answered there. Every call ends with its CDR and its limiter
//! hold released.

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::{Agent, Harness};

use crate::common;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const PROXY: &str = "127.0.0.1:5080";
const B2BUA: &str = "127.0.0.1:5090";

/// The bound on a line and on a Request-URI.
const LINE_BOUND: usize = 8_192;
/// The bound on a header block.
const LINES_BOUND: usize = 100;
/// The lines the harness UA writes on an INVITE carrying an offer: the request
/// line and nine headers.
const OWN_LINES: usize = 10;

struct Rig {
    h: Harness,
    alice: Agent,
    bob: Agent,
    proxy: common::ProxySut,
    b2bua: B2buaSut,
}

impl Rig {
    async fn new(name: &str) -> Rig {
        let h = Harness::new(name);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let proxy = common::spawn_lb_proxy(&h, PROXY, "b2bua", B2BUA.parse().unwrap()).await;
        let b2bua = B2buaSut::route_all_to("127.0.0.1", 5070)
            .behind_proxy("127.0.0.1", 5080)
            .start(&h, "b2bua", B2BUA)
            .await;
        Rig { h, alice, bob, proxy, b2bua }
    }

    /// The call's CDR written and everything of it released.
    async fn finish(self) {
        settle_until(|| self.b2bua.cdr_records().len() == 1).await;
        settle_until(|| self.b2bua.is_reaped()).await;
        self.b2bua.assert_fully_reaped();
        let _proxy = self.proxy;
        self.h.finish().await;
    }

    /// No call was born: the worker saw nothing of it.
    async fn finish_with_no_call(self) {
        assert!(self.b2bua.cdr_records().is_empty(), "no call reached the worker");
        self.b2bua.assert_fully_reaped();
        let _proxy = self.proxy;
        self.h.finish().await;
    }
}

/// `invite` answered by bob and hung up by alice.
async fn full_call(r: &Rig, invite: scenario_harness::Invite<'_>) {
    let mut call = invite.send().await;
    r.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    r.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    r.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
}

/// `head` with its top Via line asking `rport`, so a hop stamps it, and padded to
/// `len` bytes by an extension parameter.
fn pad_top_via(head: &str, len: usize) -> String {
    let line = head.lines().find(|l| l.starts_with("Via:")).expect("a Via line");
    let frame = ";rport;x=";
    let padded = format!("{line}{frame}{}", "a".repeat(len - line.len() - frame.len()));
    head.replacen(line, &padded, 1)
}

/// A caller's `Identity` of 2,600 bytes, which real traffic carries, does not
/// keep the call from being set up and torn down.
#[tokio::test(start_paused = true)]
async fn a_2600_byte_identity_header_on_a_full_call() {
    let r = Rig::new("wire-bounds-identity").await;
    let identity = format!("{};info=<https://cert.example.com/a.pem>;alg=ES256", "a".repeat(2_560));
    assert!(identity.len() > 2_590 && identity.len() < 2_620, "{}", identity.len());
    let invite = r
        .alice
        .invite(&r.bob)
        .with_header("Identity", &identity)
        .with_sdp(OFFER)
        .through(r.proxy.addr());
    full_call(&r, invite).await;
    r.finish().await;
}

/// A To at the line-length bound draws its provisional and its final, which
/// grow by the callee's tag and the stack's.
#[tokio::test(start_paused = true)]
async fn a_to_at_the_bound_draws_its_final() {
    let r = Rig::new("wire-bounds-to").await;
    let frame = "To: <sip:@example.com>".len();
    let to = format!("sip:{}@example.com", "b".repeat(LINE_BOUND - frame));

    let mut call =
        r.alice.invite(&r.bob).to(&to).with_sdp(OFFER).through(r.proxy.addr()).send().await;
    let mut uas = r.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(486, "Busy Here").await;
    call.expect(486).await;
    r.finish().await;
}

/// A To one past the bound is answered at first ingress, and no call is born.
#[tokio::test(start_paused = true)]
async fn a_to_one_past_the_bound_is_answered_at_first_ingress() {
    let r = Rig::new("wire-bounds-to-past").await;
    let frame = "To: <sip:@example.com>".len();
    let to = format!("sip:{}@example.com", "b".repeat(LINE_BOUND + 1 - frame));

    let mut call =
        r.alice.invite(&r.bob).to(&to).with_sdp(OFFER).through(r.proxy.addr()).send().await;
    call.expect(400).await;
    r.finish_with_no_call().await;
}

/// A top Via at the bound is stamped past it by the proxy, read by the worker
/// and echoed on every response, which the proxy reads on the way back.
#[tokio::test(start_paused = true)]
async fn a_top_via_at_the_bound_completes_its_call() {
    fn at_the_bound(head: &str) -> String {
        pad_top_via(head, LINE_BOUND)
    }
    let r = Rig::new("wire-bounds-via").await;
    let invite =
        r.alice.invite(&r.bob).with_sdp(OFFER).through(r.proxy.addr()).wire_edit(at_the_bound);
    full_call(&r, invite).await;
    r.finish().await;
}

/// A top Via one past the bound is answered at first ingress.
#[tokio::test(start_paused = true)]
async fn a_top_via_one_past_the_bound_is_answered_at_first_ingress() {
    fn past_the_bound(head: &str) -> String {
        pad_top_via(head, LINE_BOUND + 1)
    }
    let r = Rig::new("wire-bounds-via-past").await;
    let mut call = r
        .alice
        .invite(&r.bob)
        .with_sdp(OFFER)
        .through(r.proxy.addr())
        .wire_edit(past_the_bound)
        .send()
        .await;
    call.expect(400).await;
    r.finish_with_no_call().await;
}

/// `lines` lines in all, padded with an extension header.
fn padded(r: &Rig, lines: usize) -> scenario_harness::Invite<'_> {
    let mut invite = r.alice.invite(&r.bob).with_sdp(OFFER).through(r.proxy.addr());
    for n in 0..(lines - OWN_LINES) {
        invite = invite.with_header(&format!("X-Pad-{n}"), "p");
    }
    invite
}

/// An INVITE at exactly the line bound, relayed whole to the callee with the
/// worker's preloaded Route and two forwards' additions: its call completes.
#[tokio::test(start_paused = true)]
async fn a_request_at_the_line_bound_completes_its_call() {
    let r = Rig::new("wire-bounds-lines").await;
    let invite = padded(&r, LINES_BOUND);
    full_call(&r, invite).await;
    r.finish().await;
}

/// With no Max-Forwards of its own, the proxy adds one on top of its Via and
/// its Record-Routes.
#[tokio::test(start_paused = true)]
async fn a_request_at_the_line_bound_with_no_max_forwards_completes_its_call() {
    fn no_max_forwards(head: &str) -> String {
        let line = head.lines().find(|l| l.starts_with("Max-Forwards:")).expect("the line");
        let without = head.replacen(&format!("{line}\r\n"), "", 1);
        without.replacen("Call-ID:", "X-Pad-last: p\r\nCall-ID:", 1)
    }
    let r = Rig::new("wire-bounds-lines-mf").await;
    r.h.allow_violation("max-forwards", "the caller under test omits Max-Forwards");
    let invite = padded(&r, LINES_BOUND).wire_edit(no_max_forwards);
    full_call(&r, invite).await;
    r.finish().await;
}

/// A top Via folding lower hops on its line at the bound: the proxy's stamp and
/// the worker's echo keep them on the one line.
#[tokio::test(start_paused = true)]
async fn a_folded_top_via_at_the_line_bound_completes_its_call() {
    fn folded(head: &str) -> String {
        let hop = "SIP/2.0/UDP 192.0.2.40:5060;branch=z9hG4bK-lower";
        let line = head.lines().find(|l| l.starts_with("Via:")).expect("a Via line");
        head.replacen(line, &format!("{line}, {hop}, {hop}, {hop}"), 1)
    }
    let r = Rig::new("wire-bounds-lines-via").await;
    let invite = padded(&r, LINES_BOUND).wire_edit(folded);
    full_call(&r, invite).await;
    r.finish().await;
}

/// One line past the bound is answered at first ingress.
#[tokio::test(start_paused = true)]
async fn a_request_one_line_past_the_bound_is_answered_at_first_ingress() {
    let r = Rig::new("wire-bounds-lines-past").await;
    let mut call = padded(&r, LINES_BOUND + 1).send().await;
    call.expect(400).await;
    r.finish_with_no_call().await;
}

/// A Contact URI at the bound becomes the Request-URI of the BYE the stack
/// sends when the callee hangs up.
#[tokio::test(start_paused = true)]
async fn a_contact_at_the_bound_receives_the_stacks_bye() {
    let r = Rig::new("wire-bounds-contact").await;
    let frame = "Contact: <sip:@127.0.0.1:5060>".len();
    let contact = format!("sip:{}@127.0.0.1:5060", "c".repeat(LINE_BOUND - frame));
    let mut call = r
        .alice
        .invite(&r.bob)
        .with_contact(&contact)
        .with_sdp(OFFER)
        .through(r.proxy.addr())
        .send()
        .await;
    let mut uas = r.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    r.bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();
    let mut bye = bob_dialog.bye().await;
    let mut relayed = r.alice.receive("BYE").await;
    assert_eq!(relayed.request().request_uri().to_string(), contact, "the BYE's target");
    relayed.respond(200, "OK").await;
    bye.expect(200).await;
    r.finish().await;
}

/// A Call-ID at the bound, every byte of it one the stack escapes: the worker's
/// Via and Contact carry it at three times its length, past any line the caller
/// could send, and the call completes.
#[tokio::test(start_paused = true)]
async fn a_call_id_at_the_bound_completes_its_call() {
    let r = Rig::new("wire-bounds-call-id").await;
    let call_id = "%".repeat(LINE_BOUND - "Call-ID: ".len());
    let invite =
        r.alice.invite(&r.bob).with_call_id(call_id).with_sdp(OFFER).through(r.proxy.addr());
    full_call(&r, invite).await;
    r.finish().await;
}

/// A worker with no proxy is the first hop: it holds the same bound.
#[tokio::test(start_paused = true)]
async fn a_standalone_worker_answers_one_past_the_bound() {
    let s = B2buaScene::new("wire-bounds-standalone").await;
    let frame = "To: <sip:@example.com>".len();
    let at = format!("sip:{}@example.com", "b".repeat(LINE_BOUND - frame));
    let past = format!("sip:{}@example.com", "b".repeat(LINE_BOUND + 1 - frame));

    let mut refused =
        s.alice.invite(&s.bob).to(&past).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    refused.expect(400).await;

    let mut call =
        s.alice.invite(&s.bob).to(&at).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    s.bob.receive("INVITE").await.respond(486, "Busy Here").await;
    call.expect(486).await;
    s.finish().await;
}
