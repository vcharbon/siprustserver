use pivot_schema::placement::{Binding, Endpoint, Side};

use super::*;

fn finding(
    rule: &str,
    advisory: bool,
    offending: Option<usize>,
    charged: Option<&str>,
) -> sip_net_testkit::RfcFinding {
    sip_net_testkit::RfcFinding {
        rule: rule.into(),
        lane: "127.0.0.1:5060".into(),
        detail: format!("{rule} fired"),
        advisory,
        offending,
        charged: charged.map(str::to_string),
        counterpart: None,
    }
}

fn entry(from: &str) -> RecordedSipEntry {
    RecordedSipEntry {
        from: from.parse().unwrap(),
        to: "127.0.0.1:5060".parse().unwrap(),
        raw: b"OPTIONS sip:x@h SIP/2.0\r\n\r\n".to_vec(),
        sent_ms: 0,
        received_ms: Some(1),
        delivered: true,
        recv_note: None,
        reemit: None,
        from_lane: None,
        to_lane: None,
        seq: 0,
    }
}

/// A document whose statements are a lookup: actors named per endpoint,
/// the statement cancelling a (rule, actor), offending bytes ignored, and
/// the reason it gives for never reading its captured trace.
#[derive(Default)]
struct Stated {
    legs: BTreeMap<String, Vec<RecordedMessage>>,
    actors: BTreeMap<&'static str, &'static str>,
    cancels: BTreeMap<(&'static str, &'static str), &'static str>,
    unread: Option<&'static str>,
}

impl Statements for Stated {
    fn actor_named(&self, endpoint: &str, _offending: &[u8]) -> Option<String> {
        self.actors.get(endpoint).map(|a| a.to_string())
    }
    fn cancelled_by(&self, rule: &str, actor: &str, _offending: &[u8]) -> Option<String> {
        self.cancels.get(&(rule, actor)).map(|c| c.to_string())
    }
    fn relayed(&self, rule: &str, _offending: &[u8]) -> Option<crate::cancellation::Relayed> {
        self.cancels.get(&(rule, "relayed")).map(|by| crate::cancellation::Relayed {
            label: format!("relayed: {rule} originated by {by}"),
            caused_by: by.to_string(),
        })
    }
    fn unread(&self) -> Option<String> {
        self.unread.map(str::to_string)
    }
    fn legs(&self) -> &BTreeMap<String, Vec<RecordedMessage>> {
        &self.legs
    }
}

/// Nothing named, nothing cancelled.
fn uncancelled() -> Stated {
    Stated::default()
}

/// Only the system under test's hard-gate finding gates — its own, and
/// one attributable to nobody. The scripted peer's is named as that
/// peer's and is a source fault, never the SUT's; an advisory one counts
/// for nothing.
#[test]
fn only_the_systems_hard_gate_finding_gates_and_a_peers_is_a_source_fault() {
    let sut = finding("cseq-in-dialog-order", false, Some(1), Some("127.0.0.1:5080"));
    let peer = finding("cseq-in-dialog-order", false, Some(2), Some("127.0.0.1:5071"));
    let advisory = finding("rport-echo", true, Some(1), Some("127.0.0.1:5080"));
    let nobody = finding("no-target-404", false, None, None);
    let all = vec![sut.clone(), peer.clone(), advisory, nobody.clone()];
    let entries = vec![entry("127.0.0.1:5080"), entry("127.0.0.1:5071")];
    let actors = actors(
        &peers(),
        &[&BTreeMap::from([("ep1".to_string(), "127.0.0.1:5071".parse().unwrap())])],
    );
    let audit = audited(&all, &[sut, peer, nobody], &entries, &actors, &uncancelled());
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings.iter().map(|f| f.gating).collect::<Vec<_>>(), [true, false, false, true]);
    assert_eq!(
        findings.iter().map(|f| f.source_fault).collect::<Vec<_>>(),
        [false, true, false, false]
    );
    assert_eq!(findings[0].charged.as_deref(), Some("127.0.0.1:5080"));
    assert_eq!(findings[1].actor.as_deref(), Some("ep1"));
    assert!(!findings[1].advisory);
    assert_eq!((audit.gating(), audit.source_faults()), (2, 1));
}

