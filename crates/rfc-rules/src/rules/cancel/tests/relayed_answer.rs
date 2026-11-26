//! A 2xx after a taken CANCEL that the endpoint relays from its far side, on
//! another Call-ID (RFC 3261 §9.2, §16.10): which relays the rule excuses, and
//! which it still charges.

use super::*;

/// The relaying element between [`UAC`] and a far UAS, on its own Call-ID.
const RELAY: &str = "10.0.0.3:5060";
const FAR: &str = "10.0.0.4:5060";

/// `m` sent from `src` to `dst` on Call-ID `call_id`.
fn between(src: &str, dst: &str, call_id: &str, mut m: Msg) -> Msg {
    m.src = src.to_string();
    m.dst = dst.to_string();
    m.call_id = call_id.to_string();
    m
}

/// The description a far UAS's 200 carries, and the relay's 200 carries back.
const FAR_SDP: &[u8] =
    b"v=0\r\no=far 1 1 IN IP4 10.0.0.4\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";

/// `m` carrying `body`.
fn with_body(body: &[u8], mut m: Msg) -> Msg {
    m.body = Some(body.to_vec());
    m
}

/// A relaying element (a B2BUA, its far leg on another Call-ID) takes the
/// UAC's INVITE and relays it, takes the UAC's CANCEL and relays it, and the
/// far UAS's 200 comes back at `far_200_at`: the element answers the UAC with
/// that 200's description at 2.3 s (§16.10).
fn relayed_crossing(far_200_at: u64) -> Vec<Msg> {
    vec![
        between(UAC, RELAY, "c1", req(1_000, "INVITE")),
        on(B2, between(RELAY, FAR, "c2", req(1_100, "INVITE"))),
        on(B2, between(FAR, RELAY, "c2", rsp(1_200, 100))),
        between(UAC, RELAY, "c1", req(2_000, "CANCEL")),
        on(B2, between(RELAY, FAR, "c2", req(2_100, "CANCEL"))),
        with_body(FAR_SDP, on(B2, between(FAR, RELAY, "c2", rsp(far_200_at, 200)))),
        with_body(FAR_SDP, between(RELAY, UAC, "c1", rsp(far_200_at.max(2_300) + 100, 200))),
    ]
}

/// The far 200 crossed the relayed CANCEL: compliant on both legs.
#[test]
fn a_relayed_200_that_crossed_the_relayed_cancel_is_compliant() {
    let msgs = relayed_crossing(2_050);
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    let relay: Vec<_> = f.iter().filter(|f| f.emitter == RELAY).collect();
    assert_eq!(relay.len(), 1, "the relay's cancelled transaction is one occasion: {f:?}");
    assert!(matches!(relay[0].decision, Decision::Compliant), "{:?}", relay[0].decision);
    assert!(relay[0].relayed, "the relay carries the far UAS's answer: {f:?}");
    let far: Vec<_> = f.iter().filter(|f| f.emitter == FAR).collect();
    assert!(matches!(far[0].decision, Decision::Compliant), "the far 200 crossed: {far:?}");
}

/// The far UAS answers 200 only after it took the relayed CANCEL: the far UAS
/// broke §9.2 and is charged; the relay forwarded its answer and is not.
#[test]
fn a_relayed_200_the_far_uas_sent_after_the_cancel_charges_the_far_uas() {
    let msgs = relayed_crossing(2_200);
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    let far = f.iter().find(|f| f.emitter == FAR).expect("the far UAS's occasion");
    assert!(matches!(far.decision, Decision::Violated(_)), "{far:?}");
    let relay = f.iter().find(|f| f.emitter == RELAY).expect("the relay's occasion");
    assert!(matches!(relay.decision, Decision::Compliant), "{relay:?}");
}

