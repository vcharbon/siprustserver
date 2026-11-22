//! The form a description leaves in once the call has a restatement, and
//! the bytes a kept version leaves in (RFC 3264 §8).

use super::*;

/// A description as a peer writes it: the `fmtp` before the `rtpmap` lines,
/// no direction attribute.
fn peer_sdp(origin: &str, port: u16) -> String {
    format!(
        "v=0\r\no={origin}\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio {port} RTP/AVP 8 18\r\na=fmtp:18 annexb=no\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:18 G729/8000\r\na=ptime:20\r\n"
    )
}

/// [`peer_sdp`] in canonical form.
fn in_form(origin: &str, port: u16) -> String {
    format!(
        "v=0\r\no={origin}\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio {port} RTP/AVP 8 18\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:18 G729/8000\r\na=fmtp:18 annexb=no\r\na=ptime:20\r\na=sendrecv\r\n"
    )
}

/// Caller `a` answered by `b-1`, the transfer target `b-2` offering: the
/// first restatement of the call, toward the caller.
fn restated_once(form: SdpForm) -> (Call, String) {
    let mut c = call();
    send_in(
        &mut c,
        "a",
        Author::Leg("b-1"),
        Carried::Opening,
        &peer_sdp("bob 202 1 IN IP4 192.0.2.2", 20000),
        form,
    );
    let out = send_in(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::InDialog,
        &peer_sdp("carol 303 1 IN IP4 192.0.2.3", 30000),
        form,
    );
    (c, out)
}

/// Until the call's first restatement a peer's description leaves as its
/// author wrote it, whatever the form: a plain relay is byte-transparent.
#[test]
fn before_any_restatement_a_description_leaves_as_written() {
    let mut c = call();
    for (carried, version) in [(Carried::Opening, 1), (Carried::InDialog, 2)] {
        let body = peer_sdp(&format!("bob 202 {version} IN IP4 192.0.2.2"), 20000);
        let out = send_in(&mut c, "a", Author::Leg("b-1"), carried, &body, SdpForm::Canonical);
        assert_eq!(out, body);
    }
}

/// The restatement itself leaves in canonical form; under `AsWritten` it
/// keeps the author's attribute order.
#[test]
fn a_restatement_leaves_in_canonical_form() {
    let (_, out) = restated_once(SdpForm::Canonical);
    assert_eq!(out, in_form("bob 202 2 IN IP4 192.0.2.2", 30000));
    let (_, out) = restated_once(SdpForm::AsWritten);
    assert_eq!(out, peer_sdp("bob 202 2 IN IP4 192.0.2.2", 30000));
}

/// Once the call has a restatement, a peer's description moving the version
/// leaves in canonical form on every leg, restated or not: the caller's answer
/// opening the transfer target's dialog, and the target's own next description
/// relayed back as its author's.
#[test]
fn after_a_restatement_every_new_peer_version_leaves_in_canonical_form() {
    let (mut c, _) = restated_once(SdpForm::Canonical);
    let answer = send_in(
        &mut c,
        "b-3",
        Author::Leg("a"),
        Carried::InDialog,
        &peer_sdp("alice 101 1 IN IP4 192.0.2.1", 10000),
        SdpForm::Canonical,
    );
    assert_eq!(answer, in_form("alice 101 1 IN IP4 192.0.2.1", 10000), "not restated, in form");
    let again = send_in(
        &mut c,
        "b-3",
        Author::Leg("a"),
        Carried::InDialog,
        &peer_sdp("alice 101 2 IN IP4 192.0.2.1", 10002),
        SdpForm::Canonical,
    );
    assert_eq!(again, in_form("alice 101 2 IN IP4 192.0.2.1", 10002));
}

/// The stack's own description leaves as the stack wrote it, restatement or
/// not: only a peer's description is re-serialized.
#[test]
fn a_stack_description_keeps_its_own_form_after_a_restatement() {
    let (mut c, _) = restated_once(SdpForm::Canonical);
    let own = peer_sdp("as 7 7 IN IP4 192.0.2.9", 40000);
    let out = send_in(&mut c, "b-3", Author::Stack, Carried::Opening, &own, SdpForm::Canonical);
    assert_eq!(out, own);
}

/// A description of the stack's own stating the next version inside an early
/// dialog restates nothing: a peer's description elsewhere in the call still
/// leaves as written.
#[test]
fn a_stack_description_at_the_next_version_is_no_restatement() {
    let mut c = forked();
    answer_in_f1(&mut c, "alice 1 2 IN IP4 192.0.2.1");
    send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::Opening,
        &peer_sdp("dave 4 1 IN IP4 192.0.2.4", 40000),
    );
    let body = peer_sdp("dave 4 2 IN IP4 192.0.2.4", 40002);
    let out =
        send_in(&mut c, "a", Author::Leg("b-2"), Carried::InDialog, &body, SdpForm::Canonical);
    assert_eq!(out, body);
}