/// A scripted peer's finding the document states on its transaction is
/// cancelled: written with what cancelled it, and no longer gating. The
/// lookup is asked about the actor's own endpoint and the offending bytes.
#[test]
fn a_stated_peer_violation_is_cancelled_and_says_by_what() {
    let peer = finding("cseq-in-dialog-order", false, Some(1), Some("127.0.0.1:5071"));
    let entries = vec![entry("127.0.0.1:5071")];
    let actors = actors(
        &peers(),
        &[&BTreeMap::from([("ep1".to_string(), "127.0.0.1:5071".parse().unwrap())])],
    );
    let stated = Stated {
        actors: BTreeMap::from([("ep1", "uas1")]),
        cancels: BTreeMap::from([(
            ("cseq-in-dialog-order", "uas1"),
            "rfc_violations: cseq-in-dialog-order at s4 by uas1",
        )]),
        ..Stated::default()
    };
    let audit = audited(
        std::slice::from_ref(&peer),
        std::slice::from_ref(&peer),
        &entries,
        &actors,
        &stated,
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert!(!findings[0].gating);
    assert!(!findings[0].source_fault, "the captured trace shows it");
    assert_eq!(findings[0].actor.as_deref(), Some("uas1"), "named by the actor");
    assert_eq!(
        findings[0].cancelled_by.as_deref(),
        Some("rfc_violations: cseq-in-dialog-order at s4 by uas1")
    );
    assert!(audit.passed(), "listed, and the cell is ok");
}

/// A cancellation is a scripted party's alone: the SUT's finding is never
/// looked up, whatever the document states.
#[test]
fn the_systems_own_finding_is_never_cancelled() {
    let sut = finding("cseq-in-dialog-order", false, Some(1), Some("127.0.0.1:5080"));
    let entries = vec![entry("127.0.0.1:5080")];
    let audit = audited(
        std::slice::from_ref(&sut),
        std::slice::from_ref(&sut),
        &entries,
        &Actors::default(),
        &Stated {
            actors: BTreeMap::from([("127.0.0.1:5080", "sut")]),
            cancels: BTreeMap::from([(("cseq-in-dialog-order", "sut"), "anything")]),
            unread: Some("the captured trace's violations were never read"),
            ..Stated::default()
        },
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert!(findings[0].gating);
    assert_eq!(findings[0].cancelled_by, None);
    assert_eq!(findings[0].unjudged, None, "an unread trace never excuses the SUT");
    assert!(!audit.passed());
}

/// The run's caller (ep0 at 127.0.0.1:5060, actor uac1) and the SUT both
/// break `payload-type-mapping-stable`; the document states the caller's
/// violation and the SUT relaying it.
fn relaying_run() -> (Vec<sip_net_testkit::RfcFinding>, Vec<RecordedSipEntry>, Stated) {
    let rule = "payload-type-mapping-stable";
    let party = finding(rule, false, Some(1), Some("127.0.0.1:5060"));
    let sut = finding(rule, false, Some(2), Some("127.0.0.1:5080"));
    let entries = vec![entry("127.0.0.1:5060"), entry("127.0.0.1:5080")];
    let stated = Stated {
        actors: BTreeMap::from([("ep0", "uac1")]),
        cancels: BTreeMap::from([
            ((rule, "uac1"), "rfc_violations: payload-type-mapping-stable at s2 by uac1"),
            ((rule, "relayed"), "uac1"),
        ]),
        ..Stated::default()
    };
    (vec![party, sut], entries, stated)
}

/// A document endpoint `id` on `side`, its observed socket unread here.
fn endpoint(id: &str, side: Side) -> Endpoint {
    Endpoint { id: id.into(), observed: String::new(), side, binding: Binding::Dedicated }
}

/// The scripted peers the tests' documents bind, `ep0` and `ep1`.
fn peers() -> Vec<Endpoint> {
    vec![endpoint("ep0", Side::Peer), endpoint("ep1", Side::Peer)]
}

/// A SUT-side endpoint names no actor, though the lane maps it to an address:
/// a finding charged there is the system under test's, and gates.
#[test]
fn a_sut_side_endpoint_never_makes_a_finding_scripted() {
    let sut = finding("cseq-in-dialog-order", false, Some(1), Some("127.0.0.1:5080"));
    let endpoints = [endpoint("ep0", Side::Peer), endpoint("sut0", Side::Sut)];
    let actors = actors(
        &endpoints,
        &[&BTreeMap::from([
            ("ep0".to_string(), "127.0.0.1:5060".parse().unwrap()),
            ("sut0".to_string(), "127.0.0.1:5080".parse().unwrap()),
        ])],
    );
    assert_eq!(actors.ids.iter().collect::<Vec<_>>(), ["ep0"]);
    let audit = audited(
        std::slice::from_ref(&sut),
        std::slice::from_ref(&sut),
        &[entry("127.0.0.1:5080")],
        &actors,
        &uncancelled(),
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].actor, None);
    assert!(findings[0].gating && !findings[0].source_fault, "{:#?}", findings[0]);
}

fn caller_endpoint() -> Actors {
    actors(&peers(), &[&BTreeMap::from([("ep0".to_string(), "127.0.0.1:5060".parse().unwrap())])])
}

/// The SUT's finding the document states as relayed from a party, where
/// the run shows that party breaking the rule too and its own statement
/// cancelling it, stops gating, says what cancelled it and which party
/// caused it; the party's finding is never read as relayed.
#[test]
fn the_systems_relayed_finding_is_cancelled_as_caused_by_its_origin() {
    let (all, entries, stated) = relaying_run();
    let audit = audited(&all, &all, &entries, &caller_endpoint(), &stated);
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].actor.as_deref(), Some("uac1"));
    assert_eq!(findings[0].caused_by, None, "the party's own finding");
    assert!(!findings[0].gating, "cancelled by the party's own statement");
    assert!(!findings[1].gating);
    assert_eq!(findings[1].caused_by.as_deref(), Some("uac1"));
    assert_eq!(
        findings[1].cancelled_by.as_deref(),
        Some("relayed: payload-type-mapping-stable originated by uac1")
    );
    assert_eq!(audit.gating(), 0);
}

