//! The caller's early dialogs, each its own session (RFC 3261 §12.1.2), and
//! the versions a peer receives (RFC 3264 §8).

use super::*;

/// A caller leg `a` still in setup, holding the caller-facing early dialogs
/// `x` and `y`, and legs `b-1`, `b-2`, `b-3` of its own.
fn early_caller() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &invite("w0", "w1", "sdp"),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    for id in ["b-1", "b-2", "b-3"] {
        let mut leg = call.a_leg.clone();
        leg.leg_id = id.into();
        call.b_legs.push(leg);
    }
    let caller = call.a_leg_invite.from_tag.clone();
    for tag in ["x", "y"] {
        call.a_leg.dialogs.insert(call::Dialog::new(tag, &caller, call::DialogState::Early, 0));
    }
    call
}

/// The caller-facing dialog this stack shows under `tag`.
fn face(c: &Call, tag: &str) -> call::DialogId {
    call::DialogId { our_tag: tag.to_string(), peer_tag: c.a_leg_invite.from_tag.clone() }
}

/// What reaches the caller of `body`, written by `author`, in her dialog `tag`.
fn to_caller(c: &mut Call, tag: &str, author: Author<'_>, carried: Carried, body: &str) -> String {
    let id = face(c, tag);
    let out = continue_on_leg(
        c,
        "a",
        Some(&id),
        author,
        carried,
        body.as_bytes().to_vec(),
        Some(&ct()),
        policy(SdpForm::AsWritten),
    );
    String::from_utf8(out).unwrap()
}

/// Two caller-facing early dialogs are two sessions (RFC 3261 §12.1.2, RFC
/// 3264 §8): a media leg's early media in `x` and the callee's answer in `y`
/// each leave as written, and the answer confirming `y` leaves the leg on the
/// callee's session.
#[test]
fn each_caller_facing_early_dialog_carries_its_own_session() {
    let mut c = early_caller();
    let media = sdp("mrf 1 1 IN IP4 192.0.2.4", 40000);
    let answer = sdp("dest 7 1 IN IP4 192.0.2.5", 50000);
    assert_eq!(to_caller(&mut c, "x", Author::Leg("b-1"), Carried::Opening, &media), media);
    assert_eq!(to_caller(&mut c, "y", Author::Leg("b-2"), Carried::Opening, &answer), answer);
    let caller = c.a_leg_invite.from_tag.clone();
    crate::rules::relay::adopt_answered_caller_dialog(&mut c.a_leg, "y", &caller);
    assert_eq!(c.a_leg.sdp_session.sent_origin.as_deref(), Some("dest 7 1 IN IP4 192.0.2.5"));
}

/// One early dialog is one session: a second author's opening description in
/// it (a second media leg, a rerouted callee shown under the same tag) is
/// restated as the next version of the session the dialog carries.
#[test]
fn another_authors_opening_description_continues_the_early_dialogs_session() {
    let mut c = early_caller();
    let first = sdp("mrf 1 1 IN IP4 192.0.2.4", 40000);
    assert_eq!(to_caller(&mut c, "x", Author::Leg("b-1"), Carried::Opening, &first), first);
    let second = to_caller(
        &mut c,
        "x",
        Author::Leg("b-3"),
        Carried::Opening,
        &sdp("mrf 2 2 IN IP4 192.0.2.4", 40002),
    );
    assert_eq!(o_line(&second), "o=mrf 1 2 IN IP4 192.0.2.4");
}

/// The stack answers `offer` from `b-1`'s peer itself (a glare 491): its
/// version reaches no other leg.
fn refused_from_b1(c: &mut Call, offer: &str) {
    super::super::note_absorbed_offer(c, "b-1", offer.as_bytes());
}

/// What the caller receives continues what she last received, whatever
/// versions the author spent on offers the stack refused: the author's same
/// description again past a refused offer leaves at the version she holds, a
/// changed one at the next.
#[test]
fn a_version_the_caller_never_received_is_not_skipped_over() {
    let ringing = sdp("bob 1 1 IN IP4 192.0.2.2", 20000);
    let refused = sdp("bob 1 2 IN IP4 192.0.2.2", 20004);
    for (port, expected) in
        [(20000, "o=bob 1 1 IN IP4 192.0.2.2"), (20002, "o=bob 1 2 IN IP4 192.0.2.2")]
    {
        let mut c = early_caller();
        to_caller(&mut c, "x", Author::Leg("b-1"), Carried::Opening, &ringing);
        refused_from_b1(&mut c, &refused);
        let after = sdp("bob 1 3 IN IP4 192.0.2.2", port);
        let out = to_caller(&mut c, "x", Author::Leg("b-1"), Carried::Opening, &after);
        assert_eq!(o_line(&out), expected, "port {port}");
    }
}

/// The same on a confirmed dialog: the author's re-offer the stack refused
/// never reached the far party, so its next description is restated above
/// what that party holds, not above what the author spent.
#[test]
fn a_skipped_version_on_a_confirmed_dialog_is_restated_above_the_held_one() {
    let mut c = call();
    let opening = sdp("bob 1 1 IN IP4 192.0.2.2", 20000);
    send(&mut c, "a", Author::Leg("b-1"), Carried::Opening, &opening);
    let held = sdp("bob 1 2 IN IP4 192.0.2.2", 20002);
    assert_eq!(send(&mut c, "a", Author::Leg("b-1"), Carried::InDialog, &held), held);
    refused_from_b1(&mut c, &sdp("bob 1 3 IN IP4 192.0.2.2", 20006));
    let after = sdp("bob 1 4 IN IP4 192.0.2.2", 20004);
    let out = send(&mut c, "a", Author::Leg("b-1"), Carried::InDialog, &after);
    assert_eq!(o_line(&out), "o=bob 1 3 IN IP4 192.0.2.2");
}

/// A version the author skipped on its own account — no offer of its was
/// answered here — is the author's: its description leaves byte for byte, and
/// the leg records no restatement.
#[test]
fn an_authors_own_skip_leaves_as_written() {
    let mut c = call();
    send(
        &mut c,
        "a",
        Author::Leg("b-1"),
        Carried::Opening,
        &sdp("bob 1 1 IN IP4 192.0.2.2", 20000),
    );
    let skipped = sdp("bob 1 4 IN IP4 192.0.2.2", 20004);
    assert_eq!(send(&mut c, "a", Author::Leg("b-1"), Carried::InDialog, &skipped), skipped);
    assert!(!c.a_leg.sdp_session.restated && !c.a_leg.sdp_session.has_restated);
}
