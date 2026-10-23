use super::*;
use crate::plan::StepKind;
use crate::program::StepLoc;
use crate::state::RunState;
use pivot_schema::bundle::{ClockMode, IdentityBindings, RunConfig};
use pivot_schema::flow::{Anchor, CheckMode, Delay};
use pivot_schema::known_bug::KnownBug;
use pivot_schema::msg::Header;
use pivot_schema::scoping::CheckClass;
use std::sync::OnceLock;

/// A resolver over an empty binding set. These tests freeze literal values,
/// so every `${…}`-free header resolves to itself; the ones that DO state an
/// accessor build their own bindings.
fn plain() -> Resolver<'static> {
    static STATE: OnceLock<RunState> = OnceLock::new();
    static BINDINGS: OnceLock<IdentityBindings> = OnceLock::new();
    Resolver::new(STATE.get_or_init(RunState::new), BINDINGS.get_or_init(IdentityBindings::new))
}

/// A lane's configuration; `Scope::new(&config, origin)` binds it to the
/// document a test is replaying.
fn lane() -> RunConfig {
    RunConfig::new("upstream-demo", ClockMode::Virtual, "127.0.0.1:5080")
}

fn step(msg: MsgSpec, check: CheckMode) -> CompiledStep {
    let discriminator = match (&msg.method, msg.status) {
        (Some(m), _) => Discriminator::Request { method: m.clone() },
        (None, Some(s)) => {
            Discriminator::Response { status: s, cseq_method: msg.cseq_method.clone() }
        }
        _ => panic!("a test step states a discriminator"),
    };
    CompiledStep {
        id: "s1".into(),
        leg: "A".into(),
        kind: StepKind::Expect { check, optional: false },
        auto: false,
        after: vec![],
        checks: vec![],
        early: None,
        overlap: None,
        msg,
        delay: Delay { ms: 0, from: Anchor::Trigger, compressible: true, timer_linked: false },
        within_ms: 32000,
        retransmits: None,
        retransmit_intervals_ms: Vec::new(),
        loc: StepLoc { item: 0, branch: None, within: 0 },
        order: 0,
        deviations: vec![],
        discriminator,
    }
}

fn inbound_response(status: u16, cseq_method: &str) -> Inbound {
    Inbound {
        method: None,
        status: Some(status),
        reason: Some("Busy Here".into()),
        cseq_method: cseq_method.into(),
        cseq: 1,
        call_id: "c1".into(),
        rseq: None,
        headers: vec![("Reason".into(), "Q.850;cause=17".into())].into(),
        body: vec![],
        content_type: None,
        ruri_user: None,
        from_tag: Some("a1".into()),
        to_tag: Some("b1".into()),
        from_user: Some("alice".into()),
        to_user: Some("bob".into()),
    }
}

fn inbound_request(method: &str) -> Inbound {
    Inbound {
        method: Some(method.into()),
        status: None,
        reason: None,
        cseq_method: method.into(),
        cseq: 1,
        call_id: "c1".into(),
        rseq: None,
        headers: HeaderProjection::default(),
        body: vec![],
        content_type: None,
        ruri_user: Some("0900004".into()),
        from_tag: Some("a1".into()),
        to_tag: None,
        from_user: Some("alice".into()),
        to_user: Some("bob".into()),
    }
}

#[test]
fn a_response_gate_pins_the_transaction_it_answers() {
    let spec =
        MsgSpec { status: Some(200), cseq_method: Some("INVITE".into()), ..MsgSpec::default() };
    let step = step(spec, CheckMode::Record);
    assert!(discriminates(&step, &inbound_response(200, "INVITE")).matches());
    // A PRACK's own 2xx must not satisfy the call's answer.
    let other = discriminates(&step, &inbound_response(200, "PRACK"));
    assert_eq!(
        other,
        GateVerdict::Rejects("gated on 200 to INVITE; a 200 to PRACK arrived".into())
    );
    assert!(!discriminates(&step, &inbound_response(486, "INVITE")).matches());
    assert!(!discriminates(&step, &inbound_request("ACK")).matches());
}

