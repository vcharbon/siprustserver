use super::*;
use crate::state::RunState;
use pivot_schema::bundle::IdentityBindings;

fn inbound() -> Inbound {
    Inbound {
        method: None,
        status: Some(200),
        reason: Some("OK".into()),
        cseq_method: "INVITE".into(),
        cseq: 1,
        call_id: "call-a@h".into(),
        rseq: None,
        headers: vec![("To".into(), "<sip:bob@h>;tag=t7".into())].into(),
        body: vec![],
        content_type: None,
        ruri_user: None,
        ruri: None,
        from_tag: Some("a1".into()),
        to_tag: Some("t7".into()),
        from_user: Some("0009001".into()),
        to_user: Some("bob".into()),
        from_uri: Some("sip:0009001@h;user=phone".into()),
        to_uri: Some("sip:bob@h".into()),
    }
}

fn check(field: &str, op: CheckOp, value: Option<&str>) -> Check {
    Check { field: field.into(), op, value: value.map(str::to_string), class: None }
}

fn run<'a>(
    check: &Check,
    inbound: &Inbound,
    state: &'a RunState,
    bindings: &'a IdentityBindings,
) -> Option<Failure> {
    let resolver = Resolver::new(state, bindings);
    evaluate("s1", check, &MessageObservables(inbound), &resolver)
}

#[test]
fn every_op_reads_the_message_and_says_what_it_saw() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let m = inbound();
    assert!(run(&check("status", CheckOp::Eq, Some("200")), &m, &state, &bindings).is_none());
    let failed = run(&check("status", CheckOp::Eq, Some("486")), &m, &state, &bindings);
    assert!(
        matches!(&failed, Some(Failure::CheckFailed { expected, observed, .. })
            if expected == "486" && observed == "200"),
        "{failed:?}"
    );
    assert!(run(&check("header(To)", CheckOp::Exists, None), &m, &state, &bindings).is_none());
    assert!(run(&check("header(Replaces)", CheckOp::Absent, None), &m, &state, &bindings).is_none());
    assert!(run(&check("header(To)", CheckOp::Absent, None), &m, &state, &bindings).is_some());
    assert!(run(&check("header(To)", CheckOp::Regex, Some("tag=t\\d+")), &m, &state, &bindings)
        .is_none());
    assert!(
        run(&check("header(To)", CheckOp::Regex, Some("^nope$")), &m, &state, &bindings).is_some()
    );
}

/// `tags(…)` reads every row of every named header: an option tag only a
/// second `Supported` row or a `Require` lists is found.
#[test]
fn the_tags_selector_reads_every_row_of_every_named_header() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let mut m = inbound();
    m.headers = vec![
        ("Supported".into(), "timer".into()),
        ("k".into(), "replaces, 100rel".into()),
        ("Require".into(), "precondition".into()),
    ]
    .into();
    let tags =
        check("tags(Supported,Require)", CheckOp::Eq, Some("timer,replaces,100rel,precondition"));
    assert!(run(&tags, &m, &state, &bindings).is_none(), "{:?}", run(&tags, &m, &state, &bindings));
}

/// A value bound into a pattern is a literal there: the `+` of an E.164
/// number matches a plus sign.
#[test]
fn a_bound_value_is_a_literal_inside_a_regex() {
    let state = RunState::new();
    let bindings = IdentityBindings::new().bind("caller", "e164", "+15550100");
    let mut m = inbound();
    m.headers = vec![("P-Asserted-Identity".into(), "<sip:+15550100@h>".into())].into();
    let pattern =
        check("header(P-Asserted-Identity)", CheckOp::Regex, Some("^<sip:${num:caller:e164}@"));
    assert!(run(&pattern, &m, &state, &bindings).is_none());
}

#[test]
fn a_check_value_resolves_through_the_run_s_own_state() {
    let mut state = RunState::new();
    state.leg_mut("A").remote_tag = Some("t7".into());
    let bindings = IdentityBindings::new();
    let matched = check("header(To)", CheckOp::Regex, Some("tag=${leg:A.remote-tag}$"));
    assert!(run(&matched, &inbound(), &state, &bindings).is_none());
    let unresolvable = check("status", CheckOp::Eq, Some("${leg:Z.remote-tag}"));
    assert!(matches!(
        run(&unresolvable, &inbound(), &state, &bindings),
        Some(Failure::AccessorUnresolved { .. })
    ));
}

