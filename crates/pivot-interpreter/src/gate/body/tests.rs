use super::*;
use crate::gate::tests::{inbound_request, inbound_response, lane, plain, step};
use crate::gate::{content_holds, GateVerdict};
use pivot_schema::body::{BodyMode, Multipart, MultipartBody, Part, ResourceBody, ShapeBody};
use pivot_schema::flow::CheckMode;

fn shape(mode: BodyShape) -> Body {
    Body::Shape(ShapeBody { mode })
}

fn part(content_type: &str) -> Part {
    Part {
        content_type: content_type.into(),
        reference: "resources/uas1_r0_1.bin".into(),
        rewrite: vec![],
        mode: Some(BodyMode::Frozen),
        compare: None,
        content_id: None,
        headers: vec![],
        cid_linked: vec![],
    }
}

fn multipart(parts: &[&str]) -> Body {
    Body::Multipart(MultipartBody {
        multipart: Multipart {
            content_type: "multipart/mixed".into(),
            parts: parts.iter().map(|t| part(t)).collect(),
        },
    })
}

fn resource(content_type: Option<&str>) -> Body {
    Body::Resource(ResourceBody {
        reference: "resources/uas1_r0_0.bin".into(),
        rewrite: vec![],
        mode: Some(BodyMode::Frozen),
        content_type: content_type.map(str::to_string),
        compare: None,
    })
}

/// An INVITE expect stating `body` under `check`.
fn invite_expect(body: Body, check: CheckMode) -> CompiledStep {
    step(MsgSpec { method: Some("INVITE".into()), body: Some(body), ..MsgSpec::default() }, check)
}

/// An INVITE carrying `body` under `content_type`.
fn invite(content_type: Option<&str>, body: &[u8]) -> Inbound {
    let mut invite = inbound_request("INVITE");
    invite.content_type = content_type.map(str::to_string);
    invite.body = body.to_vec();
    invite
}

/// The `(expected, observed)` a gating body-type finding states; `None` where
/// the type held. Panics on a waived one, which no test here declares.
fn miss(stated: Body, check: CheckMode, arrived: &Inbound) -> Option<(String, String)> {
    let config = lane();
    let scope = Scope::new(&config, None);
    match body_type_finding(&invite_expect(stated, check), arrived, &scope)? {
        BodyTypeFinding::Gating(Finding {
            class: None,
            failure: Failure::CheckFailed { site, field, op, expected, observed },
        }) if site == "step \"s1\"" && field == "body" && op == "type" => {
            Some((expected, observed))
        }
        other => panic!("not a gating body-type finding: {other:?}"),
    }
}

fn pair(expected: &str, observed: &str) -> Option<(String, String)> {
    Some((expected.into(), observed.into()))
}

/// Two parts framed under `boundary`, with a preamble and bare-LF delimiters
/// where `loose` asks for them.
fn framed(boundary: &str, types: &[&str], loose: bool) -> Vec<u8> {
    let eol = if loose { "\n" } else { "\r\n" };
    let mut body = if loose { format!("a preamble{eol}") } else { String::new() };
    for content_type in types {
        body.push_str(&format!("--{boundary}{eol}Content-Type: {content_type}{eol}"));
        body.push_str(&format!("Content-ID: <p@example.invalid>{eol}{eol}payload{eol}"));
    }
    body.push_str(&format!("--{boundary}--{eol}"));
    body.into_bytes()
}