/// The forking gate: two rings of one leg differ only by the To-tag the UAS
/// minted, so a PRACK for fork 1 must not complete fork 2's step.
#[test]
fn an_expect_scoped_to_a_fork_takes_only_that_fork_s_message() {
    let mut step =
        step(MsgSpec { method: Some("PRACK".into()), ..MsgSpec::default() }, CheckMode::Record);
    step.early = Some("f1".into());
    let mut prack = inbound_request("PRACK");
    prack.to_tag = Some("B-early-f1".into());
    assert!(rides_early_dialog(&step, &prack, Some("B-early-f1")).matches());

    let mut other_fork = prack.clone();
    other_fork.to_tag = Some("B-early-f2".into());
    assert_eq!(
        rides_early_dialog(&step, &other_fork, Some("B-early-f1")),
        GateVerdict::Rejects(
            "gated on early dialog \"f1\" (To-tag \"B-early-f1\"); To-tag \"B-early-f2\" arrived"
                .into()
        )
    );
    // A message with no To-tag rides no early dialog at all.
    let mut untagged = prack.clone();
    untagged.to_tag = None;
    assert!(!rides_early_dialog(&step, &untagged, Some("B-early-f1")).matches());
}

/// A step naming no early dialog is scoped to none: the gate is silent, and
/// what the message carries in To is the discriminator's business, not its.
#[test]
fn a_step_naming_no_early_dialog_is_scoped_to_none() {
    let step =
        step(MsgSpec { method: Some("PRACK".into()), ..MsgSpec::default() }, CheckMode::Record);
    let mut prack = inbound_request("PRACK");
    prack.to_tag = Some("whatever".into());
    assert!(rides_early_dialog(&step, &prack, None).matches());
    assert!(rides_observed_fork(&step, &prack, &LearnedForks::default()).matches());
}

/// A step gated on an observed fork, with the fork it names bound or not.
fn observed_step(early: &str) -> CompiledStep {
    let mut step = step(
        MsgSpec { status: Some(180), cseq_method: Some("INVITE".into()), ..MsgSpec::default() },
        CheckMode::Record,
    );
    step.early = Some(early.into());
    step
}

/// The observed-fork demux: an unbound id takes a free tag, and a second id
/// cannot take a tag another id on the leg already rides — one tag toward
/// this side is ONE early dialog (RFC 3261 §12.1.1), whatever the document
/// hoped.
#[test]
fn a_second_observed_fork_refuses_a_tag_another_id_already_rides() {
    let mut ring = inbound_response(180, "INVITE");
    ring.to_tag = Some("sut-tag-1".into());

    let mut learned = LearnedForks::default();
    assert!(rides_observed_fork(&observed_step("f1"), &ring, &learned).matches());
    learned.bind("A", "f1", "sut-tag-1");
    assert_eq!(
        rides_observed_fork(&observed_step("f2"), &ring, &learned),
        GateVerdict::Rejects(
            "gated on early dialog \"f2\", which must be a dialog of its own; \
                 To-tag \"sut-tag-1\" already rides \"f1\""
                .into()
        )
    );
    // A fresh tag is a dialog of its own, and f2 is free to be it.
    let mut second = ring.clone();
    second.to_tag = Some("sut-tag-2".into());
    assert!(rides_observed_fork(&observed_step("f2"), &second, &learned).matches());
}

/// A bound id requires the tag that taught it, exactly as a minted fork
/// requires the tag it answered under.
#[test]
fn a_bound_observed_fork_refuses_every_other_tag() {
    let mut learned = LearnedForks::default();
    learned.bind("A", "f1", "sut-tag-1");
    let mut same = inbound_response(180, "INVITE");
    same.to_tag = Some("sut-tag-1".into());
    assert!(rides_observed_fork(&observed_step("f1"), &same, &learned).matches());

    let mut other = same.clone();
    other.to_tag = Some("sut-tag-2".into());
    assert_eq!(
        rides_observed_fork(&observed_step("f1"), &other, &learned),
        GateVerdict::Rejects(
            "gated on early dialog \"f1\" (To-tag \"sut-tag-1\"); To-tag \"sut-tag-2\" arrived"
                .into()
        )
    );
    // A message with no To-tag rides no early dialog at all, bound or not.
    let mut untagged = same.clone();
    untagged.to_tag = None;
    assert!(!rides_observed_fork(&observed_step("f1"), &untagged, &learned).matches());
    assert!(!rides_observed_fork(&observed_step("f9"), &untagged, &learned).matches());
}

