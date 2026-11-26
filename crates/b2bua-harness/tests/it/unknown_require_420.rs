//! RFC 3261 §8.2.2.3: a UAS handed a request whose `Require` names an option
//! tag it does not understand answers 420 Bad Extension, listing in
//! `Unsupported` exactly the tags it does not understand. The back-to-back UA
//! is the UAS of every request it receives, so it refuses such a request
//! itself: nothing reaches the peer, nobody is dialled, and the dialog, its
//! session and the call go on as they were. A `Require` naming only what the
//! stack understands still lets the request through.

use std::time::Duration;

use b2bua::admission::Class;
use b2bua::new_calls::Refusal;
use b2bua_harness::{settle_until, B2buaScene};
use call::TerminationCause;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, Dialog, Harness};
use sip_message::generators::{InDialogMethod, OutOfDialogMethod};
use sip_message::header::HeaderName;
use sip_message::{SipRequest, SipResponse};

/// An option tag no implementation defines.
const UNKNOWN: &str = "x-nonexistent";

/// Past a request's two hops through the stack to the peer
/// ([`scenario_harness::SIMULATED_TRANSIT_DELAY_MS`] each) and short of its
/// first retransmission (RFC 3261 §17.1.2.2, T1).
const RELAY_WINDOW: Duration = Duration::from_millis(300);

const ALICE_REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

/// The `Unsupported` tags `resp` lists, one per entry, in order.
fn unsupported(resp: &SipResponse) -> Vec<String> {
    resp.raw(HeaderName::Unsupported)
        .flat_map(|line| line.split(','))
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect()
}

fn request_values(req: &SipRequest, name: HeaderName) -> Vec<String> {
    req.raw(name).map(str::to_string).collect()
}

/// A confirmed call alice ↔ b2bua ↔ bob, with each party's own dialog.
struct Confirmed {
    s: B2buaScene,
    alice: Dialog,
    bob: Dialog,
}

/// Which party sends the request a case is about.
#[derive(Clone, Copy)]
enum Side {
    Caller,
    Callee,
}

impl Confirmed {
    async fn new(name: &str) -> Self {
        let s = B2buaScene::new(name).await;
        let mut call =
            s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
        let mut uas = s.bob.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        call.expect(200).await;
        let alice = call.ack().await;
        s.bob.receive("ACK").await;
        let bob = uas.dialog();
        Self { s, alice, bob }
    }

    /// The harness, the sender's dialog, the peer the stack relays to, and the
    /// sender's next offer.
    fn sides(&mut self, side: Side) -> (&Harness, &mut Dialog, &Agent, &'static str) {
        let Self { s, alice, bob } = self;
        match side {
            Side::Caller => (&s.h, alice, &s.bob, ALICE_REOFFER),
            Side::Callee => (&s.h, bob, &s.alice, BOB_REOFFER),
        }
    }

    /// The sender on `side` sends `method` requiring `require`, carrying an
    /// offer where `offer`: the peer is sent nothing, neither the request nor
    /// the ACK of its refusal, and the sender is answered 420. Returns the
    /// tags the 420 lists in `Unsupported`.
    async fn refused(
        &mut self,
        side: Side,
        method: InDialogMethod,
        require: &str,
        offer: bool,
    ) -> Vec<String> {
        let (h, dialog, peer, sdp) = self.sides(side);
        let name = method.as_str();
        let mut request = dialog.send_request(method).with_header("Require", require);
        if offer {
            request = request.with_sdp(sdp);
        }
        let mut txn = request.send().await;
        h.advance(RELAY_WINDOW).await;
        let relayed = peer.take_queued().await;
        assert!(relayed.is_none(), "the {name} is refused, not relayed; the peer got {relayed:?}");
        let refusal = txn.expect(420).await;
        h.advance(RELAY_WINDOW).await;
        let after = peer.take_queued().await;
        assert!(after.is_none(), "nothing of the refused {name} reaches the peer, got {after:?}");
        unsupported(&refusal)
    }

    /// The sender on `side` offers again in a plain `method`, which reaches
    /// the peer and is answered: the refused offer left the session as it was
    /// (RFC 3261 §14.2), so this one is no glare and no second offer.
    async fn renegotiated(&mut self, side: Side, method: InDialogMethod) {
        let (_, dialog, peer, sdp) = self.sides(side);
        let mut txn = dialog.send_request(method).with_sdp(sdp).send().await;
        let answer = match side {
            Side::Caller => ANSWER_SDP,
            Side::Callee => OFFER_SDP,
        };
        peer.receive(method.as_str()).await.respond(200, "OK").with_sdp(answer).await;
        txn.expect(200).await;
        if matches!(method, InDialogMethod::Invite) {
            dialog.ack(None).await;
            peer.receive("ACK").await;
        }
    }