/// The document's relaying statement is not enough on its own: where the
/// run holds no finding of the rule against the stated party — it did not
/// commit the violation this time — the SUT's finding is its own and gates.
#[test]
fn a_relayed_finding_whose_party_did_not_break_the_rule_in_the_run_gates() {
    let (all, entries, stated) = relaying_run();
    let sut_only = &all[1..];
    let audit = audited(sut_only, sut_only, &entries, &caller_endpoint(), &stated);
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert!(findings[0].gating, "{findings:#?}");
    assert_eq!(
        (findings[0].caused_by.as_deref(), findings[0].cancelled_by.as_deref()),
        (None, None)
    );

    // Nor where the party's finding stands uncancelled: it is the party's
    // source fault, and the SUT's copy of an unstated violation gates
    // beside it.
    let uncancelled = Stated {
        actors: BTreeMap::from([("ep0", "uac1")]),
        cancels: BTreeMap::from([(("payload-type-mapping-stable", "relayed"), "uac1")]),
        ..Stated::default()
    };
    let audit = audited(&all, &all, &entries, &caller_endpoint(), &uncancelled);
    assert_eq!((audit.gating(), audit.source_faults()), (1, 1));
}

/// An omission is charged to the party that OWED the message, not to the
/// sender of the entry the rule points at: a scripted caller's withheld ACK
/// is the caller's, though the offending entry is the SUT's 2xx — and, the
/// document stating it on that transaction, it is cancelled.
#[test]
fn an_omission_is_the_owing_partys_not_the_offending_entrys_senders() {
    let no_ack = finding("no-ack-to-dialog-creating-2xx", false, Some(1), Some("127.0.0.1:5060"));
    let entries = vec![entry("127.0.0.1:5080")];
    let actors = actors(
        &peers(),
        &[&BTreeMap::from([("ep0".to_string(), "127.0.0.1:5060".parse().unwrap())])],
    );
    let stated = Stated {
        actors: BTreeMap::from([("ep0", "uac1")]),
        cancels: BTreeMap::from([(
            ("no-ack-to-dialog-creating-2xx", "uac1"),
            "deviation d1 (suppress-auto) withholds s8",
        )]),
        ..Stated::default()
    };
    let audit = audited(
        std::slice::from_ref(&no_ack),
        std::slice::from_ref(&no_ack),
        &entries,
        &actors,
        &stated,
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].actor.as_deref(), Some("uac1"));
    assert!(!findings[0].gating);
    assert_eq!(audit.gating(), 0);
}