/// A FINAL under a second tag is a second observed fork exactly as a
/// provisional is — nothing in the fork gate reads the status. This is the
/// forking shape where one fork rings and another answers: 180 under tag X,
/// 200 under tag Y, one INVITE transaction.
#[test]
fn a_final_under_a_second_tag_is_a_second_observed_fork() {
    let mut learned = LearnedForks::default();
    learned.bind("A", "r1", "sut-tag-1");
    let mut answer = step(
        MsgSpec { status: Some(200), cseq_method: Some("INVITE".into()), ..MsgSpec::default() },
        CheckMode::Record,
    );
    answer.early = Some("r2".into());
    let mut ok = inbound_response(200, "INVITE");
    ok.to_tag = Some("sut-tag-2".into());
    assert!(rides_observed_fork(&answer, &ok, &learned).matches());
    ok.to_tag = Some("sut-tag-1".into());
    assert!(!rides_observed_fork(&answer, &ok, &learned).matches(), "r1's tag is not r2's");
}

/// A leg owns its own learned tag space: a binding on ANOTHER leg neither
/// satisfies this leg's id nor holds a tag against it.
#[test]
fn a_binding_on_another_leg_is_another_dialog() {
    let mut learned = LearnedForks::default();
    learned.bind("B", "f1", "sut-tag-1");
    let mut ring = inbound_response(180, "INVITE");
    ring.to_tag = Some("sut-tag-1".into());
    // The step under test rides leg "A", where nothing is bound.
    assert!(rides_observed_fork(&observed_step("f2"), &ring, &learned).matches());
    assert!(rides_observed_fork(&observed_step("f1"), &ring, &learned).matches());
}

#[test]
fn a_request_gate_reads_method_identity_not_spelling() {
    let step =
        step(MsgSpec { method: Some("Invite".into()), ..MsgSpec::default() }, CheckMode::Record);
    assert!(discriminates(&step, &inbound_request("INVITE")).matches());
    assert!(!discriminates(&step, &inbound_request("ACK")).matches());
}

#[test]
fn record_matches_nothing_and_assert_matches_the_stored_headers() {
    let config = lane();
    let scope = Scope::new(&config, None);
    let spec = MsgSpec {
        status: Some(486),
        cseq_method: Some("INVITE".into()),
        headers: vec![Header {
            name: "Reason".into(),
            value: "Q.850;cause=17".into(),
            class: None,
        }],
        ..MsgSpec::default()
    };
    let recorded = step(spec.clone(), CheckMode::Record);
    let asserted = step(spec, CheckMode::Assert);
    let mut arrived = inbound_response(486, "INVITE");
    assert!(content_holds(&recorded, &arrived, &scope, &plain()).matches());
    assert!(content_holds(&asserted, &arrived, &scope, &plain()).matches());
    arrived.headers = vec![("Reason".into(), "Q.850;cause=16".into())].into();
    assert!(
        content_holds(&recorded, &arrived, &scope, &plain()).matches(),
        "record asserts nothing"
    );
    let rejected = content_holds(&asserted, &arrived, &scope, &plain());
    assert!(matches!(&rejected, GateVerdict::Rejects(r) if r.contains("cause=16")), "{rejected:?}");
}