/// A relay whose 200 leaves before its far UAS answered originates it: the
/// far answer coming later relays nothing back.
#[test]
fn a_relays_200_ahead_of_the_far_answer_is_violated() {
    let mut msgs = relayed_crossing(2_400);
    let early = msgs.pop().expect("the relay's 200");
    msgs.insert(5, Msg { at_us: 2_300, ..early });
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    let relay = f.iter().find(|f| f.emitter == RELAY).expect("the relay's occasion");
    assert!(matches!(relay.decision, Decision::Violated(_)), "{relay:?}");
}

/// A relay whose 200 carries another description than the far 200's answers
/// on its own account, as far as the wire shows: violated.
#[test]
fn a_relays_200_with_another_description_is_violated() {
    let mut msgs = relayed_crossing(2_050);
    let last = msgs.pop().expect("the relay's 200");
    msgs.push(with_body(b"v=0\r\no=relay 1 1 IN IP4 10.0.0.3\r\n", last));
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    let relay = f.iter().find(|f| f.emitter == RELAY).expect("the relay's occasion");
    assert!(matches!(relay.decision, Decision::Violated(_)), "{relay:?}");
}

/// A far UAS that answers 200 before any provisional leaves its relay no
/// CANCEL to send (§9.1): the relay that forwards that 200 is compliant.
#[test]
fn a_relayed_200_drawn_before_any_provisional_is_compliant() {
    let mut msgs = relayed_crossing(2_050);
    msgs.remove(4);
    msgs.remove(2);
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    let relay = f.iter().find(|f| f.emitter == RELAY).expect("the relay's occasion");
    assert!(matches!(relay.decision, Decision::Compliant), "{relay:?}");
}

/// A far 200 that reaches the relay before it relayed the CANCEL it took, the
/// relay's turn running late (650 ms here), beat that CANCEL, which then had
/// no transaction to cancel: the relay that forwards that 200 is compliant.
#[test]
fn a_relayed_200_that_beat_the_relays_cancel_is_compliant() {
    let mut msgs = relayed_crossing(650_000);
    msgs.remove(4);
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    let relay = f.iter().find(|f| f.emitter == RELAY).expect("the relay's occasion");
    assert!(matches!(relay.decision, Decision::Compliant), "{relay:?}");
}

/// A relay that never CANCELled its far leg, the far 200 coming past 64·T1
/// after the CANCEL (the canceller's own horizon, §9.1), answers its UAC's
/// cancelled INVITE 200 on its own account: violated.
#[test]
fn a_200_past_64_t1_after_the_cancel_with_no_relayed_cancel_is_violated() {
    let mut msgs = relayed_crossing(33_000_000);
    msgs.remove(4);
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    let relay = f.iter().find(|f| f.emitter == RELAY).expect("the relay's occasion");
    assert!(matches!(relay.decision, Decision::Violated(_)), "{relay:?}");
}

/// An endpoint that answers its cancelled INVITE 200 while it has an
/// unrelated call's INVITE out, whose far UAS answered 200 before any
/// provisional, is no relay of that answer: violated.
#[test]
fn another_calls_2xx_excuses_no_200_after_the_cancel() {
    let far_2 = "10.0.0.5:5060";
    let msgs = vec![
        between(UAC, RELAY, "c1", req(1_000, "INVITE")),
        between(RELAY, UAC, "c1", rsp(1_100, 100)),
        on("z9hG4bK-3", between(RELAY, far_2, "c9", req(1_500, "INVITE"))),
        with_body(
            b"v=0\r\no=c9 1 1 IN IP4 10.0.0.5\r\n",
            on("z9hG4bK-3", between(far_2, RELAY, "c9", rsp(1_800, 200))),
        ),
        between(UAC, RELAY, "c1", req(2_000, "CANCEL")),
        with_body(FAR_SDP, between(RELAY, UAC, "c1", rsp(2_300, 200))),
    ];
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    let relay = f.iter().find(|f| f.emitter == RELAY).expect("the relay's occasion");
    assert!(matches!(relay.decision, Decision::Violated(_)), "{relay:?}");
}
