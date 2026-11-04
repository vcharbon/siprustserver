//! Body TYPES on every expect: what a step states about the body's presence,
//! its bare media type and, for a multipart body, its parts' count and bare
//! types by position, held against what arrived under `check: record` and
//! `check: assert` alike.
//!
//! Every rung replays one answered call (`binary-multipart.v3.json`, the
//! caller's INVITE relayed to the callee) with one step's stated or sent body
//! changed in memory. A type that does not hold fails the run; the step still
//! takes its datagram, so the call walks to its teardown and every step
//! completes.

use super::*;
use pivot_schema::body::{
    Body, BodyCompare, BodyMode, BodyShape, Multipart, MultipartBody, Part, ResourceBody, ShapeBody,
};
use pivot_schema::flow::CheckMode;
use pivot_schema::known_bug::KnownBug;

/// The callee's expect of the initial INVITE.
const CALLEE_INVITE: &str = "s3";

fn shape(mode: BodyShape) -> Body {
    Body::Shape(ShapeBody { mode })
}

/// The SDP offer as a single rewritten resource.
fn offer() -> Body {
    Body::Resource(ResourceBody {
        reference: "resources/uac1_offer.sdp".into(),
        rewrite: vec!["c=addr".into(), "m=port".into()],
        mode: None,
        content_type: None,
        compare: None,
    })
}

fn sdp_part() -> Part {
    Part {
        content_type: "application/sdp".into(),
        reference: "resources/uac1_offer.sdp".into(),
        rewrite: vec!["c=addr".into(), "m=port".into()],
        mode: None,
        compare: None,
        content_id: None,
        headers: vec![],
        cid_linked: vec![],
    }
}

fn blob_part() -> Part {
    Part {
        content_type: "application/vnd.example.blob".into(),
        reference: "resources/binary-multipart_part1.bin".into(),
        rewrite: vec![],
        mode: Some(BodyMode::Frozen),
        compare: None,
        content_id: None,
        headers: vec![],
        cid_linked: vec![],
    }
}

fn multipart(content_type: &str, parts: Vec<Part>) -> Body {
    Body::Multipart(MultipartBody {
        multipart: Multipart { content_type: content_type.into(), parts },
    })
}

/// The fixture, with the caller's INVITE sending `sent` and the callee's
/// expect of it stating `stated` under `check`.
fn invite_case(sent: Body, stated: Body, check: CheckMode) -> Case {
    let mut case = fixture("binary-multipart.v3.json");
    step_of(&mut case, "s1").msg.body = Some(sent);
    let expect = step_of(&mut case, CALLEE_INVITE);
    expect.msg.body = Some(stated);
    expect.check = Some(check);
    case
}

