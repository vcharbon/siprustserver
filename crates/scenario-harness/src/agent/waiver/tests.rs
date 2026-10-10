use super::*;

fn entry(from_lane: &str) -> RecordedSipEntry {
    RecordedSipEntry {
        from: "10.0.0.9:5070".parse().unwrap(),
        to: "10.0.0.1:5060".parse().unwrap(),
        raw: b"OPTIONS sip:x@h SIP/2.0\r\n\r\n".to_vec(),
        sent_ms: 0,
        received_ms: Some(1),
        delivered: true,
        recv_note: None,
        reemit: None,
        from_lane: Some(from_lane.to_string()),
        to_lane: None,
        seq: 0,
    }
}

fn finding(rule: &str, offending: usize) -> RfcFinding {
    RfcFinding {
        rule: rule.into(),
        lane: "x".into(),
        detail: "d".into(),
        advisory: false,
        offending: Some(offending),
        charged: None,
    }
}

const RULE: &str = "cseq-in-dialog-order";

fn covered(
    w: &WaiverScope,
    f: &RfcFinding,
    entries: &[RecordedSipEntry],
    attr: &Attribution,
) -> bool {
    covers(w, f, finding_party(f, entries, attr).as_deref(), attr)
}

/// ADR-0024 §6 load-lane specific: sub-lane party attribution. Two logical
/// legs share ONE mux socket; a party-scoped waiver covers ONLY the leg it
/// names, never its co-socketed sibling — because attribution resolves the
/// `ip:port#name` sub-lane key, not the addr.
#[test]
fn sublane_party_scope_isolates_co_socketed_siblings() {
    let entries = vec![entry("10.0.0.9:5070#bob"), entry("10.0.0.9:5070#bob2")];
    let bob = finding(RULE, 1); // offending = entry #1 (bob)
    let bob2 = finding(RULE, 2); // offending = entry #2 (bob2, same socket)
    let w = WaiverScope::rule(RULE, "only bob").on_party("bob");
    let attr = Attribution::SubLane;
    assert!(covered(&w, &bob, &entries, &attr), "covers bob's finding");
    assert!(
        !covered(&w, &bob2, &entries, &attr),
        "does NOT cover the co-socketed sibling bob2's finding",
    );
}

/// The plain addr→first-name map (the functional attribution) would
/// mis-attribute BOTH co-socketed legs to the first bind — the exact hazard
/// the load lane's sub-lane resolution avoids.
#[test]
fn addr_map_mis_attributes_co_socketed_legs() {
    let entries = vec![entry("10.0.0.9:5070#bob"), entry("10.0.0.9:5070#bob2")];
    let mut map = HashMap::new();
    map.insert("10.0.0.9:5070".parse().unwrap(), "bob".to_string());
    let attr = Attribution::AddrNames { names: &map, sut: &HashSet::new() };
    let w = WaiverScope::rule(RULE, "bob").on_party("bob");
    assert!(covered(&w, &finding(RULE, 1), &entries, &attr));
    assert!(
        covered(&w, &finding(RULE, 2), &entries, &attr),
        "addr map wrongly covers the sibling (both resolve to the first bind)",
    );
}

/// A party-scoped waiver on a DIFFERENT party (a SUT lane, an unnamed key)
/// never covers — the finding stays gated.
#[test]
fn sublane_party_scope_gates_other_parties() {
    let entries = vec![entry("10.0.0.9:5070#bob")];
    let attr = Attribution::SubLane;
    assert!(!covered(
        &WaiverScope::rule(RULE, "proxy").on_party("proxy"),
        &finding(RULE, 1),
        &entries,
        &attr,
    ));
}

/// An omission is charged to the party that OWED the missing message, not
/// to the sender of the message it is anchored on: a waiver naming the
/// owing party covers it, one naming the anchor's sender does not.
#[test]
fn an_omission_is_attributed_to_the_party_that_owed_it() {
    let entries = vec![entry("10.0.0.9:5070")]; // the anchor: the B2BUA's provisional
    let mut map = HashMap::new();
    map.insert("10.0.0.9:5070".parse().unwrap(), "b2bua".to_string());
    map.insert("10.0.0.1:5060".parse().unwrap(), "alice".to_string());
    let attr = Attribution::AddrNames { names: &map, sut: &HashSet::new() };
    let owed = RfcFinding { charged: Some("10.0.0.1:5060".into()), ..finding(RULE, 1) };
    let alice = WaiverScope::rule(RULE, "alice never PRACKs").on_party("alice");
    let b2bua = WaiverScope::rule(RULE, "wrong party").on_party("b2bua");
    assert!(covered(&alice, &owed, &entries, &attr), "the owing party's waiver covers it");
    assert!(!covered(&b2bua, &owed, &entries, &attr), "the anchor's sender owed nothing");
}

