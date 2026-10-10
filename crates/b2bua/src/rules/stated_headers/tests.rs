use super::*;
use crate::effects::{HandlerEffects, OutboundTxnMode, Provenance};
use crate::router::test_support::{invite, src};

fn lines(v: &[&str]) -> Vec<String> {
    v.iter().map(|l| l.to_string()).collect()
}

fn map(entries: &[(&str, HeaderUpdate)]) -> SipHeaderUpdates {
    entries.iter().map(|(n, u)| (n.to_string(), u.clone())).collect()
}

/// What `sets` change on a message carrying `carried` (name → lines):
/// `None` where nothing changes, else the names edited and their new lines.
fn edits(
    sets: &[SipHeaderUpdates],
    carried: &[(&str, &[&str])],
) -> Option<Vec<(String, Vec<String>)>> {
    let sets: Vec<&SipHeaderUpdates> = sets.iter().collect();
    let carried_of = |name: HeaderName| -> Vec<String> {
        carried
            .iter()
            .filter(|(n, _)| HeaderName::from(*n).matches(name.as_wire_str()))
            .flat_map(|(_, l)| lines(l))
            .collect()
    };
    restated(&sets, carried_of, Vec::new)
}

/// A draft recording the edits it receives, by name.
impl Draft for Vec<(String, Vec<String>)> {
    fn remove_all(mut self, name: &HeaderName) -> Self {
        self.push((name.as_wire_str().to_string(), vec![]));
        self
    }
    fn push_line(mut self, _name: HeaderName, line: String) -> Self {
        if let Some(last) = self.last_mut() {
            last.1.push(line);
        }
        self
    }
}

#[test]
fn a_set_replaces_every_line_and_is_idempotent() {
    let set = map(&[("X-A", HeaderUpdate::Set(lines(&["1", "2"])))]);
    assert_eq!(
        edits(std::slice::from_ref(&set), &[("x-a", &["old"])]),
        Some(vec![("X-A".to_string(), lines(&["1", "2"]))])
    );
    assert_eq!(edits(std::slice::from_ref(&set), &[("X-A", &["1", "2"])]), None);
    assert_eq!(
        edits(&[set], &[("X-A", &["2", "1"])]),
        Some(vec![("X-A".to_string(), lines(&["1", "2"]))]),
        "line order is the statement"
    );
}

#[test]
fn a_removal_takes_every_line_off_and_leaves_a_message_without_one() {
    for removal in [HeaderUpdate::Remove, HeaderUpdate::Set(vec![])] {
        let set = map(&[("X-A", removal)]);
        assert_eq!(
            edits(std::slice::from_ref(&set), &[("X-A", &["a", "b"])]),
            Some(vec![("X-A".to_string(), vec![])])
        );
        assert_eq!(edits(&[set], &[]), None);
    }
}

#[test]
fn an_add_rides_only_a_message_carrying_none() {
    let set = map(&[("X-A", HeaderUpdate::Add(lines(&["add"])))]);
    assert_eq!(
        edits(std::slice::from_ref(&set), &[]),
        Some(vec![("X-A".to_string(), lines(&["add"]))])
    );
    assert_eq!(edits(&[set], &[("x-a", &["own"])]), None, "the carried line stands");
    assert_eq!(edits(&[map(&[("X-A", HeaderUpdate::Add(vec![]))])], &[]), None);
}

#[test]
fn a_header_bound_to_the_stack_or_the_transaction_is_never_restated() {
    for name in ["Via", "Contact", "CSeq", "Require", "RSeq", "Content-Type", "Supported"] {
        assert_eq!(edits(&[map(&[(name, HeaderUpdate::line("x"))])], &[]), None, "{name}");
    }
}

#[test]
fn the_wider_set_applies_first_and_the_narrower_against_its_result() {
    let every = map(&[
        ("X-A", HeaderUpdate::line("every")),
        ("X-B", HeaderUpdate::Remove),
        ("X-C", HeaderUpdate::Add(lines(&["every"]))),
    ]);
    let narrow = map(&[
        ("x-a", HeaderUpdate::line("narrow")),
        ("x-b", HeaderUpdate::Add(lines(&["narrow"]))),
        ("x-c", HeaderUpdate::Remove),
    ]);
    let got = edits(&[every, narrow], &[("X-B", &["carried"])]).unwrap();
    assert_eq!(
        got,
        vec![("X-A".to_string(), lines(&["narrow"])), ("X-B".to_string(), lines(&["narrow"])),],
        "X-C: added by the wider set, removed by the narrower — left as carried"
    );
}