/// The one gating finding a body-type miss at `step` states.
fn body_type_failures(outcome: &Outcome, step: &str) -> Vec<(String, String)> {
    outcome
        .verdict
        .failures
        .iter()
        .filter_map(|failure| match failure {
            Failure::CheckFailed { site, field, op, expected, observed }
                if site == &format!("step {step:?}") && field == "body" && op == "type" =>
            {
                Some((expected.clone(), observed.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The run failed on exactly one body-type finding at `step`, and nothing
/// else: the step matched its datagram and the call ran to its teardown.
fn assert_fails_on_body_type_only(outcome: &Outcome, step: &str, expected: &str, observed: &str) {
    assert_eq!(
        body_type_failures(outcome, step),
        vec![(expected.to_string(), observed.to_string())],
        "one body-type finding at {step}: {:#?}",
        outcome.verdict.failures
    );
    assert_eq!(
        outcome.verdict.failures.len(),
        1,
        "the body type is the run's only failure: {:#?}",
        outcome.verdict.failures
    );
    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(
        outcome.verdict.completed_steps.len(),
        13,
        "the step took its datagram and the call ran to its teardown: {:#?}",
        outcome.verdict.completed_steps
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
}

/// A `record` expect stating a multipart body whose parts extraction did not
/// hand over, receiving a bare session description: the container type does
/// not hold, and the run fails on it.
#[tokio::test(start_paused = true)]
async fn a_record_expect_stating_multipart_fails_the_run_on_a_bare_sdp() {
    let scene = api_scene("pivot-body-type-multipart-shape").await;
    let case = invite_case(offer(), shape(BodyShape::MultipartPresent), CheckMode::Record);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_fails_on_body_type_only(&outcome, CALLEE_INVITE, "multipart/*", "application/sdp");
    scene.finish().await;
}

/// A `record` expect stating a session description, on a message that arrives
/// bodiless: presence is part of the type.
#[tokio::test(start_paused = true)]
async fn a_record_expect_stating_sdp_fails_the_run_on_a_bodiless_message() {
    let scene = api_scene("pivot-body-type-sdp-absent").await;
    let mut case = fixture("binary-multipart.v3.json");
    let ringing = step_of(&mut case, "s5");
    assert_eq!(ringing.check, Some(CheckMode::Record), "the caller's 180 is recorded");
    ringing.msg.body = Some(shape(BodyShape::SdpPresent));
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_fails_on_body_type_only(&outcome, "s5", "application/sdp", "absent");
    scene.finish().await;
}

/// A `record` multipart expect stating more parts than arrived: the part
/// count is part of the type.
#[tokio::test(start_paused = true)]
async fn a_record_multipart_expect_fails_the_run_on_a_missing_part() {
    let scene = api_scene("pivot-body-type-missing-part").await;
    let stated = multipart("multipart/mixed", vec![sdp_part(), blob_part(), blob_part()]);
    let sent = multipart("multipart/mixed", vec![sdp_part(), blob_part()]);
    let case = invite_case(sent, stated, CheckMode::Record);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_fails_on_body_type_only(
        &outcome,
        CALLEE_INVITE,
        "multipart/mixed (application/sdp, application/vnd.example.blob, application/vnd.example.blob)",
        "multipart/mixed (application/sdp, application/vnd.example.blob)",
    );
    scene.finish().await;
}

/// A `record` multipart expect whose parts arrive in another order: each
/// part's bare type is held by position.
#[tokio::test(start_paused = true)]
async fn a_record_multipart_expect_fails_the_run_on_parts_whose_types_are_swapped() {
    let scene = api_scene("pivot-body-type-swapped-parts").await;
    let stated = multipart("multipart/mixed", vec![blob_part(), sdp_part()]);
    let sent = multipart("multipart/mixed", vec![sdp_part(), blob_part()]);
    let case = invite_case(sent, stated, CheckMode::Record);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_fails_on_body_type_only(
        &outcome,
        CALLEE_INVITE,
        "multipart/mixed (application/vnd.example.blob, application/sdp)",
        "multipart/mixed (application/sdp, application/vnd.example.blob)",
    );
    scene.finish().await;
}

/// The framing is never part of the type: the same parts framed under the
/// boundary the sender chose, with parameters the stated container type does
/// not carry, hold a `record` expect.
#[tokio::test(start_paused = true)]
async fn a_record_multipart_expect_holds_the_same_parts_under_another_boundary() {
    let scene = api_scene("pivot-body-type-other-boundary").await;
    let stated = multipart("multipart/mixed", vec![sdp_part(), blob_part()]);
    let sent = multipart(
        "multipart/mixed;boundary=sender-chosen-boundary-7",
        vec![sdp_part(), blob_part()],
    );
    let case = invite_case(sent, stated, CheckMode::Record);
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    let callee = recorded_lines(&dir, "B");
    let invite = callee
        .iter()
        .find(|line| line["dir"] == "in" && recorded_wire(line).starts_with(b"INVITE "))
        .expect("the callee took the INVITE");
    assert!(
        String::from_utf8_lossy(&recorded_wire(invite)).contains("sender-chosen-boundary-7"),
        "the INVITE is framed under the sender's boundary"
    );
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    scene.finish().await;
}

/// An `assert` multipart expect: the container type gates the match, and a
/// missing part fails the run without refusing the datagram.
#[tokio::test(start_paused = true)]
async fn an_assert_multipart_expect_fails_the_run_on_a_missing_part() {
    let scene = api_scene("pivot-body-type-assert-missing-part").await;
    let stated = multipart("multipart/mixed", vec![sdp_part(), blob_part(), blob_part()]);
    let sent = multipart("multipart/mixed", vec![sdp_part(), blob_part()]);
    let case = invite_case(sent, stated, CheckMode::Assert);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_fails_on_body_type_only(
        &outcome,
        CALLEE_INVITE,
        "multipart/mixed (application/sdp, application/vnd.example.blob, application/vnd.example.blob)",
        "multipart/mixed (application/sdp, application/vnd.example.blob)",
    );
    scene.finish().await;
}

/// A `record` expect stating a resource of one media type, receiving another:
/// the bare media type is part of the type, its content stays the
/// confrontation's.
#[tokio::test(start_paused = true)]
async fn a_record_resource_expect_fails_the_run_on_another_media_type() {
    let scene = api_scene("pivot-body-type-resource").await;
    let stated = Body::Resource(ResourceBody {
        reference: "resources/binary-multipart_part1.bin".into(),
        rewrite: vec![],
        mode: Some(BodyMode::Frozen),
        content_type: Some("application/vnd.example.blob".into()),
        compare: Some(BodyCompare::Exact),
    });
    let case = invite_case(offer(), stated, CheckMode::Record);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_fails_on_body_type_only(
        &outcome,
        CALLEE_INVITE,
        "application/vnd.example.blob",
        "application/sdp",
    );
    scene.finish().await;
}

/// The caller's 180 carrying the callee's answer, where the `record` expect
/// of it states the provisional bodiless: the miss the lane-declarable
/// unstripped-provisional bug names.
fn ringing_with_answer() -> Case {
    let mut case = fixture("binary-multipart.v3.json");
    step_of(&mut case, "s4").msg.body = Some(Body::Resource(ResourceBody {
        reference: "resources/uas1_answer.sdp".into(),
        rewrite: vec!["c=addr".into(), "m=port".into()],
        mode: None,
        content_type: None,
        compare: None,
    }));
    let ringing = step_of(&mut case, "s5");
    assert_eq!(ringing.check, Some(CheckMode::Record), "the caller's 180 is recorded");
    ringing.msg.body = Some(shape(BodyShape::Absent));
    case
}

/// On a lane declaring the unstripped-provisional bug, the body on a recorded
/// provisional is waived exactly as on an asserted one: recorded, and the run
/// is green.
#[tokio::test(start_paused = true)]
async fn a_body_on_a_recorded_provisional_stays_waived_on_a_lane_declaring_the_bug() {
    let scene = api_scene("pivot-body-type-waived").await;
    let directives = LaneDirectives {
        known_bugs: vec![KnownBug::ProvisionalRewriteNotApplied],
        ..Default::default()
    };
    let (outcome, dir) =
        replay_directed(&scene, ringing_with_answer(), BTreeMap::new(), directives).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    assert_eq!(outcome.verdict.waived.len(), 1, "the miss is recorded as waived");
    let waived = &outcome.verdict.waived[0];
    assert_eq!(waived.bug, KnownBug::ProvisionalRewriteNotApplied);
    assert!(
        matches!(&waived.finding, Failure::CheckFailed { site, field, op, expected, observed }
            if site == "step \"s5\"" && field == "body" && op == "type"
                && expected == "absent" && observed == "application/sdp"),
        "{:#?}",
        waived.finding
    );
    scene.finish().await;
}

/// The same body on a lane declaring nothing fails the run.
#[tokio::test(start_paused = true)]
async fn a_body_on_a_recorded_provisional_fails_the_run_on_a_lane_declaring_nothing() {
    let scene = api_scene("pivot-body-type-unwaived").await;
    let (outcome, _dir) = replay_case(&scene, ringing_with_answer(), BTreeMap::new()).await;
    assert_fails_on_body_type_only(&outcome, "s5", "absent", "application/sdp");
    assert!(outcome.verdict.waived.is_empty());
    scene.finish().await;
}

/// Two armed `record` expects one datagram could satisfy: an optional one
/// stating a session description in front of the one stating none. A bare
/// 180 is the step whose stated body type it carries, and the optional is
/// released.
#[tokio::test(start_paused = true)]
async fn a_datagram_goes_to_the_armed_record_expect_whose_body_type_it_carries() {
    let scene = api_scene("pivot-body-type-preference").await;
    let mut case = fixture("binary-multipart.v3.json");
    let mut early = step_of(&mut case, "s5").clone();
    early.id = "s5a".into();
    early.optional = true;
    early.msg.body = Some(shape(BodyShape::SdpPresent));
    step_of(&mut case, "s5").msg.body = Some(shape(BodyShape::Absent));
    let at = case
        .document
        .flow
        .iter()
        .position(|node| matches!(node, pivot_schema::flow::FlowNode::Message(s) if s.id == "s5"))
        .expect("s5 is in the flow");
    case.document.flow.insert(at, pivot_schema::flow::FlowNode::Message(Box::new(early)));
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert!(outcome.verdict.completed_steps.contains(&"s5".to_string()));
    assert!(!outcome.verdict.completed_steps.contains(&"s5a".to_string()));
    scene.finish().await;
}