    /// alice hangs up; the call ends with its one CDR, fully reaped.
    async fn hang_up(mut self) {
        self.s.hangup(&mut self.alice).await;
        settle_until(|| self.s.b2bua.cdr_records().len() == 1).await;
        assert_eq!(self.s.b2bua.cdr_records().len(), 1, "one call, one CDR");
        let _report = self.s.finish().await;
    }
}

/// `method` from `side` requiring the unknown tag is refused 420 naming it;
/// where it carries an offer, a plain offer after it renegotiates the session.
async fn refused_in_dialog(name: &str, side: Side, method: InDialogMethod, offer: bool) {
    let mut c = Confirmed::new(name).await;
    let listed = c.refused(side, method, UNKNOWN, offer).await;
    assert_eq!(listed, [UNKNOWN], "the 420 lists the tag not understood");
    if offer {
        c.renegotiated(side, method).await;
    }
    c.hang_up().await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_info_requiring_an_unknown_tag_is_refused_420() {
    refused_in_dialog("unknown-require-info", Side::Caller, InDialogMethod::Info, false).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_info_requiring_an_unknown_tag_is_refused_420() {
    refused_in_dialog("unknown-require-info-callee", Side::Callee, InDialogMethod::Info, false)
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_update_requiring_an_unknown_tag_is_refused_420() {
    refused_in_dialog("unknown-require-update", Side::Caller, InDialogMethod::Update, true).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_update_requiring_an_unknown_tag_is_refused_420() {
    refused_in_dialog("unknown-require-update-callee", Side::Callee, InDialogMethod::Update, true)
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_reinvite_requiring_an_unknown_tag_is_refused_420() {
    refused_in_dialog("unknown-require-reinvite", Side::Caller, InDialogMethod::Invite, true).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_reinvite_requiring_an_unknown_tag_is_refused_420() {
    refused_in_dialog(
        "unknown-require-reinvite-callee",
        Side::Callee,
        InDialogMethod::Invite,
        true,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn an_in_dialog_message_requiring_an_unknown_tag_is_refused_420() {
    refused_in_dialog("unknown-require-message", Side::Caller, InDialogMethod::Message, false)
        .await;
}

#[tokio::test(start_paused = true)]
async fn an_in_dialog_options_requiring_an_unknown_tag_is_refused_420() {
    refused_in_dialog("unknown-require-options", Side::Caller, InDialogMethod::Options, false)
        .await;
}

/// A BYE requiring the unknown tag is refused like any request: the dialog
/// stays up (RFC 3261 §15.1.1 ends it only on a 2xx, 481 or 408), and a plain
/// BYE then ends the call.
#[tokio::test(start_paused = true)]
async fn a_bye_requiring_an_unknown_tag_is_refused_420_and_a_plain_bye_ends_the_call() {
    let mut c = Confirmed::new("unknown-require-bye").await;
    let listed = c.refused(Side::Caller, InDialogMethod::Bye, UNKNOWN, false).await;
    assert_eq!(listed, [UNKNOWN], "the 420 lists the tag not understood");
    assert_eq!(c.s.b2bua.active_calls(), 1, "the refused BYE left the call up");
    c.hang_up().await;
}

/// Of an UPDATE's `timer, x-nonexistent`, the stack understands `timer`, which
/// it relays end to end on a session request (RFC 4028 §7.1): the 420 lists
/// only the other one.
#[tokio::test(start_paused = true)]
async fn the_420_lists_only_the_tags_not_understood() {
    let mut c = Confirmed::new("unknown-require-mixed").await;
    let required = format!("timer, {UNKNOWN}");
    let listed = c.refused(Side::Caller, InDialogMethod::Update, &required, true).await;
    assert_eq!(listed, [UNKNOWN], "timer is understood, so it is not listed");
    c.renegotiated(Side::Caller, InDialogMethod::Update).await;
    c.hang_up().await;
}

/// A re-INVITE requiring `100rel` asks that every provisional to it come
/// reliably (RFC 3262 §3); the stack relays the peer's provisionals to a
/// re-INVITE as they come and never puts the requirement to the peer, so it
/// refuses the re-INVITE 420 naming `100rel`, from either side.
#[tokio::test(start_paused = true)]
async fn a_reinvite_requiring_100rel_is_refused_420() {
    for (name, side) in
        [("reinvite-100rel", Side::Caller), ("reinvite-100rel-callee", Side::Callee)]
    {
        let mut c = Confirmed::new(name).await;
        let listed = c.refused(side, InDialogMethod::Invite, "100rel", true).await;
        assert_eq!(listed, ["100rel"], "the 420 names the requirement the stack cannot meet");
        c.renegotiated(side, InDialogMethod::Invite).await;
        c.hang_up().await;
    }
}

/// `timer` names the session timer, which this stack relays end to end on a
/// session request only (RFC 4028 §7.1): an INFO requiring it is refused 420
/// naming it.
#[tokio::test(start_paused = true)]
async fn an_info_requiring_timer_is_refused_420() {
    let mut c = Confirmed::new("unknown-require-timer-info").await;
    let listed = c.refused(Side::Caller, InDialogMethod::Info, "timer", false).await;
    assert_eq!(listed, ["timer"], "timer is no requirement an INFO's UAS takes here");
    c.hang_up().await;
}

/// An UPDATE requiring `timer` (RFC 4028 §7.1) is a requirement the stack
/// relays end to end: it reaches the peer with its `Require`, and the peer's
/// answer comes back.
#[tokio::test(start_paused = true)]
async fn an_update_requiring_timer_still_reaches_the_peer() {
    let mut c = Confirmed::new("unknown-require-timer-update").await;
    let mut update = c
        .alice
        .send_request(InDialogMethod::Update)
        .with_header("Supported", "timer")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .send()
        .await;
    let mut at_bob = c.s.bob.receive("UPDATE").await;
    assert_eq!(request_values(at_bob.request(), HeaderName::Require), ["timer"]);
    at_bob
        .respond(200, "OK")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .await;
    update.expect(200).await;
    c.hang_up().await;
}

/// The callee hangs up; alice's OPTIONS requiring the unknown tag crosses the
/// stack's BYE toward her. The stack answers it itself in the ending dialog,
/// and that answer is the 420 too.
#[tokio::test(start_paused = true)]
async fn an_options_the_stack_answers_itself_is_refused_420() {
    let mut c = Confirmed::new("unknown-require-absorbed-options").await;
    c.bob.bye().await.expect(200).await;
    let mut ours = c.s.alice.receive("BYE").await;
    let refusal = c
        .alice
        .send_request(InDialogMethod::Options)
        .with_header("Require", UNKNOWN)
        .send()
        .await
        .expect(420)
        .await;
    assert_eq!(unsupported(&refusal), [UNKNOWN], "the 420 lists the tag not understood");
    c.s.h.advance(Duration::from_millis(100)).await;
    ours.respond(200, "OK").await;
    settle_until(|| c.s.b2bua.cdr_records().len() == 1).await;
    let _report = c.s.finish().await;
}

/// An out-of-dialog OPTIONS, which the stack answers itself with its health
/// report, requiring the unknown tag is refused 420 naming it; a plain one is
/// still answered 200.
#[tokio::test(start_paused = true)]
async fn a_keepalive_options_requiring_an_unknown_tag_is_refused_420() {
    let s = B2buaScene::new("unknown-require-keepalive").await;
    let options = || s.alice.request(OutOfDialogMethod::Options, &s.bob).through(s.b2bua.addr);
    let refusal = options().with_header("Require", UNKNOWN).send().await.expect(420).await;
    assert_eq!(unsupported(&refusal), [UNKNOWN], "the 420 lists the tag not understood");
    options().send().await.expect(200).await;
    let _report = s.finish().await;
}

/// An initial INVITE requiring the unknown tag is refused 420 naming it before
/// any leg is dialled, and the refused call has its CDR. It is counted a
/// refusal on sight, never an admitted call: the `adm` the load balancer
/// reads off `X-Overload` stays at zero.
#[tokio::test(start_paused = true)]
async fn an_initial_invite_requiring_an_unknown_tag_is_refused_before_any_dial() {
    let s = B2buaScene::new("unknown-require-initial-invite").await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .with_header("Require", UNKNOWN)
        .through(s.b2bua.addr)
        .send()
        .await;
    s.h.advance(RELAY_WINDOW).await;
    let dialled = s.bob.take_queued().await;
    assert!(dialled.is_none(), "nobody is dialled, bob got {dialled:?}");
    let refusal = call.expect(420).await;
    assert_eq!(unsupported(&refusal), [UNKNOWN], "the 420 lists the tag not understood");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let cdrs = s.b2bua.cdr_records();
    let cdr = cdrs.iter().find(|r| r.a_leg.call_id == call.call_id()).expect("its CDR");
    assert!(cdr.b_legs.is_empty(), "no b-leg: {cdr:?}");
    assert_eq!(cdr.termination.as_ref().map(|t| t.cause), Some(TerminationCause::Admission));
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::OnSight, Class::Normal), 1, "refused on sight");
    assert_eq!(counts.accepted(Class::Normal), 0, "no admitted call");
    let header = s.b2bua.overload().x_overload_header_value();
    assert!(header.ends_with("adm=0"), "the refused call is not counted as treated: {header}");
    let _report = s.finish().await;
}
