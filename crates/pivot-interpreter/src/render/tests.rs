use super::*;
use crate::state::RunState;
use pivot_schema::bundle::{ClockMode, IdentityBindings};
use pivot_schema::msg::{Header, PositionalRef};
use sip_message::header::Uri;
use sip_message::SipParser;

struct TestComposer;

impl UriComposer for TestComposer {
    fn compose(&self, pos: &str, form: Option<&str>, is_ruri: bool) -> Option<String> {
        let host = if is_ruri { "10.0.0.9:5080" } else { "lane.invalid" };
        match (pos, form) {
            ("caller", _) => Some(format!("sip:0009001@{host}")),
            ("called[0][0]", Some("trunk-composed")) => Some(format!("sip:+1999999900004@{host}")),
            _ => None,
        }
    }
    fn frozen(&self, value: &str, kind: Option<&str>, is_ruri: bool) -> String {
        let host = if is_ruri { "10.0.0.9:5080" } else { "lane.invalid" };
        if Uri::value_has_scheme(value) {
            value.to_string()
        } else if kind.is_some() || value.contains('@') {
            format!("sip:{value}")
        } else {
            format!("sip:{value}@{host}")
        }
    }
}

fn config() -> RunConfig {
    RunConfig::new("upstream-fake", ClockMode::Virtual, "10.0.0.9:5080")
        .with_header("X-Lane", "upstream-fake")
}

#[test]
fn a_send_carries_its_frozen_headers_its_tier_two_uris_and_the_lane_s_own_headers() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let resolver = Resolver::new(&state, &bindings);
    let msg = MsgSpec {
        method: Some("INVITE".into()),
        ruri: Some(Ref::Positional(PositionalRef {
            pos: "called[0][0]".into(),
            form: Some("trunk-composed".into()),
            addr: None,
        })),
        from: Some(Ref::Positional(PositionalRef {
            pos: "caller".into(),
            form: Some("private".into()),
            addr: None,
        })),
        headers: vec![Header {
            name: "X-Vendor-Orig".into(),
            value: "sbc.111".into(),
            class: None,
        }],
        ..MsgSpec::default()
    };
    let rendered = render(
        &msg,
        "step \"s1\"",
        &Context {
            resolver: &resolver,
            config: &config(),
            effects: &StepEffects::default(),
            composer: &TestComposer,
            base_dir: Path::new("."),
            media: &Booking::new("127.0.0.1", 40000),
            leg: "A",
            mscml: &MscmlRequests::default(),
            call_headers: &BTreeMap::new(),
        },
    )
    .expect("the message composes");
    assert_eq!(rendered.ruri.as_deref(), Some("sip:+1999999900004@10.0.0.9:5080"));
    assert_eq!(rendered.from.as_deref(), Some("sip:0009001@lane.invalid"));
    let names: Vec<&str> = rendered.template.headers().iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, ["X-Vendor-Orig", "X-Lane"], "the lane's own header rides last");
    assert_eq!(rendered.template.method(), Some(&Method::Invite));
}

#[test]
fn a_ref_that_states_its_name_addr_renders_it_around_the_bound_number() {
    let state = RunState::new();
    let bindings = IdentityBindings::new().bind("caller", "e164", "+15556000777");
    let resolver = Resolver::new(&state, &bindings);
    let msg: MsgSpec = serde_json::from_str(
        r#"{
              "method": "INVITE",
              "from": { "pos": "caller", "form": "e164",
                        "addr": "\"Alice\" <sip:${num:caller:e164};verstat=x@h.invalid;user=phone>;x-param=1" },
              "to": { "frozen": "+15556000004",
                      "addr": "<sip:+15556000004@example.invalid;x-uri=2;user=phone>" }
            }"#,
    )
    .expect("a message");
    let rendered = render(
        &msg,
        "step \"s1\"",
        &Context {
            resolver: &resolver,
            config: &config(),
            effects: &StepEffects::default(),
            composer: &TestComposer,
            base_dir: Path::new("."),
            media: &Booking::new("127.0.0.1", 40000),
            leg: "A",
            mscml: &MscmlRequests::default(),
            call_headers: &BTreeMap::new(),
        },
    )
    .expect("the message composes");
    assert_eq!(
        rendered.from.as_deref(),
        Some("\"Alice\" <sip:+15556000777;verstat=x@h.invalid;user=phone>;x-param=1")
    );
    assert_eq!(
        rendered.to.as_deref(),
        Some("<sip:+15556000004@example.invalid;x-uri=2;user=phone>")
    );
}