/// The restatement is the call's: a leg that restated nothing itself, whose
/// state travels with the call record, still sees it.
#[test]
fn the_calls_restatement_is_recorded_on_the_leg_it_crossed() {
    let (c, _) = restated_once(SdpForm::AsWritten);
    assert!(c.a_leg.sdp_session.has_restated, "the caller's leg carried a restatement");
    assert!(c.b_legs.iter().all(|l| !l.sdp_session.has_restated));
}

/// A restatement inside an early dialog whose fork then loses stays the
/// call's: the confirming fork and the pruned forks do not erase it.
#[test]
fn a_restatement_in_a_losing_fork_stays_the_calls() {
    let mut c = forked();
    answer_in_f1(&mut c, "alice 1 2 IN IP4 192.0.2.1");
    let reoffer = sdp("alice 1 2 IN IP4 192.0.2.1", 10002);
    in_f1(&mut c, Author::Leg("a"), Carried::InDialog, &reoffer);
    adopt_confirmed_dialog(&mut c.b_legs[0], "f2");
    for d in c.b_legs[0].dialogs.iter_mut() {
        d.state = if d.peer_tag == "f2" {
            call::DialogState::Confirmed
        } else {
            call::DialogState::Retired
        };
    }
    call::testkit::stage_leg(&mut c.b_legs[0], LegState::Confirmed);
    assert!(c.b_legs[0].sdp_session.has_restated, "the confirmed leg keeps the call's fact");
    send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::Opening,
        &peer_sdp("dave 4 1 IN IP4 192.0.2.4", 40000),
    );
    let out = send_in(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::InDialog,
        &peer_sdp("dave 4 2 IN IP4 192.0.2.4", 40002),
        SdpForm::Canonical,
    );
    assert_eq!(out, in_form("dave 4 2 IN IP4 192.0.2.4", 40002));
}

/// The leg's own state, set aside while a description crosses one of its early
/// dialogs, still states the call's restatement.
#[test]
fn the_calls_restatement_is_read_through_an_early_dialog() {
    let mut c = forked();
    let relay_in_f1 = |c: &mut Call, version: u32, form: SdpForm| {
        let body = peer_sdp(&format!("alice 1 {version} IN IP4 192.0.2.1"), 10002).into_bytes();
        let out = continue_on_leg(
            c,
            "b-1",
            Some(&call::DialogId { our_tag: String::new(), peer_tag: "f1".into() }),
            Author::Leg("a"),
            Carried::answering(&Method::Update, 200),
            body.clone(),
            Some(0..body.len()),
            policy(form),
        );
        String::from_utf8(out).unwrap()
    };
    relay_in_f1(&mut c, 2, SdpForm::AsWritten);
    c.b_legs[0].sdp_session.has_restated = true;
    assert_eq!(
        relay_in_f1(&mut c, 3, SdpForm::Canonical),
        in_form("alice 1 3 IN IP4 192.0.2.1", 10002)
    );
}

/// The caller `a` holds `b-1` on her own session, as written; the transfer
/// target `b-2`'s offer is then restated toward her: the call has a
/// restatement, `b-1`'s session none.
fn transferring() -> Call {
    let mut c = call();
    send(&mut c, "b-1", Author::Leg("a"), Carried::Opening, &peer_sdp(ALICE_1, 10000));
    restate_toward_a(&mut c);
    c
}

const ALICE_1: &str = "alice 1 1 IN IP4 192.0.2.1";
const ALICE_2: &str = "alice 1 2 IN IP4 192.0.2.1";

/// `b-1`'s answer opens the caller's dialog; `b-2`'s offer is restated on it.
fn restate_toward_a(c: &mut Call) {
    send(
        c,
        "a",
        Author::Leg("b-1"),
        Carried::Opening,
        &peer_sdp("bob 2 1 IN IP4 192.0.2.2", 20000),
    );
    let out = send_in(
        c,
        "a",
        Author::Leg("b-2"),
        Carried::InDialog,
        &peer_sdp("carol 3 1 IN IP4 192.0.2.3", 30000),
        SdpForm::Canonical,
    );
    assert_eq!(o_line(&out), "o=bob 2 2 IN IP4 192.0.2.2", "the call's restatement");
}