#[test]
fn the_dialog_selectors_the_spec_names_read_off_the_message() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let m = inbound();
    for (field, value) in [
        ("from.tag", "a1"),
        ("to.tag", "t7"),
        ("from.userInfo", "0009001"),
        ("to.userInfo", "bob"),
        ("from.uri", "sip:0009001@h;user=phone"),
        ("to.uri", "sip:bob@h"),
    ] {
        assert!(
            run(&check(field, CheckOp::Eq, Some(value)), &m, &state, &bindings).is_none(),
            "{field}"
        );
        assert!(run(&check(field, CheckOp::Exists, None), &m, &state, &bindings).is_none());
    }
    // The spec's own example: a tag compared against what the run minted.
    let mut state = RunState::new();
    state.leg_mut("A").remote_tag = Some("t7".into());
    let pinned = check("to.tag", CheckOp::Eq, Some("${leg:A.remote-tag}"));
    assert!(run(&pinned, &m, &state, &bindings).is_none());
    // A tag the message does not carry is ABSENT, and `eq` says so.
    let mut untagged = inbound();
    untagged.to_tag = None;
    assert!(run(&check("to.tag", CheckOp::Absent, None), &untagged, &state, &bindings).is_none());
    let failed = run(&check("to.tag", CheckOp::Eq, Some("t7")), &untagged, &state, &bindings);
    assert!(
        matches!(&failed, Some(Failure::CheckFailed { observed, .. }) if observed == "absent"),
        "{failed:?}"
    );
}

/// `eq` on a header is a comparison of wire FORMS. Whitespace
/// around a list separator is layout (RFC 3261 §7.3.1 / §25.1); a value
/// difference is still a difference, and a non-header selector is untouched.
#[test]
fn eq_on_a_header_reads_its_form_and_on_anything_else_its_bytes() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let mut m = inbound();
    m.headers = vec![
        ("Reason".into(), "Q.850; cause=16".into()),
        ("Allow".into(), "INVITE, ACK, BYE".into()),
    ]
    .into();
    let eq = |field: &str, value: &str| check(field, CheckOp::Eq, Some(value));
    assert!(run(&eq("header(Reason)", "Q.850;cause=16"), &m, &state, &bindings).is_none());
    assert!(run(&eq("header(Allow)", "INVITE,ACK,BYE"), &m, &state, &bindings).is_none());
    assert!(run(&eq("header(Reason)", "Q.850;cause=127"), &m, &state, &bindings).is_some());
    assert!(run(&eq("header(Allow)", "INVITE,ACK"), &m, &state, &bindings).is_some());
    // A status is not a header list: it compares as the string it is.
    assert!(run(&eq("status", " 200"), &m, &state, &bindings).is_some());
}

/// `body` observes the body as the text it is where the bytes are UTF-8 and
/// as standard base64 where they are not — chosen by the bytes alone, so a
/// byte the text form could not carry is never replaced on the way to a
/// comparison. `body.b64` observes the base64 always: the byte-exact
/// assertion an author writes for a binary body.
#[test]
fn a_body_is_observed_as_text_when_utf8_and_as_base64_otherwise() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let eq = |field: &str, value: &str| check(field, CheckOp::Eq, Some(value));

    let mut text = inbound();
    text.body = b"hello\r\n".to_vec();
    assert!(run(&eq("body", "hello\r\n"), &text, &state, &bindings).is_none());
    assert!(run(&eq("body.b64", "aGVsbG8NCg=="), &text, &state, &bindings).is_none());

    let mut binary = inbound();
    binary.body = vec![0x00, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x00];
    assert!(run(&eq("body", "AAEC//6AAA=="), &binary, &state, &bindings).is_none());
    assert!(run(&eq("body.b64", "AAEC//6AAA=="), &binary, &state, &bindings).is_none());
    let failed = run(&eq("body", "AAEC//6AAQ=="), &binary, &state, &bindings);
    assert!(
        matches!(&failed, Some(Failure::CheckFailed { observed, .. }) if observed == "AAEC//6AAA=="),
        "the observed side is the base64, never a lossy text: {failed:?}"
    );
    assert!(run(&check("body.b64", CheckOp::Exists, None), &binary, &state, &bindings).is_none());
}