/// A message retained before the turn's end and changed by the end-of-turn
/// stamp — its set replaced later in the turn — is retained as it leaves.
#[test]
fn the_end_of_turn_stamp_rebinds_a_retained_copy_of_what_it_changes() {
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let ids = sip_txn::IdGen::seeded(1);
    let mut call = crate::initial_invite::build_initial_call(
        &invite("w0", "w1", "stamp"),
        src(),
        &config,
        &ids,
        0,
    );
    let a_invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
    let ack = a_invite.clone();
    let (emission, _) = call::RetainedEmission::paced(
        ack.image().to_vec(),
        ("192.0.2.1".into(), 5060),
        sip_retransmit::Class::Final2xx,
        call::Repeated::response("INVITE", 200),
    );
    call.reliable_provisionals.push(call::ReliableProvisional {
        a_tag: "t".into(),
        a_rseq: 1,
        b_leg_id: "b-1".into(),
        b_tag: "u".into(),
        b_cseq: 1,
        b_rseq: 1,
        acknowledged: false,
        emission: Some(emission),
        a_cseq: 1,
        carried_sdp: false,
        responder_sdp: false,
        responder_offer: None,
    });
    let mut stated = StatedHeaders::default();
    stated.every_message.insert("X-Late".into(), HeaderUpdate::line("late"));
    let mut features = crate::decision::default_platform_features();
    features.stated_headers = Some(stated);
    call.features = Some(features);
    let mut fx = HandlerEffects::new();
    fx.outbound.push(OutboundSipEffect {
        body: OutboundBody::Request(ack),
        mode: OutboundTxnMode::Raw,
        destination: ("192.0.2.1".into(), 5060),
        label: "retained".into(),
        leg_id: Some("a".into()),
        provenance: Provenance::Authored,
    });
    let result = stamp_outbound(HandlerResult { call, effects: fx });
    let OutboundBody::Request(sent) = &result.effects.outbound[0].body else { unreachable!() };
    assert_eq!(sent.raw(HeaderName::from("X-Late")).collect::<Vec<_>>(), ["late"]);
    let retained = result.call.reliable_provisionals[0].emission.as_ref().unwrap();
    assert_eq!(retained.wire().0, sent.image().as_ref(), "the retained copy is what left");
}

/// The launched INVITE the end-of-turn stamp changes is the one the leg's
/// client-transaction handle holds: a takeover seeds the client INVITE
/// from that handle, and a retransmission of a request is the request it
/// repeats (RFC 3261 §17.1.1.2).
#[test]
fn the_end_of_turn_stamp_rebinds_the_launched_invites_handle() {
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let ids = sip_txn::IdGen::seeded(1);
    let mut call = crate::initial_invite::build_initial_call(
        &invite("w0", "w1", "handle"),
        src(),
        &config,
        &ids,
        0,
    );
    let a_invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
    let (leg, effect) = crate::rules::relay::build_b_leg(
        crate::rules::relay::CallMarks::of(&call),
        "b-1",
        &a_invite,
        ("192.0.2.9".into(), 5060),
        None,
        None,
        None,
        &config,
        &ids,
        None,
        &[],
        &sip_message::generators::CapabilitySet::default(),
        None,
        &[],
        &[],
        None,
        true,
        0,
    )
    .expect("the leg is built");
    call.b_legs.push(leg);
    let mut stated = StatedHeaders::default();
    stated.every_message.insert("X-Stamp".into(), HeaderUpdate::line("s"));
    let mut features = crate::decision::default_platform_features();
    features.stated_headers = Some(stated);
    call.features = Some(features);
    let mut fx = HandlerEffects::new();
    fx.outbound.push(effect);
    let result = stamp_outbound(HandlerResult { call, effects: fx });
    let OutboundBody::Request(sent) = &result.effects.outbound[0].body else { unreachable!() };
    assert_eq!(sent.raw(HeaderName::from("X-Stamp")).collect::<Vec<_>>(), ["s"]);
    let leg = &result.call.b_legs[0];
    let held =
        leg.outbound_invite.as_ref().and_then(|i| i.txn.as_ref()).expect("the INVITE handle");
    assert_eq!(held.original_invite, sent.image().to_vec(), "the handle holds what left");
    for d in &leg.dialogs {
        if let Some(h) = &d.pending_invite_txn {
            assert_eq!(h.original_invite, sent.image().to_vec());
        }
    }
}