#[test]
fn a_declared_body_shape_is_asserted_under_assert() {
    let config = lane();
    let scope = Scope::new(&config, None);
    let absent = step(
        MsgSpec {
            status: Some(486),
            cseq_method: Some("INVITE".into()),
            body: Some(Body::Shape(pivot_schema::body::ShapeBody { mode: BodyShape::Absent })),
            ..MsgSpec::default()
        },
        CheckMode::Assert,
    );
    let mut arrived = inbound_response(486, "INVITE");
    assert!(content_holds(&absent, &arrived, &scope, &plain()).matches());
    arrived.body = b"v=0".to_vec();
    assert!(!content_holds(&absent, &arrived, &scope, &plain()).matches());

    let sdp = step(
        MsgSpec {
            method: Some("INVITE".into()),
            body: Some(Body::Shape(pivot_schema::body::ShapeBody { mode: BodyShape::SdpPresent })),
            ..MsgSpec::default()
        },
        CheckMode::Assert,
    );
    let mut invite = inbound_request("INVITE");
    invite.body = b"v=0".to_vec();
    assert!(
        !content_holds(&sdp, &invite, &scope, &plain()).matches(),
        "no content type is not SDP"
    );
    invite.content_type = Some("application/sdp".into());
    assert!(content_holds(&sdp, &invite, &scope, &plain()).matches());

    // A resource compared as a session description gates the media type
    // exactly as the shape does; its content is the confrontation's.
    let described = step(
        MsgSpec {
            method: Some("INVITE".into()),
            body: Some(Body::Resource(pivot_schema::body::ResourceBody {
                reference: "resources/uas1_r0_0.sdp".into(),
                rewrite: vec!["c=addr".into(), "m=port".into()],
                mode: None,
                content_type: None,
                compare: Some(BodyCompare::Sdp),
            })),
            ..MsgSpec::default()
        },
        CheckMode::Assert,
    );
    let mut offer = inbound_request("INVITE");
    assert!(
        !content_holds(&described, &offer, &scope, &plain()).matches(),
        "a body owed and missing is refused"
    );
    offer.body = b"v=0".to_vec();
    offer.content_type = Some("application/example+xml".into());
    assert!(
        !content_holds(&described, &offer, &scope, &plain()).matches(),
        "another type is not a session description"
    );
    offer.content_type = Some("application/sdp".into());
    assert!(content_holds(&described, &offer, &scope, &plain()).matches());
    offer.body = b"v=0\r\ns=other".to_vec();
    assert!(
        content_holds(&described, &offer, &scope, &plain()).matches(),
        "the content is the confrontation's, never the gate's"
    );
}

/// A `multipart` body on an expect gates the media type the way the
/// `multipart-present` shape does: what arrived must be `multipart/*` for
/// the confrontation to locate its parts. The parts' content stays the
/// confrontation's.
#[test]
fn a_multipart_body_on_an_expect_gates_the_container_type() {
    let config = lane();
    let scope = Scope::new(&config, None);
    let described = step(
        MsgSpec {
            method: Some("INVITE".into()),
            body: Some(Body::Multipart(pivot_schema::body::MultipartBody {
                multipart: pivot_schema::body::Multipart {
                    content_type: "multipart/mixed".into(),
                    parts: vec![pivot_schema::body::Part {
                        content_type: "application/vnd.example.blob".into(),
                        reference: "resources/uas1_r0_1.bin".into(),
                        rewrite: vec![],
                        mode: Some(pivot_schema::body::BodyMode::Frozen),
                        compare: None,
                        content_id: None,
                        headers: vec![],
                        cid_linked: vec![],
                    }],
                },
            })),
            ..MsgSpec::default()
        },
        CheckMode::Assert,
    );
    let mut invite = inbound_request("INVITE");
    assert!(
        !content_holds(&described, &invite, &scope, &plain()).matches(),
        "a body owed and missing is refused"
    );
    invite.body = b"v=0".to_vec();
    invite.content_type = Some("application/sdp".into());
    assert!(
        !content_holds(&described, &invite, &scope, &plain()).matches(),
        "a single body is not a multipart one"
    );
    invite.body = b"--b\r\n--b--\r\n".to_vec();
    invite.content_type = Some("multipart/mixed;boundary=b".into());
    assert!(
        content_holds(&described, &invite, &scope, &plain()).matches(),
        "the parts are the confrontation's, never the gate's"
    );
}