/// A rule judged at the taker charges the sender by the bare source
/// address it saw: on the load lane that address resolves through the
/// offending entry's sub-lane at either end, so the sender's waiver
/// covers it, an omission owed by the entry's taker is the taker's, and a
/// co-socketed sibling's waiver covers neither.
#[test]
fn sublane_bare_charged_address_resolves_through_the_offending_entry() {
    let mut to_bob = entry("10.0.0.1:5060#alice");
    to_bob.to_lane = Some("10.0.0.9:5070#bob".into());
    let mut from_bob2 = entry("10.0.0.9:5070#bob2");
    from_bob2.to_lane = Some("10.0.0.1:5060#alice".into());
    let entries = vec![to_bob, from_bob2];
    let attr = Attribution::SubLane;
    let by_sender = RfcFinding { charged: Some("10.0.0.1:5060".into()), ..finding(RULE, 1) };
    let by_taker = RfcFinding { charged: Some("10.0.0.9:5070".into()), ..finding(RULE, 1) };
    let by_bob2 = RfcFinding { charged: Some("10.0.0.9:5070".into()), ..finding(RULE, 2) };
    let alice = WaiverScope::rule(RULE, "alice").on_party("alice");
    let bob = WaiverScope::rule(RULE, "bob").on_party("bob");
    let bob2 = WaiverScope::rule(RULE, "bob2").on_party("bob2");
    assert!(covered(&alice, &by_sender, &entries, &attr), "the sender's waiver covers it");
    assert!(!covered(&bob, &by_sender, &entries, &attr), "the taker owed nothing");
    assert!(covered(&bob, &by_taker, &entries, &attr), "the owing taker's waiver covers it");
    assert!(!covered(&bob2, &by_taker, &entries, &attr), "a co-socketed sibling never does");
    assert!(covered(&bob2, &by_bob2, &entries, &attr), "the sibling's own entry is its own");
    assert!(!covered(&bob, &by_bob2, &entries, &attr), "and never bob's");
}

/// Two legs on one socket talking to each other: a bare charged address
/// names both ends of the offending entry, so it resolves to no party and
/// no party-scoped waiver covers the finding.
#[test]
fn sublane_bare_charged_address_naming_both_ends_stays_gated() {
    let mut ok = entry("127.0.0.1:5060#bob"); // bob's 200, alice never ACKs it
    ok.to_lane = Some("127.0.0.1:5060#alice".into());
    let entries = vec![ok];
    let attr = Attribution::SubLane;
    let unacked = RfcFinding { charged: Some("127.0.0.1:5060".into()), ..finding(RULE, 1) };
    let alice = WaiverScope::rule(RULE, "alice").on_party("alice");
    let bob = WaiverScope::rule(RULE, "bob").on_party("bob");
    assert!(!covered(&bob, &unacked, &entries, &attr), "the sender is not the charged party");
    assert!(!covered(&alice, &unacked, &entries, &attr), "nor is it known to be the taker");
}

/// A rule-only waiver covers by rule alone (a plain rule-name filter)
/// regardless of attribution.
#[test]
fn rule_only_covers_by_rule_regardless_of_party() {
    let entries = vec![entry("10.0.0.9:5070#bob")];
    let attr = Attribution::SubLane;
    assert!(covered(&WaiverScope::rule(RULE, "coarse"), &finding(RULE, 1), &entries, &attr,));
    // A different rule is never covered.
    assert!(!covered(
        &WaiverScope::rule("rfc3261.other", "coarse"),
        &finding(RULE, 1),
        &entries,
        &attr,
    ));
}

/// A party-less waiver covers a simulated party's finding, never one
/// charged to a lane the harness bound as SUT nor an unattributed one; a
/// waiver naming the SUT lane covers the SUT's.
#[test]
fn a_party_less_waiver_never_covers_the_sut() {
    let entries = vec![entry("10.0.0.9:5070"), entry("10.0.0.1:5060")];
    let mut map = HashMap::new();
    map.insert("10.0.0.9:5070".parse().unwrap(), "sut".to_string());
    map.insert("10.0.0.1:5060".parse().unwrap(), "alice".to_string());
    let sut = HashSet::from(["sut".to_string()]);
    let attr = Attribution::AddrNames { names: &map, sut: &sut };
    let coarse = WaiverScope::rule(RULE, "coarse");
    let by_sut = finding(RULE, 1);
    let by_alice = finding(RULE, 2);
    let unattributed = RfcFinding { offending: None, ..finding(RULE, 1) };
    assert!(covered(&coarse, &by_alice, &entries, &attr), "a simulated party's finding");
    assert!(!covered(&coarse, &by_sut, &entries, &attr), "never the SUT's");
    assert!(!covered(&coarse, &unattributed, &entries, &attr), "nor an unattributed one");
    let named = WaiverScope::rule(RULE, "accepted").on_party("sut");
    assert!(covered(&named, &by_sut, &entries, &attr), "a waiver naming the SUT covers it");
}

/// The waived SUT findings are counted per rule; a waived simulated
/// party's finding is not.
#[test]
fn waived_sut_findings_are_counted_per_rule() {
    let entries = vec![entry("10.0.0.9:5070"), entry("10.0.0.1:5060")];
    let mut map = HashMap::new();
    map.insert("10.0.0.9:5070".parse().unwrap(), "sut".to_string());
    map.insert("10.0.0.1:5060".parse().unwrap(), "alice".to_string());
    let sut = HashSet::from(["sut".to_string()]);
    let attr = Attribution::AddrNames { names: &map, sut: &sut };
    let waivers = [
        WaiverState::new(WaiverScope::rule(RULE, "accepted").on_party("sut")),
        WaiverState::new(WaiverScope::rule(RULE, "alice")),
    ];
    let findings = [finding(RULE, 1), finding(RULE, 1), finding(RULE, 2)];
    let out = waive(&findings, &entries, &waivers, &attr);
    assert!(out.gating.is_empty(), "every finding is waived");
    assert_eq!(out.simulated, BTreeMap::from([(RULE.to_string(), 1)]));
    assert_eq!(out.sut, BTreeMap::from([(RULE.to_string(), 2)]));
}