/// A scripted peer's violation is reported as that peer's, and unless the
/// captured trace holds the same violation on the same transaction it is a
/// SOURCE FAULT: the replayed peer repeats a captured message where, in this
/// run, the RFC no longer allows it, the document does not state it, and the
/// SUT is not blamed for it. Nothing captured cancels this one.
#[test]
fn a_scripted_peers_violation_with_no_captured_counterpart_is_a_source_fault() {
    let withheld = finding("unacked-invite-non-2xx-final", false, Some(1), Some("127.0.0.1:5060"));
    let audit = audited(
        std::slice::from_ref(&withheld),
        std::slice::from_ref(&withheld),
        &[entry("127.0.0.1:5080")],
        &caller_endpoint(),
        &uncancelled(),
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].actor.as_deref(), Some("ep0"), "named as the scripted peer's");
    assert!(!findings[0].gating, "never the SUT's");
    assert!(findings[0].source_fault, "no captured counterpart cancels it");
    assert_eq!(findings[0].unjudged, None);
    assert_eq!((audit.gating(), audit.source_faults()), (0, 1));
    assert!(audit.passed(), "a source fault is information: the audit holds no SUT finding");
}

/// A document that states it never read its captured trace's violations
/// cannot tell a scripted peer's violation the trace shows from one the
/// harness made up: the finding is listed with that reason, for
/// information, and the cell is not failed on it. An advisory or cancelled
/// finding carries no such reason.
#[test]
fn a_scripted_peers_violation_on_a_document_that_never_read_its_trace_is_unjudged() {
    let rule = "no-200-after-cancel";
    let answered = finding(rule, false, Some(1), Some("127.0.0.1:5060"));
    let advisory = finding("rport-echo", true, Some(1), Some("127.0.0.1:5060"));
    let unread = Stated {
        actors: BTreeMap::from([("ep0", "uas1")]),
        unread: Some("census-uncovered"),
        ..Stated::default()
    };
    let audit = audited(
        &[answered.clone(), advisory],
        std::slice::from_ref(&answered),
        &[entry("127.0.0.1:5060")],
        &caller_endpoint(),
        &unread,
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].actor.as_deref(), Some("uas1"));
    assert!(!findings[0].gating && !findings[0].source_fault, "{:#?}", findings[0]);
    assert_eq!(findings[0].unjudged.as_deref(), Some("census-uncovered"));
    assert_eq!(findings[1].unjudged, None, "advisory");
    assert!(audit.passed());

    let stated =
        Stated { cancels: BTreeMap::from([((rule, "uas1"), "rfc_violations: stated")]), ..unread };
    let audit = audited(
        std::slice::from_ref(&answered),
        std::slice::from_ref(&answered),
        &[entry("127.0.0.1:5060")],
        &caller_endpoint(),
        &stated,
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].unjudged, None, "cancelled");
}

/// A rule that names no culprit falls back to the offending entry's sender.
#[test]
fn an_uncharged_finding_falls_back_to_the_offending_entrys_sender() {
    let f = finding("branch-prefix", false, Some(1), None);
    let entries = vec![entry("127.0.0.1:5071")];
    let actors = actors(
        &peers(),
        &[&BTreeMap::from([("ep1".to_string(), "127.0.0.1:5071".parse().unwrap())])],
    );
    let audit = audited(
        std::slice::from_ref(&f),
        std::slice::from_ref(&f),
        &entries,
        &actors,
        &uncancelled(),
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].charged.as_deref(), Some("127.0.0.1:5071"));
    assert_eq!(findings[0].actor.as_deref(), Some("ep1"));
}