/// `part(<type>)` observes the first entity part of that media type (RFC 2045
/// §5.1, RFC 2046 §5.1), read as `body` reads a body: a multipart's parts, or a
/// single-part body as its one part. A body holding no such part observes none.
#[test]
fn a_part_is_observed_by_its_media_type() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let exists = |field: &str| check(field, CheckOp::Exists, None);

    let mut multipart = inbound();
    multipart.content_type = Some("multipart/mixed;boundary=b1".into());
    multipart.body = b"--b1\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n\r\n--b1\r\n\
Content-Type: application/x-data\r\n\r\n\xff\xfe\r\n--b1--\r\n"
        .to_vec();
    assert!(run(&exists("part(application/sdp)"), &multipart, &state, &bindings).is_none());
    assert!(run(&exists("part(Application/SDP)"), &multipart, &state, &bindings).is_none());
    assert!(run(
        &check("part(application/sdp)", CheckOp::Eq, Some("v=0\r\n")),
        &multipart,
        &state,
        &bindings
    )
    .is_none());
    assert!(run(
        &check("part(application/x-data)", CheckOp::Eq, Some("//4=")),
        &multipart,
        &state,
        &bindings
    )
    .is_none());

    let mut single = inbound();
    single.content_type = Some("application/x-data".into());
    single.body = vec![0xff, 0xfe];
    assert!(run(&exists("part(application/sdp)"), &single, &state, &bindings).is_some());
    assert!(run(
        &check("part(application/sdp)", CheckOp::Absent, None),
        &single,
        &state,
        &bindings
    )
    .is_none());
    assert!(run(&exists("part(application/x-data)"), &single, &state, &bindings).is_none());
}

#[test]
fn a_selector_the_source_cannot_read_fails_rather_than_reading_as_absent() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let unreadable = check("sip_transactions_orphaned_total", CheckOp::Absent, None);
    let failure = run(&unreadable, &inbound(), &state, &bindings);
    assert!(
        matches!(&failure, Some(Failure::CheckFailed { observed, .. }) if observed.starts_with("unreadable")),
        "{failure:?}"
    );
}

/// The URI selectors read off a parsed message: a request's whole
/// Request-URI and its user part, the whole From and To URIs with their
/// parameters. A response has no Request-URI, so `ruri.uri` is absent there.
#[test]
fn the_uri_selectors_read_off_a_parsed_message() {
    use sip_message::parser::custom::CustomParser;
    use sip_message::SipParser;
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let parse = |raw: &str| {
        Inbound::of(&CustomParser::new().parse(raw.as_bytes()).expect("the message parses"))
    };
    let request = parse(
        "INVITE sip:+15550100@192.0.2.7:5060;user=phone SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-1\r\n\
         From: \"Alice\" <sip:+15550199@example.com;user=phone>;tag=a1\r\n\
         To: <sip:+15550100@example.net;user=phone>\r\n\
         Call-ID: c1@192.0.2.1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\n\
         Content-Length: 0\r\n\r\n",
    );
    for (field, value) in [
        ("ruri.uri", "sip:+15550100@192.0.2.7:5060;user=phone"),
        ("ruri.userInfo", "+15550100"),
        ("from.uri", "sip:+15550199@example.com;user=phone"),
        ("to.uri", "sip:+15550100@example.net;user=phone"),
    ] {
        let held = run(&check(field, CheckOp::Eq, Some(value)), &request, &state, &bindings);
        assert!(held.is_none(), "{field}: {held:?}");
    }
    let response = parse(
        "SIP/2.0 403 Forbidden\r\n\
         Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-1\r\n\
         From: <sip:+15550199@example.com;user=phone>;tag=a1\r\n\
         To: <sip:+15550100@example.net;user=phone>;tag=b1\r\n\
         Call-ID: c1@192.0.2.1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(run(&check("ruri.uri", CheckOp::Absent, None), &response, &state, &bindings).is_none());
    let to = run(
        &check("to.uri", CheckOp::Eq, Some("sip:+15550100@example.net")),
        &response,
        &state,
        &bindings,
    );
    assert!(
        matches!(&to, Some(Failure::CheckFailed { observed, .. })
            if observed == "sip:+15550100@example.net;user=phone"),
        "a parameter is part of the URI: {to:?}"
    );
}