#[test]
fn a_name_addr_that_does_not_read_or_rides_a_request_uri_is_refused() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let resolver = Resolver::new(&state, &bindings);
    let cx = Context {
        resolver: &resolver,
        config: &config(),
        effects: &StepEffects::default(),
        composer: &TestComposer,
        base_dir: Path::new("."),
        media: &Booking::new("127.0.0.1", 40000),
        leg: "A",
        mscml: &MscmlRequests::default(),
        call_headers: &BTreeMap::new(),
    };
    for (field, addr) in
        [("from", "\"open <sip:x@h"), ("to", "<sip:x@h>;tag=t1"), ("ruri", "<sip:x@h>")]
    {
        let msg: MsgSpec = serde_json::from_value(serde_json::json!({
            "method": "INVITE",
            field: { "frozen": "x", "addr": addr }
        }))
        .expect("a message");
        let refused = render(&msg, "step \"s1\"", &cx);
        assert!(
            matches!(refused, Err(RenderError::AddrRefused { .. })),
            "{field} {addr}: {refused:?}"
        );
    }
}

#[test]
fn a_call_s_own_directive_rides_its_dial_and_outranks_the_run_wide_header() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let resolver = Resolver::new(&state, &bindings);
    let msg = MsgSpec { method: Some("INVITE".into()), ..MsgSpec::default() };
    let call_headers = BTreeMap::from([
        ("X-Api-Call".to_string(), "{\"destination\":\"idle\"}".to_string()),
        ("X-Lane".to_string(), "this-call".to_string()),
    ]);
    let rendered = render(
        &msg,
        "step \"s10\"",
        &Context {
            resolver: &resolver,
            config: &config(),
            effects: &StepEffects::default(),
            composer: &TestComposer,
            base_dir: Path::new("."),
            media: &Booking::new("127.0.0.1", 40000),
            leg: "A",
            mscml: &MscmlRequests::default(),
            call_headers: &call_headers,
        },
    )
    .expect("the message composes");
    let value = |name: &str| {
        rendered
            .template
            .headers()
            .iter()
            .find(|h| h.name == name)
            .map(|h| h.value.clone())
            .unwrap_or_default()
    };
    assert_eq!(value("X-Api-Call"), "{\"destination\":\"idle\"}");
    assert_eq!(value("X-Lane"), "this-call", "the more specific statement wins");
    // And exactly once: a run-wide header the call restated is not stamped twice.
    assert_eq!(rendered.template.headers().iter().filter(|h| h.name == "X-Lane").count(), 1);
}

#[test]
fn an_accessor_inside_a_header_value_resolves_against_run_state() {
    let mut state = RunState::new();
    let leg = state.leg_mut("B");
    leg.call_id = Some("cid-b".into());
    leg.remote_tag = Some("t9".into());
    let bindings = IdentityBindings::new().bind("transferee", "e164", "+15550900006");
    let resolver = Resolver::new(&state, &bindings);
    let msg = MsgSpec {
        method: Some("REFER".into()),
        headers: vec![Header {
            name: "Refer-To".into(),
            value: "<sip:${num:transferee:e164}@h?Replaces=${leg:B.call-id}%3Bto-tag%3D${leg:B.remote-tag}>"
                .into(),
            class: None,
        }],
        ..MsgSpec::default()
    };
    let rendered = render(
        &msg,
        "step \"s8\"",
        &Context {
            resolver: &resolver,
            config: &config(),
            effects: &StepEffects::default(),
            composer: &TestComposer,
            base_dir: Path::new("."),
            media: &Booking::new("127.0.0.1", 40000),
            leg: "A",
            mscml: &MscmlRequests::default(),
            call_headers: &BTreeMap::new(),
        },
    )
    .expect("the message composes");
    assert_eq!(
        rendered.template.headers()[0].value,
        "<sip:+15550900006@h?Replaces=cid-b%3Bto-tag%3Dt9>"
    );
}

#[test]
fn an_unresolvable_accessor_refuses_the_send_rather_than_emitting_a_hole() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let resolver = Resolver::new(&state, &bindings);
    let msg = MsgSpec {
        method: Some("REFER".into()),
        headers: vec![Header {
            name: "Refer-To".into(),
            value: "<sip:x@h?Replaces=${leg:B.call-id}>".into(),
            class: None,
        }],
        ..MsgSpec::default()
    };
    let error = render(
        &msg,
        "step \"s8\"",
        &Context {
            resolver: &resolver,
            config: &config(),
            effects: &StepEffects::default(),
            composer: &TestComposer,
            base_dir: Path::new("."),
            media: &Booking::new("127.0.0.1", 40000),
            leg: "A",
            mscml: &MscmlRequests::default(),
            call_headers: &BTreeMap::new(),
        },
    )
    .unwrap_err();
    assert!(matches!(error, RenderError::Accessor { .. }), "{error}");
}