/// A lane that declares the known bug keeps taking the datagram, so the run
/// reaches the steps behind the symptom; a lane that declares nothing still
/// refuses it. What the waived assertion found is recorded either way.
#[test]
fn a_lane_declared_known_bug_takes_the_body_out_of_the_match_and_records_what_it_found() {
    let provisional = step(
        MsgSpec {
            status: Some(180),
            cseq_method: Some("INVITE".into()),
            body: Some(Body::Shape(pivot_schema::body::ShapeBody { mode: BodyShape::Absent })),
            ..MsgSpec::default()
        },
        CheckMode::Assert,
    );
    let mut arrived = inbound_response(180, "INVITE");
    arrived.body = b"v=0\r\n".to_vec();

    let strict = lane();
    let strict = Scope::new(&strict, None);
    assert_eq!(
        content_holds(&provisional, &arrived, &strict, &plain()),
        GateVerdict::Rejects("body must be absent; 5 bytes arrived".into()),
        "a lane declaring nothing refuses the datagram"
    );
    assert!(waived_body_findings(&provisional, &arrived, &strict).is_empty());

    let waiving = lane().with_known_bug(KnownBug::ProvisionalRewriteNotApplied);
    let waiving = Scope::new(&waiving, None);
    assert!(content_holds(&provisional, &arrived, &waiving, &plain()).matches());
    let found = waived_body_findings(&provisional, &arrived, &waiving);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, KnownBug::ProvisionalRewriteNotApplied);
    assert!(matches!(
        &found[0].1,
        Failure::CheckFailed { field, expected, observed, .. }
            if field == "body" && expected == "absent"
                && observed == "body must be absent; 5 bytes arrived"
    ));
}

/// The waiver is the ONE miss it names, on the ONE message class it names.
/// A body owed and missing, a wrong media type, and an unexpected body on a
/// final all still gate on the waiving lane — a lane that stopped asserting
/// bodies would tell us nothing about the calls it replays.
#[test]
fn the_waiver_reaches_no_other_body_assertion() {
    let config = lane().with_known_bug(KnownBug::ProvisionalRewriteNotApplied);
    let scope = Scope::new(&config, None);

    let mut on_a_final = step(
        MsgSpec {
            status: Some(200),
            cseq_method: Some("INVITE".into()),
            body: Some(Body::Shape(pivot_schema::body::ShapeBody { mode: BodyShape::Absent })),
            ..MsgSpec::default()
        },
        CheckMode::Assert,
    );
    let mut arrived = inbound_response(200, "INVITE");
    arrived.body = b"v=0".to_vec();
    assert!(
        !content_holds(&on_a_final, &arrived, &scope, &plain()).matches(),
        "a final is not a rewrite"
    );

    // A 100 Trying is not the message a relay18x mode rewrites either.
    on_a_final.msg.status = Some(100);
    let mut trying = inbound_response(100, "INVITE");
    trying.body = b"v=0".to_vec();
    assert!(!content_holds(&on_a_final, &trying, &scope, &plain()).matches());

    let owed = step(
        MsgSpec {
            status: Some(183),
            cseq_method: Some("INVITE".into()),
            body: Some(Body::Shape(pivot_schema::body::ShapeBody { mode: BodyShape::SdpPresent })),
            ..MsgSpec::default()
        },
        CheckMode::Assert,
    );
    let bodyless = inbound_response(183, "INVITE");
    assert!(
        !content_holds(&owed, &bodyless, &scope, &plain()).matches(),
        "an SDP the provisional owes is still gated on"
    );
}