/// A leg's own set stays with the call once the leg has ended: an ended
/// leg may still owe messages (a 200 crossing its CANCEL, a BYE's answer).
#[test]
fn an_ended_legs_own_set_stays_with_the_call() {
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let ids = sip_txn::IdGen::seeded(1);
    let mut call = crate::initial_invite::build_initial_call(
        &invite("w0", "w1", "prune"),
        src(),
        &config,
        &ids,
        0,
    );
    let a_invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
    let (mut leg, effect) = crate::rules::relay::build_b_leg(
        crate::rules::relay::CallMarks::of(&call),
        "b-1",
        &a_invite,
        ("192.0.2.9".into(), 5060),
        None,
        None,
        None,
        &config,
        &ids,
        None,
        &[],
        &sip_message::generators::CapabilitySet::default(),
        None,
        &[],
        &[],
        None,
        true,
        0,
    )
    .expect("the leg is built");
    call::testkit::stage_leg(&mut leg, call::LegState::Terminated);
    call.b_legs.push(leg);
    let mut stated = StatedHeaders::default();
    let mut own = StatedHeaders::default();
    own.every_message.insert("X-Own".into(), HeaderUpdate::line("leg"));
    stated.legs.insert("b-1".into(), own);
    stated.every_message.insert("X-Call".into(), HeaderUpdate::line("call"));
    let mut features = crate::decision::default_platform_features();
    features.stated_headers = Some(stated);
    call.features = Some(features);
    let mut fx = HandlerEffects::new();
    fx.outbound.push(effect);
    let result = stamp_outbound(HandlerResult { call, effects: fx });
    let OutboundBody::Request(sent) = &result.effects.outbound[0].body else { unreachable!() };
    assert_eq!(sent.raw(HeaderName::from("X-Own")).collect::<Vec<_>>(), ["leg"]);
    let left = result.call.features.unwrap().stated_headers.unwrap();
    assert_eq!(left.legs.len(), 1, "the ended leg's set stays: {left:?}");
    assert_eq!(left.every_message.len(), 1, "the call's set stays");
}

/// The charging arm reads the set of the leg it mints for: a leg whose own
/// set states the vector mints none, whatever the call's set says, and one
/// whose own set says nothing of it mints, whatever the call's set says.
#[test]
fn the_charging_arm_reads_the_legs_own_set() {
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let ids = sip_txn::IdGen::seeded(1);
    let mut call = crate::initial_invite::build_initial_call(
        &invite("w0", "w1", "arm"),
        src(),
        &config,
        &ids,
        0,
    );
    let mut stated = StatedHeaders::default();
    stated.every_message.insert("P-Charging-Vector".into(), HeaderUpdate::line("call"));
    let mut silent = StatedHeaders::default();
    silent.every_message.insert("X-Other".into(), HeaderUpdate::line("x"));
    stated.legs.insert("b-1".into(), silent);
    let mut stating = StatedHeaders::default();
    stating.every_message.insert("P-Charging-Vector".into(), HeaderUpdate::Add(vec!["t".into()]));
    stated.legs.insert("b-2".into(), stating);
    let mut features = crate::decision::default_platform_features();
    features.charging_vector = Some(call::features::ChargingVectorFeature::default());
    features.stated_headers = Some(stated.clone());
    call.features = Some(features);
    assert!(crate::rules::charging::minting_arm(&call, "b-1", None).is_some());
    assert!(crate::rules::charging::minting_arm(&call, "b-2", None).is_none());
    assert!(crate::rules::charging::minting_arm(&call, "b-3", None).is_none(), "the call's");
}