/// Every expect holds the body TYPE it states under `record` exactly as under
/// `assert`: presence, the bare media type, and nothing about the content.
#[test]
fn a_record_expect_holds_presence_and_the_bare_media_type() {
    let record = CheckMode::Record;
    let sdp = invite(Some("application/sdp"), b"v=0\r\n");
    let bodiless = invite(None, b"");
    let xml = invite(Some("application/example+xml;charset=utf-8"), b"<x/>");

    assert_eq!(miss(shape(BodyShape::Absent), record, &bodiless), None);
    assert_eq!(miss(shape(BodyShape::Absent), record, &sdp), pair("absent", "application/sdp"));
    assert_eq!(miss(shape(BodyShape::SdpPresent), record, &sdp), None);
    assert_eq!(
        miss(shape(BodyShape::SdpPresent), record, &bodiless),
        pair("application/sdp", "absent")
    );
    assert_eq!(
        miss(shape(BodyShape::SdpPresent), record, &invite(Some("application/sdp"), b"")),
        pair("application/sdp", "absent"),
        "a type with no bytes is no body"
    );
    assert_eq!(
        miss(shape(BodyShape::SdpPresent), record, &xml),
        pair("application/sdp", "application/example+xml")
    );
    assert_eq!(
        miss(shape(BodyShape::MultipartPresent), record, &sdp),
        pair("multipart/*", "application/sdp")
    );
    assert_eq!(
        miss(shape(BodyShape::SdpPresent), record, &invite(None, b"v=0\r\n")),
        pair("application/sdp", "untyped")
    );

    // A resource states its type by `content-type`; unstated, it is SDP.
    assert_eq!(miss(resource(None), record, &sdp), None);
    assert_eq!(
        miss(resource(None), record, &xml),
        pair("application/sdp", "application/example+xml")
    );
    assert_eq!(
        miss(resource(Some("Application/Example+XML")), record, &xml),
        None,
        "bare, either case"
    );
    assert_eq!(
        miss(
            resource(Some("application/example+xml")),
            record,
            &invite(Some("application/example+xml"), b"<y/>")
        ),
        None,
        "the content is the confrontation's"
    );
    assert_eq!(
        miss(resource(Some("application/example+xml")), record, &sdp),
        pair("application/example+xml", "application/sdp")
    );
    assert_eq!(miss(resource(None), record, &bodiless), pair("application/sdp", "absent"));
}

/// A multipart expectation's type is its container's bare type, its part
/// count and each part's bare type by position; the framing is never part of
/// it.
#[test]
fn a_multipart_type_is_its_container_its_part_count_and_its_part_types() {
    let stated = || multipart(&["application/sdp", "application/vnd.example.blob"]);
    let two = framed("b1", &["application/sdp", "application/vnd.example.blob"], false);
    for check in [CheckMode::Record, CheckMode::Assert] {
        let arrived = invite(Some("multipart/mixed;boundary=b1"), &two);
        assert_eq!(miss(stated(), check, &arrived), None);

        let reframed = framed(
            "another-boundary",
            &["application/sdp", "application/vnd.example.blob;v=2"],
            true,
        );
        let arrived =
            invite(Some("Multipart/Mixed; boundary=\"another-boundary\"; x=y"), &reframed);
        assert_eq!(
            miss(stated(), check, &arrived),
            None,
            "boundary, preamble, delimiters and parameters are framing"
        );

        let one = framed("b1", &["application/sdp"], false);
        assert_eq!(
            miss(stated(), check, &invite(Some("multipart/mixed;boundary=b1"), &one)),
            pair(
                "multipart/mixed (application/sdp, application/vnd.example.blob)",
                "multipart/mixed (application/sdp)"
            )
        );
        let swapped = framed("b1", &["application/vnd.example.blob", "application/sdp"], false);
        assert_eq!(
            miss(stated(), check, &invite(Some("multipart/mixed;boundary=b1"), &swapped)),
            pair(
                "multipart/mixed (application/sdp, application/vnd.example.blob)",
                "multipart/mixed (application/vnd.example.blob, application/sdp)"
            )
        );
        assert_eq!(
            miss(stated(), check, &invite(Some("multipart/related;boundary=b1"), &two)),
            pair(
                "multipart/mixed (application/sdp, application/vnd.example.blob)",
                "multipart/related (application/sdp, application/vnd.example.blob)"
            )
        );
    }
    // The shape states a container and no parts.
    let related = invite(Some("multipart/related;boundary=b1"), &two);
    assert_eq!(miss(shape(BodyShape::MultipartPresent), CheckMode::Record, &related), None);
}

/// Under `assert` the container type gates the MATCH; the parts do not, and
/// a part that does not hold is a finding on the datagram the step took.
#[test]
fn an_asserted_multipart_takes_the_datagram_and_finds_its_parts() {
    let config = lane();
    let scope = Scope::new(&config, None);
    let expect = invite_expect(
        multipart(&["application/sdp", "application/vnd.example.blob"]),
        CheckMode::Assert,
    );
    let one =
        invite(Some("multipart/mixed;boundary=b1"), &framed("b1", &["application/sdp"], false));
    assert!(content_holds(&expect, &one, &scope, &plain()).matches());
    assert!(matches!(body_type_finding(&expect, &one, &scope), Some(BodyTypeFinding::Gating(_))));
}