#[test]
fn a_classified_frozen_header_leaves_the_match_on_a_foreign_lane_and_is_still_read() {
    let spec = MsgSpec {
        status: Some(200),
        cseq_method: Some("INVITE".into()),
        headers: vec![
            Header {
                name: "P-Charging-Vector".into(),
                value: "icid-value=abc".into(),
                class: Some(CheckClass::OriginPlatformHeader),
            },
            Header { name: "Reason".into(), value: "Q.850;cause=17".into(), class: None },
        ],
        ..MsgSpec::default()
    };
    let asserted = step(spec, CheckMode::Assert);
    let arrived = inbound_response(200, "INVITE");
    let config = lane();

    // On the lane the header came from it is preferred, so the whole
    // assertion does not hold.
    let home = Scope::new(&config, Some("upstream-demo"));
    let rejected = content_holds(&asserted, &arrived, &home, &plain());
    assert!(
        matches!(&rejected, GateVerdict::Rejects(r) if r.contains("P-Charging-Vector")),
        "{rejected:?}"
    );

    // On another lane the same header is matched on by nothing, while the
    // unclassified `Reason` still decides the preference.
    let foreign = Scope::new(&config, Some("origin-platform"));
    assert!(content_holds(&asserted, &arrived, &foreign, &plain()).matches());

    // Under EITHER lane the header is read and named, and its class is what
    // decides whether the run pays for it.
    for scope in [home, foreign] {
        let findings = header_findings(&asserted, &arrived, &plain());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].class, Some(CheckClass::OriginPlatformHeader));
        assert!(
            matches!(&findings[0].failure, Failure::CheckFailed { field, observed, .. }
                if field == "header(P-Charging-Vector)" && observed == "absent"),
            "{:#?}",
            findings[0]
        );
        let _ = scope;
    }

    // `record` asserts nothing, so it records nothing either.
    let recorded = step(asserted.msg.clone(), CheckMode::Record);
    assert!(header_findings(&recorded, &arrived, &plain()).is_empty());
}

/// A frozen header value states `${…}` like every other
/// string a document carries, and the expect side resolves it exactly as
/// the send side does — one token cannot mean two things across a relay.
#[test]
fn a_frozen_header_value_resolves_its_accessors_before_it_is_compared() {
    let config = lane();
    let scope = Scope::new(&config, None);
    let state = RunState::new();
    let bindings = IdentityBindings::new().bind("called-0-0", "intl-00", "0015550900001");
    let resolver = Resolver::new(&state, &bindings);
    let spec = MsgSpec {
        status: Some(183),
        cseq_method: Some("INVITE".into()),
        headers: vec![Header {
            name: "Diversion".into(),
            value: "<sip:${num:called-0-0:intl-00}@h>;reason=unconditional".into(),
            class: None,
        }],
        ..MsgSpec::default()
    };
    let asserted = step(spec, CheckMode::Assert);
    let mut arrived = inbound_response(183, "INVITE");
    arrived.headers =
        vec![("Diversion".into(), "<sip:0015550900001@h>;reason=unconditional".into())].into();

    // The relayed value IS what the accessor names, so the assertion holds
    // and nothing is owed a finding.
    assert!(content_holds(&asserted, &arrived, &scope, &resolver).matches());
    assert!(header_findings(&asserted, &arrived, &resolver).is_empty());

    // A finding states the RESOLVED value, never the token: what the run
    // wanted is a number, and the verdict has to be readable as one.
    let mut other = arrived.clone();
    other.headers =
        vec![("Diversion".into(), "<sip:+15550900001@h>;reason=unconditional".into())].into();
    let findings = header_findings(&asserted, &other, &resolver);
    assert!(
        matches!(&findings[..], [f] if matches!(&f.failure,
            Failure::CheckFailed { expected, observed, .. }
                if expected == "<sip:0015550900001@h>;reason=unconditional"
                    && observed == "<sip:+15550900001@h>;reason=unconditional")),
        "{findings:#?}"
    );

    // An accessor this run cannot answer is not a value: the header does
    // not hold, and the finding names the accessor rather than passing.
    let nothing_bound = IdentityBindings::new();
    let unbound = Resolver::new(&state, &nothing_bound);
    assert!(!content_holds(&asserted, &arrived, &scope, &unbound).matches());
    let findings = header_findings(&asserted, &arrived, &unbound);
    assert!(
        matches!(&findings[..], [f] if matches!(&f.failure,
            Failure::CheckFailed { expected, .. } if expected.starts_with("unresolved accessor:"))),
        "{findings:#?}"
    );
}

