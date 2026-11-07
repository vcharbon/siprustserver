use super::*;

const DECL: &str = r#"<?xml version="1.0" encoding="utf-8"?>"#;

/// The response a media resource sends for a `playcollect`, as captured.
const PLAYCOLLECT_RESPONSE: &str = r#"<MediaServerControl version="1.0"><response playduration="11081" reason="timeout" playoffset="11081" text="The request has succeeded" request="playcollect" code="200" digits=""></response></MediaServerControl>"#;

fn wrap(inner: &str) -> String {
    format!(r#"{DECL}<MediaServerControl version="1.0">{inner}</MediaServerControl>"#)
}

#[test]
fn a_request_names_its_command_element_and_its_client_defined_id() {
    let body = wrap(
        r#"<request><play id="332985001"><prompt><audio url="file://a.wav"/></prompt></play></request>"#,
    );
    assert_eq!(
        request(&body),
        Some(Request { command: "play".into(), id: Some("332985001".into()) })
    );
}

#[test]
fn a_request_without_an_id_reads_as_its_command_alone() {
    let body = wrap(
        r#"<request><playcollect maxdigits="1" barge="yes"><prompt><audio url="file://a.wav"/></prompt></playcollect></request>"#,
    );
    assert_eq!(request(&body), Some(Request { command: "playcollect".into(), id: None }));
}

/// XML admits either quote around a value, and whitespace around `=`.
#[test]
fn an_id_reads_in_either_quote_on_an_empty_element() {
    let single = wrap("<request><stop id='s-1'/></request>");
    assert_eq!(request(&single), Some(Request { command: "stop".into(), id: Some("s-1".into()) }));
    let spaced = wrap("<request>\r\n  <stop  id = \"s 2\" />\r\n</request>");
    assert_eq!(request(&spaced), Some(Request { command: "stop".into(), id: Some("s 2".into()) }));
}

/// Only the attribute NAMED `id` is the id: a longer name ending in it, or
/// an `id` on the `<request>` wrapper, is not the command's.
#[test]
fn only_the_command_element_s_own_id_attribute_is_read() {
    let body = wrap(r#"<request id="wrapper"><play xid="x" uid='u'/></request>"#);
    assert_eq!(request(&body), Some(Request { command: "play".into(), id: None }));
}

/// A comment, a processing instruction and an attribute value holding `>`
/// before the command element do not move the walk.
#[test]
fn the_walk_skips_comments_and_reads_through_a_quoted_angle_bracket() {
    let body = "<!-- lead --><?pi x?>\r\n<MediaServerControl version=\"1.0\" note=\"a>b\">\
                <!-- inner --><request><playrecord id=\"r1\" dest=\"x>y\"/></request>\
                </MediaServerControl>";
    assert_eq!(
        request(body),
        Some(Request { command: "playrecord".into(), id: Some("r1".into()) })
    );
}

#[test]
fn a_response_a_notification_or_another_root_is_no_request() {
    assert_eq!(request(PLAYCOLLECT_RESPONSE), None);
    assert_eq!(request(&wrap(r#"<notification><keypress digit="1"/></notification>"#)), None);
    assert_eq!(request(r#"<other><request><play id="1"/></request></other>"#), None);
    assert_eq!(request(&wrap("<request/>")), None, "a request with no command names none");
    assert_eq!(request("not xml at all"), None);
}

#[test]
fn a_response_names_the_command_type_it_answers() {
    assert_eq!(response_request(PLAYCOLLECT_RESPONSE), Some("playcollect"));
    assert_eq!(response_request(&wrap(r#"<response request='stop' code="200"/>"#)), Some("stop"));
    assert_eq!(response_request(&wrap(r#"<request><play id="1"/></request>"#)), None);
    assert_eq!(response_request(&wrap(r#"<notification><keypress/></notification>"#)), None);
    assert_eq!(response_request(&wrap(r#"<response code="400"/>"#)), None);
}

#[test]
fn a_response_reports_its_code_and_reason() {
    assert_eq!(
        response(PLAYCOLLECT_RESPONSE),
        Some(Response {
            request: Some("playcollect".into()),
            code: Some(200),
            reason: Some("timeout".into()),
        })
    );
}

/// Each body is one shape the element's own attributes must be told from:
/// XML spacing around `=`, a `>` or `code=` inside a value, a longer name
/// ending in `code`, a comment or a look-alike element ahead of it.
#[test]
fn only_the_response_element_s_own_attributes_are_read() {
    let table: [(&str, Option<u16>, Option<&str>); 7] = [
        (
            r#"<response request = "play" code = "200" reason = "stopped"/>"#,
            Some(200),
            Some("stopped"),
        ),
        (r#"<response request="play" text="a>b" code="200"/>"#, Some(200), None),
        (r#"<response request="play" text="code=599" code="200"/>"#, Some(200), None),
        (
            r#"<response request="play" code="200" text="reason=stopped" reason="EOF"/>"#,
            Some(200),
            Some("EOF"),
        ),
        (r#"<response request="play" xcode="599" code="200"/>"#, Some(200), None),
        (
            r#"<!-- <response code="599"/> --><response request="play" code="200"/>"#,
            Some(200),
            None,
        ),
        (r#"<response request='play' code='404' reason='EOF'></response>"#, Some(404), Some("EOF")),
    ];
    for (inner, code, reason) in table {
        let read = response(&wrap(inner)).unwrap_or_else(|| panic!("{inner}"));
        assert_eq!((read.code, read.reason.as_deref()), (code, reason), "{inner}");
    }
    assert_eq!(response(&wrap(r#"<responsex code="599"/>"#)), None);
    assert_eq!(response(&wrap(r#"<request><play/></request>"#)), None);
    assert_eq!(response(r#"<response request="play" code="200"/>"#), None, "no root");
}

/// A code is three digits of a class MSCML uses (2xx, 4xx, 5xx) and nothing
/// else.
#[test]
fn a_code_off_three_digits_reads_as_none() {
    for code in ["200", "299", "404", "599"] {
        let body = wrap(&format!(r#"<response request="play" code="{code}"/>"#));
        assert_eq!(response(&body).and_then(|r| r.code).map(|c| c.to_string()), Some(code.into()));
    }
    for code in [
        "",
        "20",
        "2000",
        " 200",
        "200 ",
        "+20",
        "2x0",
        "２００",
        "099",
        "100",
        "302",
        "600",
        "999",
    ] {
        let body = wrap(&format!(r#"<response request="play" code="{code}"/>"#));
        assert_eq!(response(&body).map(|r| r.code), Some(None), "{code:?}");
    }
    assert_eq!(response(&wrap(r#"<response request="play"/>"#)).map(|r| r.code), Some(None));
}

/// An attribute is led by XML white space (SP, HTAB, CR, LF): one glued to
/// the value before it, or led by another space character, is not read, and
/// neither is any attribute after it.
#[test]
fn an_attribute_needs_xml_space_before_it() {
    let code = |inner: &str| response(&wrap(inner)).and_then(|r| r.code);
    assert_eq!(code("<response\trequest=\"play\"\r\n code\t=\n\"200\"/>"), Some(200));
    assert_eq!(code(r#"<response request="play"code="200"/>"#), None);
    assert_eq!(code("<response request=\"play\"\u{a0}code=\"200\"/>"), None);
    assert_eq!(code("<response request=\"play\"\u{c}code=\"200\"/>"), None);
    assert_eq!(code("<response request=\"play\" \u{a0}code=\"200\"/>"), None);
    let glued = response(&wrap(r#"<response request="play"code="200"/>"#));
    assert_eq!(glued.and_then(|r| r.request).as_deref(), Some("play"));
}

/// The responses are read off a bare MSCML body and off each MSCML part,
/// never off a body of another type.
#[test]
fn responses_are_read_by_the_media_type() {
    let eof = wrap(r#"<response request="play" code="200" reason="EOF"/>"#);
    let read = |ct: Option<&str>, body: &str| -> Vec<Option<u16>> {
        responses_in(ct, body.as_bytes()).into_iter().map(|r| r.code).collect()
    };
    assert_eq!(read(Some(CONTENT_TYPE), &eof), [Some(200)]);
    assert!(read(Some("application/xml"), &eof).is_empty());
    assert!(read(None, &eof).is_empty());
    let framed = format!(
        "--b1\r\nContent-Type: text/plain\r\n\r\nx\r\n\
         --b1\r\nContent-Type: {CONTENT_TYPE}\r\n\r\n{eof}\r\n--b1--\r\n"
    );
    assert_eq!(read(Some("multipart/mixed;boundary=b1"), &framed), [Some(200)]);
}

/// Inserted right after the element name; every other byte as it was.
#[test]
fn an_id_is_inserted_into_a_response_that_states_none() {
    let echoed = with_response_id(PLAYCOLLECT_RESPONSE, Some("332985001"));
    assert_eq!(
        echoed,
        PLAYCOLLECT_RESPONSE.replacen("<response ", "<response id=\"332985001\" ", 1)
    );
    assert_eq!(response_request(&echoed), Some("playcollect"));
}

#[test]
fn a_stated_id_is_replaced_in_place_keeping_its_quote() {
    let body = wrap(r#"<response request="play" id='old-1' code="200" reason="stopped"/>"#);
    assert_eq!(
        with_response_id(&body, Some("live-7")),
        wrap(r#"<response request="play" id='live-7' code="200" reason="stopped"/>"#)
    );
}

/// A value holding the stated quote takes the other one, so the attribute
/// stays well-formed.
#[test]
fn a_value_holding_the_quote_takes_the_other_quote() {
    let body = wrap(r#"<response request="play" id="old" code="200"/>"#);
    assert_eq!(
        with_response_id(&body, Some(r#"a"b"#)),
        wrap(r#"<response request="play" id='a"b' code="200"/>"#)
    );
    let bare = wrap(r#"<response request="play" code="200"/>"#);
    assert_eq!(
        with_response_id(&bare, Some(r#"a"b"#)),
        wrap(r#"<response id='a"b' request="play" code="200"/>"#)
    );
}

/// Removed with the whitespace that led it; a response stating none and
/// asked for none is unchanged.
#[test]
fn a_stated_id_is_removed_where_the_request_carried_none() {
    let body = wrap(r#"<response request="stop"  id = "old" code="200"></response>"#);
    assert_eq!(
        with_response_id(&body, None),
        wrap(r#"<response request="stop" code="200"></response>"#)
    );
    assert_eq!(with_response_id(PLAYCOLLECT_RESPONSE, None), PLAYCOLLECT_RESPONSE);
}

#[test]
fn a_body_holding_no_response_is_never_rewritten() {
    let notification = wrap(r#"<notification><keypress digit="1" id="k"/></notification>"#);
    assert_eq!(with_response_id(&notification, Some("x")), notification);
    let req = wrap(r#"<request><play id="1"/></request>"#);
    assert_eq!(with_response_id(&req, Some("x")), req);
    assert_eq!(with_response_id(&req, None), req);
}

#[test]
fn the_media_type_is_read_by_token() {
    assert!(is_mscml("application/mediaservercontrol+xml"));
    assert!(is_mscml(" Application/MediaServerControl+XML ; charset=utf-8"));
    assert!(!is_mscml("application/xml"));
    assert!(!is_mscml("application/mediaservercontrol+xml-x"));
}

/// A bare MSCML body is one request; a multipart body yields each MSCML
/// part's; any other type, or an empty body, yields none.
#[test]
fn requests_are_read_off_a_bare_body_and_off_each_mscml_part() {
    let play = wrap(r#"<request><play id="p1"/></request>"#);
    assert_eq!(
        requests_in(Some(CONTENT_TYPE), play.as_bytes()),
        vec![Request { command: "play".into(), id: Some("p1".into()) }]
    );
    assert!(requests_in(Some("application/xml"), play.as_bytes()).is_empty());
    assert!(requests_in(None, play.as_bytes()).is_empty());
    assert!(requests_in(Some(CONTENT_TYPE), b"").is_empty());

    let framed = format!(
        "--b1\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n\r\n\
         --b1\r\nContent-Type: {CONTENT_TYPE}\r\n\r\n{play}\r\n--b1--\r\n"
    );
    assert_eq!(
        requests_in(Some("multipart/mixed;boundary=b1"), framed.as_bytes()),
        vec![Request { command: "play".into(), id: Some("p1".into()) }]
    );
}