#[test]
fn a_body_reference_that_labels_itself_neither_sdp_nor_typed_is_refused() {
    let dir = std::env::temp_dir().join(format!("pivot-render-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("part.bin"), b"\x00\x01").unwrap();
    let unlabelled = Body::Resource(pivot_schema::body::ResourceBody {
        reference: "part.bin".into(),
        rewrite: vec![],
        mode: None,
        content_type: None,
        compare: None,
    });
    assert!(matches!(
        load_body(
            &unlabelled,
            &dir,
            &Booking::new("127.0.0.1", 40000),
            "A",
            &mut Echo::new(&MscmlRequests::default())
        ),
        Err(RenderError::BodyUnlabelled { .. })
    ));
    let typed = Body::Resource(pivot_schema::body::ResourceBody {
        reference: "part.bin".into(),
        rewrite: vec![],
        mode: Some(pivot_schema::body::BodyMode::Frozen),
        content_type: Some("application/EmergencyCallData.eCall.MSD".into()),
        compare: None,
    });
    let (bytes, ct) = load_body(
        &typed,
        &dir,
        &Booking::new("127.0.0.1", 40000),
        "A",
        &mut Echo::new(&MscmlRequests::default()),
    )
    .unwrap();
    assert_eq!(bytes, b"\x00\x01");
    assert_eq!(ct.as_deref(), Some("application/EmergencyCallData.eCall.MSD"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The multipart shape the corpus carries (an INVITE whose part 0 is the
/// offer and whose part 1 is the deployment's own payload): every part rides
/// byte-exact under its own type, the SDP part takes the lane's media
/// address, and the framing states the boundary it used.
#[test]
fn a_multipart_body_frames_each_part_under_its_own_type() {
    let dir = std::env::temp_dir().join(format!("pivot-render-mp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("uac1_0_0.sdp"),
        "v=0\r\nc=IN IP4 1.2.3.4\r\nm=audio 16804 RTP/AVP 8\r\n",
    )
    .unwrap();
    let blob: &[u8] = &[0x77, 0x15, 0x47, 0x00, 0x83, 0x0a];
    std::fs::write(dir.join("uac1_0_1.bin"), blob).unwrap();
    let body = Body::Multipart(pivot_schema::body::MultipartBody {
        multipart: pivot_schema::body::Multipart {
            content_type: "multipart/mixed".into(),
            parts: vec![
                pivot_schema::body::Part {
                    content_type: "application/sdp".into(),
                    reference: "uac1_0_0.sdp".into(),
                    rewrite: vec!["c=addr".into(), "m=port".into()],
                    mode: None,
                    compare: None,
                    content_id: None,
                    headers: vec![],
                    cid_linked: vec![],
                },
                pivot_schema::body::Part {
                    content_type: "application/vnd.example.blob".into(),
                    reference: "uac1_0_1.bin".into(),
                    rewrite: vec![],
                    mode: Some(pivot_schema::body::BodyMode::Frozen),
                    compare: None,
                    content_id: Some("<blob@example.invalid>".into()),
                    headers: vec![
                        pivot_schema::body::EntityHeader {
                            name: "Content-Transfer-Encoding".into(),
                            value: "binary".into(),
                        },
                        pivot_schema::body::EntityHeader {
                            name: "Content-Disposition".into(),
                            value: "signal;handling=optional".into(),
                        },
                    ],
                    cid_linked: vec![],
                },
            ],
        },
    });
    let media = Booking::new("127.0.0.9", 41000);
    let (bytes, ct) =
        load_body(&body, &dir, &media, "A", &mut Echo::new(&MscmlRequests::default())).unwrap();
    let ct = ct.expect("a multipart body states the type it framed under");
    let boundary = ct.split("boundary=").nth(1).expect("the type names its boundary");
    assert!(ct.starts_with("multipart/mixed;boundary="), "{ct}");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(text.starts_with(&format!("--{boundary}\r\nContent-Type: application/sdp\r\n\r\n")));
    assert!(text.contains("c=IN IP4 127.0.0.9\r\n"), "the SDP part took the lane's address");
    assert_eq!(media.held("A", 0), Some(41000), "the SDP part booked the leg's first stream");
    assert!(
        text.contains("m=audio 41000 RTP/AVP 8\r\n"),
        "the SDP part took the port the lane booked it:\n{text}"
    );
    assert!(text.contains("Content-Type: application/vnd.example.blob\r\n"));
    // The stored entity block rides verbatim, `Content-ID` first.
    assert!(
        text.contains(
            "Content-Type: application/vnd.example.blob\r\n\
             Content-ID: <blob@example.invalid>\r\n\
             Content-Transfer-Encoding: binary\r\n\
             Content-Disposition: signal;handling=optional\r\n\r\n"
        ),
        "{text}"
    );
    assert!(bytes.windows(blob.len()).any(|w| w == blob), "the binary part rides byte-exact");
    assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A `cid-linked` part with no `content-id` of its own is REFUSED: the
/// header referencing it would point at nothing. A document written before
/// the id was stored fails loudly rather than emitting a stranded link.
#[test]
fn a_cid_linked_part_without_a_content_id_refuses_rather_than_stranding_its_header() {
    let body = Body::Multipart(pivot_schema::body::MultipartBody {
        multipart: pivot_schema::body::Multipart {
            content_type: "multipart/mixed".into(),
            parts: vec![pivot_schema::body::Part {
                content_type: "application/EmergencyCallData.eCall.MSD".into(),
                reference: "resources/uac1_0_1.bin".into(),
                rewrite: vec![],
                mode: Some(pivot_schema::body::BodyMode::Frozen),
                compare: None,
                content_id: None,
                headers: vec![],
                cid_linked: vec!["call-info".into()],
            }],
        },
    });
    let error = load_body(
        &body,
        Path::new("."),
        &Booking::new("127.0.0.1", 40000),
        "A",
        &mut Echo::new(&MscmlRequests::default()),
    )
    .unwrap_err();
    assert!(
        matches!(&error, RenderError::MultipartCidUnstated { headers, .. } if headers == &["call-info"]),
        "{error}"
    );
}

/// The same part stating its id EMITS, under that id: `cid-linked` is a
/// link record, not a refusal.
#[test]
fn a_cid_linked_part_stating_its_content_id_emits_under_it() {
    let dir = std::env::temp_dir().join(format!("pivot-render-cid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("msd.bin"), [0x01, 0x02, 0x03]).unwrap();
    let body = Body::Multipart(pivot_schema::body::MultipartBody {
        multipart: pivot_schema::body::Multipart {
            content_type: "multipart/mixed".into(),
            parts: vec![pivot_schema::body::Part {
                content_type: "application/EmergencyCallData.eCall.MSD".into(),
                reference: "msd.bin".into(),
                rewrite: vec![],
                mode: Some(pivot_schema::body::BodyMode::Frozen),
                compare: None,
                content_id: Some("<msd@example.invalid>".into()),
                headers: vec![pivot_schema::body::EntityHeader {
                    name: "Content-Transfer-Encoding".into(),
                    value: "binary".into(),
                }],
                cid_linked: vec!["call-info".into()],
            }],
        },
    });
    let (bytes, _) = load_body(
        &body,
        &dir,
        &Booking::new("127.0.0.1", 40000),
        "A",
        &mut Echo::new(&MscmlRequests::default()),
    )
    .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(text.contains("Content-ID: <msd@example.invalid>\r\n"), "{text}");
    assert!(text.contains("Content-Transfer-Encoding: binary\r\n"), "{text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Both tokens, on the body the corpus's ordinary offer is: the address and
/// the port are the lane's, the `o=` line's address is NOT (it is not a
/// connection line), and the framing is untouched.
#[test]
fn an_sdp_body_takes_the_lane_s_media_address_and_its_booked_port() {
    let sdp = "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\nc=IN IP4 1.2.3.4\r\nm=audio 5000 RTP/AVP 0\r\n";
    let media = Booking::new("127.0.0.1", 40000);
    let mut stream = 0usize;
    let out = rewrite_sdp(sdp, both(), &media, "A", &mut stream);
    assert_eq!(
        out,
        "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\nc=IN IP4 127.0.0.1\r\nm=audio 40000 RTP/AVP 0\r\n"
    );
    assert_eq!(media.held("A", 0), Some(40000));
    assert_eq!(stream, 1, "the one active stream took one booking");
}

/// The port field is the ONLY thing an `m=` line loses: the media kind, the
/// `<port>/<count>` pair count and the whole format list ride byte-exact,
/// and so does every attribute line around them (RFC 4566 §5.14).
#[test]
fn a_media_line_keeps_everything_but_its_port_including_a_port_count() {
    let sdp = "v=0\r\n\
                   c=IN IP4 1.2.3.4\r\n\
                   m=audio 5000/2 RTP/AVP 8 101 0\r\n\
                   a=rtpmap:101 telephone-event/8000\r\n\
                   a=fmtp:101 0-15\r\n\
                   a=sendrecv\r\n\
                   m=image 5008 udptl t38\r\n\
                   a=T38FaxVersion:0\r\n";
    let media = Booking::new("127.0.0.1", 40000);
    let mut stream = 0usize;
    let out = rewrite_sdp(sdp, both(), &media, "B", &mut stream);
    assert_eq!(
        out,
        "v=0\r\n\
             c=IN IP4 127.0.0.1\r\n\
             m=audio 40000/2 RTP/AVP 8 101 0\r\n\
             a=rtpmap:101 telephone-event/8000\r\n\
             a=fmtp:101 0-15\r\n\
             a=sendrecv\r\n\
             m=image 40004 udptl t38\r\n\
             a=T38FaxVersion:0\r\n"
    );
    assert_eq!(stream, 2, "the corpus's audio+T.38 offer books a port per live stream");
}

/// A port-0 stream is the capture's own rejected/disabled one (RFC 3264
/// §5.1): it stays zero and does not take a booking, so the stream that
/// lives beside it books the leg's FIRST port and not its second.
#[test]
fn a_rejected_stream_keeps_its_zero_port_and_books_nothing() {
    let sdp = "m=audio 0 RTP/AVP 8\r\nm=image 6000 udptl t38\r\n";
    let media = Booking::new("127.0.0.1", 40000);
    let mut stream = 0usize;
    let out = rewrite_sdp(sdp, both(), &media, "A", &mut stream);
    assert_eq!(out, "m=audio 0 RTP/AVP 8\r\nm=image 40000 udptl t38\r\n");
    assert_eq!(media.held("A", 1), None, "only one stream was ever booked");
}

/// The tokens gate independently: a body stating `c=addr` alone keeps the
/// port the document holds, and one stating `m=port` alone keeps the
/// address. Nothing is rewritten that the document did not ask for.
#[test]
fn each_rewrite_token_gates_only_its_own_lines() {
    let sdp = "c=IN IP4 1.2.3.4\r\nm=audio 5000 RTP/AVP 0\r\n";
    let media = Booking::new("127.0.0.1", 40000);
    let mut stream = 0usize;
    let addr_only = Rewrite { addr: true, port: false };
    assert_eq!(
        rewrite_sdp(sdp, addr_only, &media, "A", &mut stream),
        "c=IN IP4 127.0.0.1\r\nm=audio 5000 RTP/AVP 0\r\n"
    );
    assert_eq!(media.held("A", 0), None, "no token, no booking");
    let port_only = Rewrite { addr: false, port: true };
    assert_eq!(
        rewrite_sdp(sdp, port_only, &media, "A", &mut stream),
        "c=IN IP4 1.2.3.4\r\nm=audio 40000 RTP/AVP 0\r\n"
    );
}

/// On a lane without media the booking answers neither token: a body
/// stating both rides byte for byte, `c=` address and `m=` port as captured,
/// under the type the tokens declare, and nothing is booked. The same body
/// on a rebooked lane is still rewritten.
#[test]
fn a_verbatim_booking_leaves_a_tokened_body_byte_for_byte() {
    let dir = std::env::temp_dir().join(format!("pivot-render-verbatim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let stored = "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\nc=IN IP4 1.2.3.4\r\nm=audio 5000 RTP/AVP 0\r\n\
                      c=IN IP4 0.0.0.0\r\nm=video 5002/2 RTP/AVP 96\r\n";
    std::fs::write(dir.join("offer.sdp"), stored).unwrap();
    let body = Body::Resource(pivot_schema::body::ResourceBody {
        reference: "offer.sdp".into(),
        rewrite: vec!["c=addr".into(), "m=port".into()],
        mode: None,
        content_type: None,
        compare: None,
    });
    let verbatim = Booking::verbatim();
    let (bytes, ct) =
        load_body(&body, &dir, &verbatim, "A", &mut Echo::new(&MscmlRequests::default())).unwrap();
    assert_eq!(String::from_utf8_lossy(&bytes), stored, "no line is rewritten");
    assert_eq!(ct.as_deref(), Some("application/sdp"), "the tokens still label the body");
    assert_eq!(verbatim.held("A", 0), None, "and the port book stays empty");

    // Byte for byte means the bytes: a `\r\r\n` ending the rewrite would
    // re-assemble and a byte no UTF-8 decoding keeps both ride unchanged.
    let odd: &[u8] = b"v=0\r\r\no=- 1 1 IN IP4 1.2.3.4\r\ns=\xff\r\nm=audio 5000 RTP/AVP 0\r\n";
    std::fs::write(dir.join("odd.sdp"), odd).unwrap();
    let Body::Resource(resource) = &body else { unreachable!() };
    let odd_body = Body::Resource(pivot_schema::body::ResourceBody {
        reference: "odd.sdp".into(),
        ..resource.clone()
    });
    let (bytes, _) =
        load_body(&odd_body, &dir, &verbatim, "A", &mut Echo::new(&MscmlRequests::default()))
            .unwrap();
    assert_eq!(bytes, odd, "the stored bytes ride unchanged");

    let rebooked = Booking::new("127.0.0.1", 40000);
    let (bytes, _) =
        load_body(&body, &dir, &rebooked, "A", &mut Echo::new(&MscmlRequests::default())).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\nc=IN IP4 127.0.0.1\r\nm=audio 40000 RTP/AVP 0\r\n\
             c=IN IP4 127.0.0.1\r\nm=video 40002/2 RTP/AVP 96\r\n",
        "a rebooked lane still rewrites"
    );
    assert_eq!(rebooked.held("A", 1), Some(40002));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A body stating NO rewrite token is not SDP at all: it rides byte-exact
/// under its stored type, ports included.
#[test]
fn a_body_stating_no_rewrite_token_keeps_its_stored_port() {
    let dir = std::env::temp_dir().join(format!("pivot-render-frozen-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let stored = "v=0\r\nc=IN IP4 1.2.3.4\r\nm=audio 5000 RTP/AVP 0\r\n";
    std::fs::write(dir.join("frozen.sdp"), stored).unwrap();
    let body = Body::Resource(pivot_schema::body::ResourceBody {
        reference: "frozen.sdp".into(),
        rewrite: vec![],
        mode: Some(pivot_schema::body::BodyMode::Frozen),
        content_type: Some("application/sdp".into()),
        compare: None,
    });
    let media = Booking::new("127.0.0.1", 40000);
    let (bytes, ct) =
        load_body(&body, &dir, &media, "A", &mut Echo::new(&MscmlRequests::default())).unwrap();
    assert_eq!(String::from_utf8_lossy(&bytes), stored, "a frozen body is not rewritten");
    assert_eq!(ct.as_deref(), Some("application/sdp"));
    assert_eq!(media.held("A", 0), None, "and books nothing");
    std::fs::remove_dir_all(&dir).unwrap();
}

const MSCML: &str = "application/mediaservercontrol+xml";

/// An MSCML body as the wire frames it.
fn mscml_body(inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><MediaServerControl version=\"1.0\">{inner}</MediaServerControl>"
    )
}

/// The leg's record after it received each of `requests` as an INFO.
fn received(requests: &[&str]) -> MscmlRequests {
    let mut record = MscmlRequests::default();
    for (cseq, inner) in requests.iter().enumerate() {
        let body = mscml_body(inner);
        let raw = format!(
            "INFO sip:ms@127.0.0.1:5070 SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-{cseq}\r\n\
                 From: <sip:as@example.invalid>;tag=a1\r\n\
                 To: <sip:ms@example.invalid>;tag=b1\r\n\
                 Call-ID: call-b\r\n\
                 CSeq: {} INFO\r\n\
                 Content-Type: {MSCML}\r\n\
                 Content-Length: {}\r\n\r\n{body}",
            cseq + 2,
            body.len()
        );
        let message = sip_message::CustomParser::new().parse(raw.as_bytes()).expect("parses");
        record.remember(&crate::gate::Inbound::of(&message));
    }
    record
}

/// The bytes a frozen MSCML resource holding `stored` emits after the leg
/// received `requests`, the record left as the send leaves it.
fn emitted(stored: &str, requests: &mut MscmlRequests) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pivot-render-mscml-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("response.xml"), stored).unwrap();
    let body = Body::Resource(pivot_schema::body::ResourceBody {
        reference: "response.xml".into(),
        rewrite: vec![],
        mode: Some(pivot_schema::body::BodyMode::Frozen),
        content_type: Some(MSCML.into()),
        compare: None,
    });
    let mut echo = Echo::new(requests);
    let (bytes, ct) =
        load_body(&body, &dir, &Booking::new("127.0.0.1", 40000), "B", &mut echo).unwrap();
    let answered = echo.answered();
    requests.answer(&answered);
    assert_eq!(ct.as_deref(), Some(MSCML), "the stored type labels the body");
    std::fs::remove_dir_all(&dir).unwrap();
    String::from_utf8(bytes).expect("UTF-8")
}

/// RFC 5022 §10.2: the response echoes the id the request carried, live,
/// and no other byte of the recorded body changes.
#[test]
fn an_mscml_response_echoes_the_id_its_request_carried() {
    let mut requests =
        received(&[r#"<request><playcollect id="332985001" maxdigits="1"/></request>"#]);
    let stored = mscml_body(
        r#"<response playduration="11081" reason="timeout" request="playcollect" code="200" digits=""></response>"#,
    );
    assert_eq!(
        emitted(&stored, &mut requests),
        mscml_body(
            r#"<response id="332985001" playduration="11081" reason="timeout" request="playcollect" code="200" digits=""></response>"#
        )
    );
}

/// Two types in flight: the stop's own response and the stopped play's
/// each take their OWN request's id, never the last request's of any type.
#[test]
fn each_response_echoes_the_request_of_its_own_type() {
    let mut requests = received(&[
        r#"<request><play id="p-1"><prompt/></play></request>"#,
        r#"<request><stop id="s-1"/></request>"#,
    ]);
    assert_eq!(
        emitted(&mscml_body(r#"<response request="stop" code="200"/>"#), &mut requests),
        mscml_body(r#"<response id="s-1" request="stop" code="200"/>"#)
    );
    assert_eq!(
        emitted(
            &mscml_body(r#"<response request="play" code="200" reason="stopped"/>"#),
            &mut requests
        ),
        mscml_body(r#"<response id="p-1" request="play" code="200" reason="stopped"/>"#)
    );
}

/// The client sent no id this time: the id the capture's response held
/// names a request this run never made, so it goes.
#[test]
fn a_recorded_id_is_removed_where_the_live_request_carried_none() {
    let mut requests = received(&[r#"<request><play><prompt/></play></request>"#]);
    assert_eq!(
        emitted(
            &mscml_body(r#"<response request="play" id="captured-9" code="200"/>"#),
            &mut requests
        ),
        mscml_body(r#"<response request="play" code="200"/>"#)
    );
}

/// A response of a type this leg never received rides as recorded, its
/// recorded id included.
#[test]
fn a_response_of_a_type_never_received_rides_byte_for_byte() {
    let mut requests = received(&[r#"<request><play id="p-1"/></request>"#]);
    let stored = mscml_body(r#"<response request="playrecord" id="captured-3" code="200"/>"#);
    assert_eq!(emitted(&stored, &mut requests), stored);
    assert_eq!(emitted(&stored, &mut MscmlRequests::default()), stored);
}

/// RFC 5022 §6: the media server does not queue, so a second `play`
/// stops the first and each still draws its own response, oldest first —
/// the stopped play's response answers the play that carried no id, the
/// one that ends at EOF answers `final`.
#[test]
fn responses_of_one_type_answer_their_requests_oldest_first() {
    let mut requests = received(&[
        r#"<request><play><prompt/></play></request>"#,
        r#"<request><play id="final"><prompt/></play></request>"#,
    ]);
    assert_eq!(
        emitted(
            &mscml_body(
                r#"<response request="play" id="captured-1" code="200" reason="stopped"/>"#
            ),
            &mut requests
        ),
        mscml_body(r#"<response request="play" code="200" reason="stopped"/>"#),
        "the stopped play answers the older request, which carried no id"
    );
    assert_eq!(
        emitted(
            &mscml_body(r#"<response request="play" code="200" reason="eof"/>"#),
            &mut requests
        ),
        mscml_body(r#"<response id="final" request="play" code="200" reason="eof"/>"#),
        "the play that ran to its end answers the newer request"
    );
}

/// A response consumes the request it answers: once every request of a
/// type is answered, a further response of that type rides as recorded.
#[test]
fn a_response_consumes_the_request_it_answers() {
    let mut requests = received(&[r#"<request><play id="p-1"/></request>"#]);
    let response = mscml_body(r#"<response request="play" id="captured-3" code="200"/>"#);
    assert_eq!(
        emitted(&response, &mut requests),
        mscml_body(r#"<response request="play" id="p-1" code="200"/>"#)
    );
    assert_eq!(emitted(&response, &mut requests), response, "no request is left to answer");
}

/// Only a `<response>` echoes: a notification and a request a replayed
/// party sends ride as recorded, whatever ids they hold.
#[test]
fn an_mscml_body_that_is_no_response_is_untouched() {
    let mut requests = received(&[r#"<request><play id="p-1"/></request>"#]);
    let notification =
        mscml_body(r#"<notification><keypress digit="1" request="play" id="n-1"/></notification>"#);
    assert_eq!(emitted(&notification, &mut requests), notification);
    let request = mscml_body(r#"<request><play id="mine"/></request>"#);
    assert_eq!(emitted(&request, &mut requests), request);
    assert_eq!(
        emitted(&mscml_body(r#"<response request="play" code="200"/>"#), &mut requests),
        mscml_body(r#"<response id="p-1" request="play" code="200"/>"#),
        "neither answered the play"
    );
}

/// A response a replayed party SENDS is not one it received: the record is
/// built from requests alone.
#[test]
fn only_a_received_request_is_remembered() {
    let body = mscml_body(r#"<request><play id="p-1"/></request>"#);
    let raw = format!(
        "SIP/2.0 200 OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-1\r\n\
             From: <sip:as@example.invalid>;tag=a1\r\n\
             To: <sip:ms@example.invalid>;tag=b1\r\n\
             Call-ID: call-b\r\n\
             CSeq: 2 INFO\r\n\
             Content-Type: {MSCML}\r\n\
             Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let message = sip_message::CustomParser::new().parse(raw.as_bytes()).expect("parses");
    let mut record = MscmlRequests::default();
    record.remember(&crate::gate::Inbound::of(&message));
    assert_eq!(record, MscmlRequests::default());
}

/// The echo reaches an MSCML part of a multipart body too, and only it.
#[test]
fn an_mscml_response_part_echoes_and_its_siblings_ride_as_stored() {
    let dir = std::env::temp_dir().join(format!("pivot-render-mscml-mp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let stored = mscml_body(r#"<response request="play" code="200"/>"#);
    std::fs::write(dir.join("response.xml"), &stored).unwrap();
    std::fs::write(dir.join("other.xml"), &stored).unwrap();
    let part = |reference: &str, content_type: &str| pivot_schema::body::Part {
        content_type: content_type.into(),
        reference: reference.into(),
        rewrite: vec![],
        mode: Some(pivot_schema::body::BodyMode::Frozen),
        compare: None,
        content_id: None,
        headers: vec![],
        cid_linked: vec![],
    };
    let body = Body::Multipart(pivot_schema::body::MultipartBody {
        multipart: pivot_schema::body::Multipart {
            content_type: "multipart/mixed".into(),
            parts: vec![part("response.xml", MSCML), part("other.xml", "application/xml")],
        },
    });
    let requests = received(&[r#"<request><play id="p-1"/></request>"#]);
    let (bytes, _) =
        load_body(&body, &dir, &Booking::new("127.0.0.1", 40000), "B", &mut Echo::new(&requests))
            .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let echoed = mscml_body(r#"<response id="p-1" request="play" code="200"/>"#);
    assert_eq!(text.matches(&echoed).count(), 1, "the MSCML part echoes:\n{text}");
    assert_eq!(text.matches(&stored).count(), 1, "the other part rides as stored:\n{text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Two MSCML responses of one type in one body answer two requests, in
/// order, never one twice.
#[test]
fn two_response_parts_of_one_type_answer_two_requests() {
    let dir = std::env::temp_dir().join(format!("pivot-render-mscml-mp2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("first.xml"), mscml_body(r#"<response request="play" n="1"/>"#))
        .unwrap();
    std::fs::write(dir.join("second.xml"), mscml_body(r#"<response request="play" n="2"/>"#))
        .unwrap();
    let part = |reference: &str| pivot_schema::body::Part {
        content_type: MSCML.into(),
        reference: reference.into(),
        rewrite: vec![],
        mode: Some(pivot_schema::body::BodyMode::Frozen),
        compare: None,
        content_id: None,
        headers: vec![],
        cid_linked: vec![],
    };
    let body = Body::Multipart(pivot_schema::body::MultipartBody {
        multipart: pivot_schema::body::Multipart {
            content_type: "multipart/mixed".into(),
            parts: vec![part("first.xml"), part("second.xml")],
        },
    });
    let requests = received(&[
        r#"<request><play id="p-1"/></request>"#,
        r#"<request><play id="p-2"/></request>"#,
    ]);
    let (bytes, _) =
        load_body(&body, &dir, &Booking::new("127.0.0.1", 40000), "B", &mut Echo::new(&requests))
            .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains(r#"<response id="p-1" request="play" n="1"/>"#), "{text}");
    assert!(text.contains(r#"<response id="p-2" request="play" n="2"/>"#), "{text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// What every corpus-generated SDP body states.
fn both() -> Rewrite {
    Rewrite::of(&["c=addr".to_string(), "m=port".to_string()])
}