/// A frozen list header compares by wire FORM, not by bytes.
/// RFC 3261 §7.3.1 lets the separators of a list carry whitespace and lets
/// several rows combine into one, so a platform that re-lays out its own
/// header has changed nothing — and a platform that changed a VALUE still
/// fails, including where our stack advertised more than the capture did.
#[test]
fn a_frozen_list_header_holds_across_whitespace_and_row_layout() {
    let config = lane();
    let scope = Scope::new(&config, None);
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let resolver = Resolver::new(&state, &bindings);
    let frozen = |rows: &[&str]| MsgSpec {
        status: Some(200),
        cseq_method: Some("INVITE".into()),
        headers: rows
            .iter()
            .map(|value| Header { name: "Allow".into(), value: (*value).into(), class: None })
            .collect(),
        ..MsgSpec::default()
    };
    let carrying = |rows: &[&str]| {
        let mut arrived = inbound_response(200, "INVITE");
        arrived.headers =
            rows.iter().map(|v| ("Allow".to_string(), (*v).to_string())).collect::<Vec<_>>().into();
        arrived
    };
    let holds = |spec: MsgSpec, arrived: &Inbound| {
        let asserted = step(spec, CheckMode::Assert);
        let matched = content_holds(&asserted, arrived, &scope, &resolver).matches();
        assert_eq!(matched, header_findings(&asserted, arrived, &resolver).is_empty());
        matched
    };

    // Whitespace around the separators is layout, not value.
    assert!(holds(frozen(&["INVITE,ACK,BYE"]), &carrying(&["INVITE, ACK, BYE"])));
    // Rows fold, in both directions.
    assert!(holds(frozen(&["INVITE, ACK", "BYE"]), &carrying(&["INVITE,ACK,BYE"])));
    assert!(holds(frozen(&["INVITE,ACK,BYE"]), &carrying(&["INVITE, ACK", "BYE"])));
    // Order is the value: a reordered list is a difference.
    assert!(!holds(frozen(&["INVITE,ACK,BYE"]), &carrying(&["ACK, INVITE, BYE"])));
    // §6.4's residue: our stack advertising MORE is still a finding.
    assert!(!holds(frozen(&["INVITE,ACK"]), &carrying(&["INVITE, ACK, OPTIONS"])));
    assert!(!holds(frozen(&["INVITE, ACK", "BYE"]), &carrying(&["INVITE,ACK,BYE,OPTIONS"])));
}

/// A header set that differs NAMES itself and still fails.
/// The whole assertion does not hold, so the step is not preferred; the
/// match gate takes the datagram anyway, and every header that missed is a
/// finding carrying the frozen value and what actually arrived.
#[test]
fn a_header_set_that_differs_leaves_the_match_and_names_each_header_with_both_sides() {
    let config = lane();
    let scope = Scope::new(&config, None);
    let spec = MsgSpec {
        status: Some(200),
        cseq_method: Some("INVITE".into()),
        headers: vec![
            // Present, and carrying another value.
            Header { name: "Reason".into(), value: "Q.850;cause=31".into(), class: None },
            // Absent altogether.
            Header {
                name: "Content-Disposition".into(),
                value: "session;handling=required".into(),
                class: None,
            },
            // Present and holding: no finding is owed for it.
            Header { name: "Reason".into(), value: "Q.850;cause=17".into(), class: None },
        ],
        headers_present: vec!["Session-Expires".into()],
        ..MsgSpec::default()
    };
    let asserted = step(spec, CheckMode::Assert);
    let arrived = inbound_response(200, "INVITE");

    // The preference does not hold — and the MATCH does, so the step takes
    // the message, answers it, and the dialog goes on.
    assert!(!content_holds(&asserted, &arrived, &scope, &plain()).matches());
    assert!(body_content_holds(&asserted, &arrived, &scope).matches());

    let findings = header_findings(&asserted, &arrived, &plain());
    let named: Vec<(String, String, String, String)> = findings
        .iter()
        .map(|f| match &f.failure {
            Failure::CheckFailed { field, op, expected, observed, .. } => {
                (field.clone(), op.clone(), expected.clone(), observed.clone())
            }
            other => panic!("a header finding is a check failure: {other:?}"),
        })
        .collect();
    assert_eq!(
        named,
        [
            (
                "header(Reason)".into(),
                "eq".into(),
                "Q.850;cause=31".into(),
                "Q.850;cause=17".into()
            ),
            (
                "header(Content-Disposition)".into(),
                "eq".into(),
                "session;handling=required".into(),
                "absent".into()
            ),
            ("header(Session-Expires)".into(), "exists".into(), "present".into(), "absent".into()),
        ],
        "{findings:#?}"
    );
    // Every one of them gates: an unclassified header is the protocol's own.
    assert!(findings.iter().all(|f| f.class.is_none()), "{findings:#?}");
}