/// A description keeping the version the peer holds is identical to what the
/// peer holds at that version (RFC 3264 §8): the author's unchanged re-offer,
/// sent as written before the call's restatement, leaves as written after it.
#[test]
fn a_kept_version_leaves_in_the_bytes_it_first_left_in() {
    let mut c = transferring();
    let again = peer_sdp(ALICE_1, 10000);
    let out =
        send_in(&mut c, "b-1", Author::Leg("a"), Carried::InDialog, &again, SdpForm::Canonical);
    assert_eq!(out, again, "the version did not move: the bytes b-1 holds at it");
}

/// Where the version moves the description leaves in the service's form; that
/// version's unchanged re-send leaves in that form again, in a dialog or in
/// the initial INVITE's exchange.
#[test]
fn a_kept_version_first_reformed_leaves_reformed_again() {
    let mut c = transferring();
    let moved = peer_sdp(ALICE_2, 10002);
    for carried in [Carried::InDialog, Carried::InDialog, Carried::Opening] {
        let out = send_in(&mut c, "b-1", Author::Leg("a"), carried, &moved, SdpForm::Canonical);
        assert_eq!(out, in_form(ALICE_2, 10002), "{carried:?}");
    }
}

/// A repeat the stack restates at the version the peer holds (the author's
/// unchanged description past an offer the stack refused) leaves in the bytes
/// the peer holds at that version, whatever the call restated meanwhile.
#[test]
fn a_repeat_restated_at_the_held_version_leaves_in_the_held_bytes() {
    let mut c = call();
    let held = peer_sdp("bob 1 1 IN IP4 192.0.2.2", 20000);
    send(&mut c, "a", Author::Leg("b-1"), Carried::Opening, &held);
    super::super::note_absorbed_offer(
        &mut c,
        "b-1",
        peer_sdp("bob 1 2 IN IP4 192.0.2.2", 20004).as_bytes(),
    );
    send(
        &mut c,
        "b-3",
        Author::Leg("b-1"),
        Carried::Opening,
        &peer_sdp("bob 1 1 IN IP4 192.0.2.2", 20000),
    );
    let elsewhere = send_in(
        &mut c,
        "b-3",
        Author::Leg("b-2"),
        Carried::InDialog,
        &peer_sdp("carol 3 1 IN IP4 192.0.2.3", 30000),
        SdpForm::Canonical,
    );
    assert_eq!(o_line(&elsewhere), "o=bob 1 2 IN IP4 192.0.2.2", "the call's restatement");
    let repeat = peer_sdp("bob 1 3 IN IP4 192.0.2.2", 20000);
    let out =
        send_in(&mut c, "a", Author::Leg("b-1"), Carried::InDialog, &repeat, SdpForm::Canonical);
    assert_eq!(out, held, "restated at the held version, in the held bytes");
}

/// Every crossing the seam asks a policy about, in order.
#[derive(Debug, Default)]
struct Recording(std::sync::Mutex<Vec<SdpCrossing>>);

impl SdpFormPolicy for Recording {
    fn form(&self, crossing: &SdpCrossing) -> SdpForm {
        self.0.lock().unwrap().push(*crossing);
        SdpForm::AsWritten
    }
}

/// The seam asks the policy about in-dialog descriptions moving the version
/// only, and states who wrote each and whether the call, or this description,
/// is restated.
#[test]
fn the_policy_sees_each_in_dialog_crossing_as_it_stands() {
    let rec = Recording::default();
    let mut c = call();
    let cross = |c: &mut Call, author: Author<'_>, carried: Carried, body: String| {
        let whole = Some(0..body.len());
        continue_on_leg(c, "a", None, author, carried, body.into_bytes(), whole, &rec);
    };
    cross(&mut c, Author::Leg("b-1"), Carried::Opening, peer_sdp("bob 202 1 IN IP4 192.0.2.2", 1));
    cross(&mut c, Author::Leg("b-1"), Carried::InDialog, peer_sdp("bob 202 2 IN IP4 192.0.2.2", 2));
    cross(&mut c, Author::Leg("b-1"), Carried::InDialog, peer_sdp("bob 202 2 IN IP4 192.0.2.2", 2));
    cross(&mut c, Author::Leg("b-2"), Carried::InDialog, peer_sdp("carol 3 1 IN IP4 192.0.2.3", 3));
    cross(&mut c, Author::Stack, Carried::InDialog, peer_sdp("as 7 7 IN IP4 192.0.2.9", 4));
    let seen = rec.0.lock().unwrap().clone();
    let crossing =
        |by_peer, restated, call_restated| SdpCrossing { by_peer, restated, call_restated };
    assert_eq!(
        seen,
        vec![crossing(true, false, false), crossing(true, true, true), crossing(false, true, true)],
        "neither the opening description nor the kept version is asked about"
    );
}