/// A lane that declares the known bug keeps taking the datagram, so the run
/// reaches the steps behind the symptom; a lane that declares nothing still
/// refuses it under `assert`. What the body type found is recorded either way,
/// under `record` exactly as under `assert`.
#[test]
fn a_lane_declared_known_bug_takes_the_body_out_of_the_match_and_records_what_it_found() {
    let mut arrived = inbound_response(180, "INVITE");
    arrived.body = b"v=0\r\n".to_vec();
    arrived.content_type = Some("application/sdp".into());
    let provisional = |check| {
        step(
            MsgSpec {
                status: Some(180),
                cseq_method: Some("INVITE".into()),
                body: Some(shape(BodyShape::Absent)),
                ..MsgSpec::default()
            },
            check,
        )
    };
    let finding = Failure::CheckFailed {
        site: "step \"s1\"".into(),
        field: "body".into(),
        op: "type".into(),
        expected: "absent".into(),
        observed: "application/sdp".into(),
    };

    let strict = lane();
    let strict = Scope::new(&strict, None);
    assert_eq!(
        content_holds(&provisional(CheckMode::Assert), &arrived, &strict, &plain()),
        GateVerdict::Rejects("body must be absent; 5 bytes arrived".into()),
        "a lane declaring nothing refuses the datagram"
    );
    assert_eq!(
        body_type_finding(&provisional(CheckMode::Record), &arrived, &strict),
        Some(BodyTypeFinding::Gating(Finding::gating(finding.clone()))),
        "and fails the run where the step records"
    );

    let waiving = lane().with_known_bug(KnownBug::ProvisionalRewriteNotApplied);
    let waiving = Scope::new(&waiving, None);
    for check in [CheckMode::Assert, CheckMode::Record] {
        let step = provisional(check);
        assert!(content_holds(&step, &arrived, &waiving, &plain()).matches());
        assert_eq!(
            body_type_finding(&step, &arrived, &waiving),
            Some(BodyTypeFinding::Waived(KnownBug::ProvisionalRewriteNotApplied, finding.clone())),
            "{check:?}"
        );
    }
}

/// The waiver names a body on a provisional and nothing else: a final carrying
/// one, and a provisional missing one it owes, still gate on the waiving lane.
#[test]
fn the_waiver_reaches_no_other_body_type() {
    let config = lane().with_known_bug(KnownBug::ProvisionalRewriteNotApplied);
    let scope = Scope::new(&config, None);
    let expect = |status, mode| {
        step(
            MsgSpec {
                status: Some(status),
                cseq_method: Some("INVITE".into()),
                body: Some(shape(mode)),
                ..MsgSpec::default()
            },
            CheckMode::Record,
        )
    };
    let mut final_with_body = inbound_response(200, "INVITE");
    final_with_body.body = b"v=0\r\n".to_vec();
    final_with_body.content_type = Some("application/sdp".into());
    assert!(matches!(
        body_type_finding(&expect(200, BodyShape::Absent), &final_with_body, &scope),
        Some(BodyTypeFinding::Gating(_))
    ));
    assert!(matches!(
        body_type_finding(
            &expect(183, BodyShape::SdpPresent),
            &inbound_response(183, "INVITE"),
            &scope
        ),
        Some(BodyTypeFinding::Gating(_))
    ));
}

/// A body captured with no `Content-Type` is stored with an empty one: relayed
/// as it came, it holds, and a typed arrival does not.
#[test]
fn a_body_stored_untyped_holds_the_same_untyped_arrival() {
    let stated = || resource(Some(""));
    assert_eq!(miss(stated(), CheckMode::Record, &invite(None, b"opaque")), None);
    assert_eq!(miss(stated(), CheckMode::Record, &invite(Some(""), b"opaque")), None);
    assert_eq!(
        miss(stated(), CheckMode::Record, &invite(Some("application/sdp"), b"v=0\r\n")),
        pair("untyped", "application/sdp")
    );
}