/// A body the assertion states still gates the MATCH: only the header set
/// left it, and a message whose body is the wrong shape is not the step's.
#[test]
fn a_body_shape_still_gates_the_match_a_header_set_no_longer_does() {
    let config = lane();
    let scope = Scope::new(&config, None);
    let asserted = step(
        MsgSpec {
            status: Some(486),
            cseq_method: Some("INVITE".into()),
            headers: vec![Header {
                name: "Reason".into(),
                value: "Q.850;cause=31".into(),
                class: None,
            }],
            body: Some(Body::Shape(pivot_schema::body::ShapeBody { mode: BodyShape::Absent })),
            ..MsgSpec::default()
        },
        CheckMode::Assert,
    );
    let mut arrived = inbound_response(486, "INVITE");
    assert!(body_content_holds(&asserted, &arrived, &scope).matches());
    arrived.body = b"v=0".to_vec();
    assert_eq!(
        body_content_holds(&asserted, &arrived, &scope),
        GateVerdict::Rejects("body must be absent; 3 bytes arrived".into())
    );
}

/// The projection reads header IDENTITY, not spelling: a message spelled in
/// compact forms still answers a long-form gate, accessor and body-shape
/// check — and the same body-shape check refuses a near-miss token that a
/// value-prefix probe would have taken for SDP.
#[test]
fn a_compact_spelled_message_is_read_as_its_headers() {
    use sip_message::{CustomParser, SipParser};
    let msg = CustomParser::new()
        .parse(
            b"INVITE sip:b@h SIP/2.0\r\n\
v: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
From: <sip:a@h>;tag=f1\r\n\
To: <sip:b@h>\r\n\
i: compact-1\r\n\
CSeq: 1 INVITE\r\n\
c: application/sdp\r\n\
l: 3\r\n\r\nv=0",
        )
        .expect("test message parses");
    let inbound = Inbound::of(&msg);
    assert_eq!(inbound.content_type.as_deref(), Some("application/sdp"));
    assert_eq!(inbound.header("Via"), Some("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1"));
    assert_eq!(inbound.header("Content-Type"), Some("application/sdp"));

    let config = lane();
    let scope = Scope::new(&config, None);
    let sdp_expected = step(
        MsgSpec {
            method: Some("INVITE".into()),
            headers_present: vec!["Via".into()],
            body: Some(Body::Shape(pivot_schema::body::ShapeBody { mode: BodyShape::SdpPresent })),
            ..MsgSpec::default()
        },
        CheckMode::Assert,
    );
    assert!(content_holds(&sdp_expected, &inbound, &scope, &plain()).matches());

    let mut near_miss = inbound.clone();
    near_miss.content_type = Some("application/sdp-x".into());
    assert!(
        !content_holds(&sdp_expected, &near_miss, &scope, &plain()).matches(),
        "a longer media-type token is another type, not SDP"
    );
}

#[test]
fn a_missing_header_names_itself_rather_than_passing() {
    let config = lane();
    let scope = Scope::new(&config, None);
    let spec = MsgSpec {
        status: Some(200),
        cseq_method: Some("INVITE".into()),
        headers_present: vec!["Session-Expires".into()],
        ..MsgSpec::default()
    };
    let asserted = step(spec, CheckMode::Assert);
    let rejected = content_holds(&asserted, &inbound_response(200, "INVITE"), &scope, &plain());
    assert_eq!(rejected, GateVerdict::Rejects("header \"Session-Expires\" is absent".into()));
}