/// A lane that binds port `0` charges the ASKED address and listens on the
/// resolved one; either names the endpoint, so the document's statement of
/// its violation cancels it under both.
#[test]
fn an_endpoint_is_named_by_the_address_it_asked_for_and_by_the_one_it_got() {
    let asked =
        finding("ack-body-after-complete-offer-answer", false, Some(1), Some("127.0.0.2:0"));
    let got =
        finding("ack-body-after-complete-offer-answer", false, Some(1), Some("127.0.0.2:41234"));
    let entries = vec![entry("127.0.0.2:41234")];
    let actors = actors(
        &peers(),
        &[
            &BTreeMap::from([("ep0".to_string(), "127.0.0.2:0".parse().unwrap())]),
            &BTreeMap::from([("ep0".to_string(), "127.0.0.2:41234".parse().unwrap())]),
        ],
    );
    let stated = Stated {
        cancels: BTreeMap::from([(
            ("ack-body-after-complete-offer-answer", "ep0"),
            "rfc_violations: stated",
        )]),
        ..Stated::default()
    };
    let audit = audited(&[asked.clone(), got.clone()], &[asked, got], &entries, &actors, &stated);
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].actor.as_deref(), Some("ep0"));
    assert_eq!(findings[1].actor.as_deref(), Some("ep0"));
    assert_eq!(audit.gating(), 0);
}

/// Two endpoints that asked for one address on a lane binding port `0`.
fn shared_address() -> Actors {
    actors(
        &peers(),
        &[&BTreeMap::from([
            ("ep0".to_string(), "127.0.0.2:0".parse().unwrap()),
            ("ep1".to_string(), "127.0.0.2:0".parse().unwrap()),
        ])],
    )
}

/// A labelled bind charges its sub-lane `ip:port#<endpoint>`: the finding is
/// that endpoint's, named by the label, so a scripted caller's withheld ACK
/// stays the caller's although the offending entry is the SUT's 2xx, and its
/// statement cancels it.
#[test]
fn a_sub_lane_charge_names_the_endpoint_its_label_names() {
    let rule = "no-ack-to-dialog-creating-2xx";
    let no_ack = finding(rule, false, Some(1), Some("127.0.0.2:0#ep0"));
    let stated = Stated {
        actors: BTreeMap::from([("ep0", "uac1")]),
        cancels: BTreeMap::from([((rule, "uac1"), "rfc_violations: stated")]),
        ..Stated::default()
    };
    let audit = audited(
        std::slice::from_ref(&no_ack),
        std::slice::from_ref(&no_ack),
        &[entry("127.0.0.2:15200")],
        &shared_address(),
        &stated,
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].charged.as_deref(), Some("127.0.0.2:0#ep0"));
    assert_eq!(findings[0].actor.as_deref(), Some("uac1"));
    assert!(!findings[0].gating, "{:#?}", findings[0]);
    assert_eq!(audit.gating(), 0);
}

/// Where several endpoints share the charged address, the label decides which
/// one the finding is: each sub-lane names its own endpoint, never whichever
/// the address map happens to keep.
#[test]
fn endpoints_sharing_an_address_are_told_apart_by_their_sub_lane() {
    let rule = "unacked-invite-non-2xx-final";
    let charged = [
        finding(rule, false, Some(1), Some("127.0.0.2:0#ep0")),
        finding(rule, false, Some(1), Some("127.0.0.2:0#ep1")),
    ];
    let audit =
        audited(&charged, &charged, &[entry("127.0.0.2:15200")], &shared_address(), &uncancelled());
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(
        findings.iter().map(|f| f.actor.as_deref()).collect::<Vec<_>>(),
        [Some("ep0"), Some("ep1")]
    );
    assert_eq!((audit.gating(), audit.source_faults()), (0, 2));
}

/// A culprit the rule named is never swapped for the offending entry's
/// sender: a charge naming no socket stays the rule's word, written as
/// charged, and gates as the system under test's.
#[test]
fn a_charge_that_names_no_socket_is_not_replaced_by_the_offending_sender() {
    let f = finding("cseq-in-dialog-order", false, Some(1), Some("b2bua"));
    let audit = audited(
        std::slice::from_ref(&f),
        std::slice::from_ref(&f),
        &[entry("127.0.0.1:5060")],
        &caller_endpoint(),
        &uncancelled(),
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].charged.as_deref(), Some("b2bua"));
    assert_eq!(findings[0].actor, None, "the offending entry's sender is ep0's socket");
    assert!(findings[0].gating);
}

/// A plain bind several endpoints share is the endpoint whose leg carries the
/// offending call, whichever of them sorts last.
#[test]
fn a_plain_bind_shared_by_endpoints_is_the_one_whose_leg_carries_the_call() {
    let f = finding("unacked-invite-non-2xx-final", false, Some(1), Some("127.0.0.2:0"));
    let stated = Stated { actors: BTreeMap::from([("ep0", "uac1")]), ..Stated::default() };
    let audit = audited(
        std::slice::from_ref(&f),
        std::slice::from_ref(&f),
        &[entry("127.0.0.2:15200")],
        &shared_address(),
        &stated,
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].actor.as_deref(), Some("uac1"));
}

/// A SUT-side endpoint's sub-lane names no actor, though it shares the socket
/// with scripted peers: the finding is the system under test's, and gates.
#[test]
fn a_sut_side_sub_lane_is_never_a_scripted_party() {
    let f = finding("cseq-in-dialog-order", false, Some(1), Some("127.0.0.2:0#sut0"));
    let endpoints = [endpoint("ep0", Side::Peer), endpoint("sut0", Side::Sut)];
    let actors = actors(
        &endpoints,
        &[&BTreeMap::from([
            ("ep0".to_string(), "127.0.0.2:0".parse().unwrap()),
            ("sut0".to_string(), "127.0.0.2:0".parse().unwrap()),
        ])],
    );
    let stated = Stated { actors: BTreeMap::from([("ep0", "uac1")]), ..Stated::default() };
    let audit = audited(
        std::slice::from_ref(&f),
        std::slice::from_ref(&f),
        &[entry("127.0.0.2:0")],
        &actors,
        &stated,
    );
    let RunRfcAudit::Audited { findings } = &audit else { panic!("audited") };
    assert_eq!(findings[0].actor, None);
    assert!(findings[0].gating, "{:#?}", findings[0]);
}

/// The audit numbers its own wire order; the recording merges legs by instant,
/// and two legs' datagrams may share one. The finding names the message as the
/// recording holds it — its leg and that leg's seq — so a reader resolves the
/// one message the rule meant, not the one at that merged index.
#[test]
fn a_finding_names_its_offending_message_as_the_recording_holds_it() {
    use pivot_schema::bundle::{Dir, MessageRef};
    let invite_a = b"INVITE sip:b SIP/2.0\r\nCall-ID: a\r\nCSeq: 30 INVITE\r\n\r\n".to_vec();
    let trying_a = b"SIP/2.0 100 Trying\r\nCall-ID: a\r\nCSeq: 30 INVITE\r\n\r\n".to_vec();
    let invite_b = b"INVITE sip:c SIP/2.0\r\nCall-ID: b\r\nCSeq: 1 INVITE\r\n\r\n".to_vec();
    let wire = |raw: &Vec<u8>| RecordedSipEntry { raw: raw.clone(), ..entry("127.0.0.1:5071") };
    // Wire order: A's INVITE, B's INVITE, A's 100 — all recorded at one instant.
    let entries = vec![wire(&invite_a), wire(&invite_b), wire(&trying_a)];
    let legs = BTreeMap::from([
        (
            "A".to_string(),
            vec![
                RecordedMessage::new(
                    1,
                    Dir::Out,
                    7,
                    None,
                    invite_a.clone(),
                    &sip_message::CustomParser::new(),
                ),
                RecordedMessage::new(
                    2,
                    Dir::In,
                    7,
                    None,
                    trying_a.clone(),
                    &sip_message::CustomParser::new(),
                ),
            ],
        ),
        (
            "B".to_string(),
            vec![RecordedMessage::new(
                1,
                Dir::In,
                7,
                None,
                invite_b.clone(),
                &sip_message::CustomParser::new(),
            )],
        ),
    ]);
    let all = vec![
        finding("cseq-in-dialog-order", false, Some(2), None),
        finding("rport-echo", true, Some(3), None),
    ];
    let audit = audited(&all, &[], &entries, &caller_endpoint(), &Stated { legs, ..uncancelled() });
    let RunRfcAudit::Audited { findings } = audit else { panic!("audited") };
    assert_eq!(findings[0].offending_message, Some(MessageRef { leg: "B".into(), seq: 1 }));
    assert_eq!(findings[1].offending_message, Some(MessageRef { leg: "A".into(), seq: 2 }));
}
