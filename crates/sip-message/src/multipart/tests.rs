use super::*;

fn media(raw: &str) -> MediaType {
    MediaType::parse(&SipStr::owned(raw)).unwrap()
}

/// RFC 5621 §3.1: a description framed inside a multipart body is the
/// message's offer or answer, wherever the part sits; a body typed
/// `application/sdp` is the description whole.
#[test]
fn the_sdp_range_reads_a_bare_body_and_a_framed_part_alike() {
    let bare = sdp().payload;
    assert_eq!(sdp_range(&media("application/sdp"), &bare), Some(0..bare.len()));
    assert_eq!(sdp_range(&media("application/sdp"), b""), None);

    let composed = compose("multipart/mixed", &[indata(), sdp()]).unwrap();
    let range = sdp_range(&media(&composed.content_type), &composed.body)
        .expect("the framed description is located");
    assert_eq!(&composed.body[range], sdp().payload.as_slice());

    let none = compose("multipart/mixed", &[indata()]).unwrap();
    assert_eq!(sdp_range(&media(&none.content_type), &none.body), None);
    let empty =
        compose("multipart/mixed", &[MultipartPart::new("application/sdp", Vec::new()), indata()])
            .unwrap();
    assert_eq!(sdp_range(&media(&empty.content_type), &empty.body), None, "an empty part");
    assert_eq!(sdp_range(&media("multipart/mixed"), &composed.body), None, "no boundary");
    assert_eq!(sdp_range(&media("text/plain"), b"v=0\r\n"), None);
}

fn sdp() -> MultipartPart {
    MultipartPart::new("application/sdp", b"v=0\r\nc=IN IP4 1.2.3.4\r\n".to_vec())
}

fn indata() -> MultipartPart {
    MultipartPart::new("application/vnd.example.indata", vec![0x77, 0x15, 0x47, 0x00, 0x0a])
        .with_header("Content-ID", "<indata@example.invalid>")
        .with_header("Content-Transfer-Encoding", "binary")
        .with_header("Content-Disposition", "signal;handling=optional")
}

/// The framing a capture shows (RFC 2046 §5.1): a delimiter line per part,
/// the part's entity headers, a blank line, the content, and a closing
/// delimiter.
#[test]
fn parts_frame_into_the_shape_a_capture_carries() {
    let composed = compose("multipart/mixed", &[sdp(), indata()]).unwrap();
    let boundary = composed
        .content_type
        .split("boundary=")
        .nth(1)
        .expect("the composed type names its boundary")
        .to_string();
    let text = String::from_utf8_lossy(&composed.body).into_owned();
    assert!(text.starts_with(&format!("--{boundary}\r\nContent-Type: application/sdp\r\n\r\n")));
    // The entity headers ride in the order the caller stated them, under
    // the part's own Content-Type (RFC 2045 §3).
    assert!(text.contains(
        "Content-Type: application/vnd.example.indata\r\n\
             Content-ID: <indata@example.invalid>\r\n\
             Content-Transfer-Encoding: binary\r\n\
             Content-Disposition: signal;handling=optional\r\n\r\n"
    ));
    assert!(text.ends_with(&format!("--{boundary}--\r\n")));
}

/// The property the replay depends on: a payload goes out byte for byte,
/// trailing newline included, because the delimiter brings its own CRLF.
#[test]
fn a_part_payload_rides_byte_exact_including_a_trailing_newline() {
    let composed = compose("multipart/mixed", &[sdp(), indata()]).unwrap();
    let boundary = composed.content_type.split("boundary=").nth(1).unwrap().as_bytes().to_vec();
    // Split the way an extractor does and read each part's content back.
    let delim = [b"--".as_slice(), &boundary].concat();
    let text = composed.body.clone();
    let mut contents: Vec<Vec<u8>> = Vec::new();
    let mut at = 0usize;
    while let Some(i) = find(&text[at..], &delim) {
        let block_start = at + i + delim.len();
        if text[block_start..].starts_with(b"--") {
            break;
        }
        let rest = &text[block_start + 2..];
        let head = find(rest, b"\r\n\r\n").expect("part headers end");
        let content_start = block_start + 2 + head + 4;
        let next = find(&text[content_start..], &delim).expect("next delimiter");
        contents.push(text[content_start..content_start + next - 2].to_vec());
        at = content_start + next;
    }
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[0], sdp().payload, "the SDP part is byte-exact");
    assert_eq!(contents[1], indata().payload, "the binary part is byte-exact");
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Same parts, same framing: a replayed body does not churn between runs.
#[test]
fn the_derived_boundary_is_a_function_of_the_parts() {
    let once = compose("multipart/mixed", &[sdp(), indata()]).unwrap();
    let twice = compose("multipart/mixed", &[sdp(), indata()]).unwrap();
    assert_eq!(once, twice);
    let other = compose("multipart/mixed", &[sdp()]).unwrap();
    assert_ne!(once.content_type, other.content_type);
}

/// A delimiter that would appear inside a payload does not separate parts,
/// it splits one — so the derivation walks past it.
#[test]
fn a_derived_boundary_never_occurs_inside_a_payload() {
    let seed = compose("multipart/mixed", &[sdp()]).unwrap();
    let boundary = seed.content_type.split("boundary=").nth(1).unwrap().to_string();
    let hostile =
        MultipartPart::new("application/sdp", format!("v=0\r\na=x:{boundary}\r\n").into_bytes());
    let composed = compose("multipart/mixed", std::slice::from_ref(&hostile)).unwrap();
    let used = composed.content_type.split("boundary=").nth(1).unwrap();
    assert!(!contains(&hostile.payload, used.as_bytes()));
}

/// A caller that states its own boundary keeps it — and is refused when a
/// payload holds it, rather than framing a body nothing can split.
#[test]
fn a_stated_boundary_is_honoured_and_a_colliding_one_is_refused() {
    let composed = compose("multipart/mixed;boundary=unique-boundary-1", &[sdp()]).unwrap();
    assert_eq!(composed.content_type, "multipart/mixed;boundary=unique-boundary-1");
    assert!(composed.body.starts_with(b"--unique-boundary-1\r\n"));
    let hostile = MultipartPart::new("text/plain", b"x unique-boundary-1 x".to_vec());
    assert_eq!(
        compose("multipart/mixed;boundary=unique-boundary-1", &[hostile]),
        Err(MultipartError::BoundaryInPayload { boundary: "unique-boundary-1".into() })
    );
}

/// The BINARY-IDENTITY proof for MIME framing: what [`decompose`] locates,
/// [`compose`] puts back. A captured body in the corpus's shape — an offer,
/// a location body whose content type states a `charset` and whose block
/// carries a `Content-ID`, and a binary payload ending in 0x0a — decomposes
/// and recomposes to the SAME BYTES. The container's boundary is the ONE
/// substitution (a replay regenerates it); the equality then leaves nothing
/// else free to move.
#[test]
fn a_captured_multipart_body_recomposes_byte_identically_bar_its_boundary() {
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(b"--unique-boundary-1\r\nContent-Type: application/sdp\r\n\r\n");
    body.extend_from_slice(b"v=0\r\nc=IN IP4 10.0.0.1\r\nm=audio 16804 RTP/AVP 8");
    body.extend_from_slice(
        b"\r\n--unique-boundary-1\r\n\
              Content-Type: application/pidf+xml;charset=utf-8\r\n\
              Content-ID: <geoloc@example.invalid>\r\n\
              Content-Disposition: render;handling=optional\r\n\r\n",
    );
    body.extend_from_slice(b"<presence/>");
    body.extend_from_slice(
        b"\r\n--unique-boundary-1\r\n\
              Content-Type: application/vnd.example.indata\r\n\
              Content-Transfer-Encoding: binary\r\n\
              Content-Disposition: signal;handling=optional\r\n\r\n",
    );
    body.extend_from_slice(&[0x77, 0x15, 0x47, 0x00, 0x83, 0x11, 0x89, 0x07, 0x0a]);
    body.extend_from_slice(b"\r\n--unique-boundary-1--\r\n");

    let located = decompose(&body, "unique-boundary-1");
    assert_eq!(located.len(), 3);
    // Recompose exactly as a replay does: the located content, the part's
    // own content type with its parameters, its id, then its other entity
    // headers in the order the walk read them.
    let parts: Vec<MultipartPart> = located
        .iter()
        .map(|p| {
            let payload = body[p.offset..p.offset + p.len].to_vec();
            let mut part = MultipartPart::new(p.content_type.clone(), payload);
            if let Some(id) = &p.content_id {
                part = part.with_header("Content-ID", id.clone());
            }
            for (name, value) in &p.headers {
                part = part.with_header(name.clone(), value.clone());
            }
            part
        })
        .collect();
    let composed = compose("multipart/mixed", &parts).expect("the parts frame");
    let derived = composed
        .content_type
        .strip_prefix("multipart/mixed;boundary=")
        .expect("the container states its own type and the boundary it derived");

    let recomposed = replace_bytes(&composed.body, derived.as_bytes(), b"unique-boundary-1");
    assert_eq!(
        String::from_utf8_lossy(&recomposed),
        String::from_utf8_lossy(&body),
        "the recomposed body differs from the captured one beyond its boundary"
    );
    assert_eq!(recomposed, body, "the difference is not a UTF-8 artefact");
}

/// `haystack` with every occurrence of `from` replaced by `to`.
fn replace_bytes(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut at = 0usize;
    while at < haystack.len() {
        match find(&haystack[at..], from) {
            Some(i) => {
                out.extend_from_slice(&haystack[at..at + i]);
                out.extend_from_slice(to);
                at += i + from.len();
            }
            None => {
                out.extend_from_slice(&haystack[at..]);
                break;
            }
        }
    }
    out
}

#[test]
fn a_non_multipart_container_and_an_empty_part_list_are_refused() {
    assert_eq!(
        compose("application/sdp", &[sdp()]),
        Err(MultipartError::NotMultipart { content_type: "application/sdp".into() })
    );
    assert_eq!(compose("multipart/mixed", &[]), Err(MultipartError::NoParts));
}

fn data() -> MultipartPart {
    MultipartPart::new("application/vnd.example.data", vec![0x77, 0x15, 0x00, 0x0a])
        .with_header("Content-ID", "<data@example.invalid>")
        .with_header("Content-Transfer-Encoding", "binary")
}

fn location() -> MultipartPart {
    MultipartPart::new("application/pidf+xml", b"<presence/>".to_vec())
        .with_header("Content-ID", "<loc@example.invalid>")
}

/// RFC 5621 §3: a bare description beside parts frames as
/// `multipart/mixed`, description first and byte-exact, the parts after it
/// with their own entity headers, `MIME-Version` on the message.
#[test]
fn a_bare_description_and_parts_frame_as_multipart_mixed() {
    let attached = attach(Some("application/sdp"), &[], &sdp().payload, &[data()]);
    let ct = attached.content_type.as_deref().expect("a body");
    assert!(media(ct).is("multipart/mixed"), "{ct}");
    assert_eq!(attached.headers, [("MIME-Version".to_string(), "1.0".to_string())]);
    let boundary = media(ct).param("boundary").and_then(ParamValue::as_str).unwrap().to_string();
    let parts = decompose(&attached.body, &boundary);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].content_type, "application/sdp");
    assert_eq!(&attached.body[parts[0].offset..][..parts[0].len], sdp().payload.as_slice());
    assert_eq!(parts[1].content_type, "application/vnd.example.data");
    assert_eq!(parts[1].content_id.as_deref(), Some("<data@example.invalid>"));
    assert_eq!(&attached.body[parts[1].offset..][..parts[1].len], data().payload.as_slice());
}

/// The parts replace every non-description part the source frames; the
/// framed description keeps its own entity headers.
#[test]
fn the_parts_replace_the_sources_other_parts() {
    let source = compose(
        "multipart/mixed",
        &[sdp().with_header("Content-Disposition", "session"), location()],
    )
    .unwrap();
    let attached = attach(Some(&source.content_type), &[], &source.body, &[data()]);
    let ct = attached.content_type.unwrap();
    let boundary = media(&ct).param("boundary").and_then(ParamValue::as_str).unwrap().to_string();
    let parts = decompose(&attached.body, &boundary);
    let types: Vec<&str> = parts.iter().map(|p| p.content_type.as_str()).collect();
    assert_eq!(types, ["application/sdp", "application/vnd.example.data"]);
    assert_eq!(parts[0].headers, [("Content-Disposition".to_string(), "session".to_string())]);
}

/// Without a description one part is the body, its entity headers and
/// `MIME-Version` the message's; several frame as `multipart/mixed`; none
/// leave no body.
#[test]
fn without_a_description_one_part_is_the_body() {
    let one = attach(None, &[], b"", &[location()]);
    assert_eq!(one.content_type.as_deref(), Some("application/pidf+xml"));
    assert_eq!(
        one.headers,
        [
            ("MIME-Version".to_string(), "1.0".to_string()),
            ("Content-ID".to_string(), "<loc@example.invalid>".to_string())
        ]
    );
    assert_eq!(one.body, location().payload);

    let several = attach(Some("text/plain"), &[], b"note", &[location(), data()]);
    assert!(media(several.content_type.as_deref().unwrap()).is("multipart/mixed"));
    assert_eq!(several.headers, [("MIME-Version".to_string(), "1.0".to_string())]);

    let none = attach(None, &[], b"", &[]);
    assert_eq!(none, Attached { content_type: None, headers: Vec::new(), body: Vec::new() });
    let bare = attach(Some("application/sdp"), &[], &sdp().payload, &[]);
    assert_eq!(bare.content_type.as_deref(), Some("application/sdp"));
    assert!(bare.headers.is_empty(), "a description alone is no MIME entity of ours");
    assert_eq!(bare.body, sdp().payload);
}

/// RFC 2045 §9: `MIME-Version` and the `Content-` fields describe the body.
#[test]
fn the_entity_headers_are_mime_version_and_the_content_fields() {
    for name in ["MIME-Version", "mime-version", "Content-ID", "content-disposition"] {
        assert!(is_entity_header(name), "{name}");
    }
    for name in ["Contact", "Content-", "Via", "X-Content-Id"] {
        assert!(!is_entity_header(name), "{name}");
    }
}

/// A part's own `Content-Type` header types it; its `MIME-Version` and
/// its structural `Content-Length` never ride, lifted or framed.
#[test]
fn a_parts_message_level_headers_are_filtered() {
    let stated = MultipartPart::new("application/octet-stream", b"hello".to_vec())
        .with_header("Content-Type", "application/vnd.example.data")
        .with_header("MIME-Version", "1.0")
        .with_header("Content-Length", "5")
        .with_header("Content-ID", "<d@example.invalid>");
    let one = attach(None, &[], b"", std::slice::from_ref(&stated));
    assert_eq!(one.content_type.as_deref(), Some("application/vnd.example.data"));
    assert_eq!(
        one.headers,
        [
            ("MIME-Version".to_string(), "1.0".to_string()),
            ("Content-ID".to_string(), "<d@example.invalid>".to_string())
        ]
    );

    let framed = attach(Some("application/sdp"), &[], &sdp().payload, &[stated]);
    let ct = media(framed.content_type.as_deref().unwrap());
    let boundary = ct.param("boundary").and_then(ParamValue::as_str).unwrap().to_string();
    let parts = decompose(&framed.body, &boundary);
    assert_eq!(parts[1].content_type, "application/vnd.example.data");
    assert_eq!(
        parts[1].entity_headers,
        [("Content-ID".to_string(), "<d@example.invalid>".to_string())]
    );
}

/// The description keeps its entity headers: a framed one in wire order, a
/// bare one those the message stated for it.
#[test]
fn the_description_keeps_its_entity_headers_in_order() {
    let source = compose(
        "multipart/mixed",
        &[
            sdp()
                .with_header("Content-Disposition", "session")
                .with_header("Content-ID", "<s@example.invalid>"),
            location(),
        ],
    )
    .unwrap();
    let attached = attach(Some(&source.content_type), &[], &source.body, &[data()]);
    let boundary = media(attached.content_type.as_deref().unwrap())
        .param("boundary")
        .and_then(ParamValue::as_str)
        .unwrap()
        .to_string();
    let parts = decompose(&attached.body, &boundary);
    assert_eq!(
        parts[0].entity_headers,
        [
            ("Content-Disposition".to_string(), "session".to_string()),
            ("Content-ID".to_string(), "<s@example.invalid>".to_string())
        ]
    );

    let message = [
        ("Content-Disposition".to_string(), "session".to_string()),
        ("MIME-Version".to_string(), "1.0".to_string()),
        ("Content-Length".to_string(), "40".to_string()),
    ];
    let bare = attach(Some("application/sdp"), &message, &sdp().payload, &[data()]);
    let boundary = media(bare.content_type.as_deref().unwrap())
        .param("boundary")
        .and_then(ParamValue::as_str)
        .unwrap()
        .to_string();
    let parts = decompose(&bare.body, &boundary);
    assert_eq!(
        parts[0].entity_headers,
        [("Content-Disposition".to_string(), "session".to_string())]
    );
}

/// A body's entity parts: each part of a multipart body, or the bare body
/// as one part described by the message's entity headers.
#[test]
fn a_bodys_entity_parts() {
    let message = [
        ("Content-ID".to_string(), "<x@example.invalid>".to_string()),
        ("Geolocation".to_string(), "<cid:x@example.invalid>".to_string()),
    ];
    let bare = entity_parts(Some("application/pidf+xml"), &message, b"<presence/>");
    assert_eq!(
        bare,
        [MultipartPart::new("application/pidf+xml", b"<presence/>".to_vec())
            .with_header("Content-ID", "<x@example.invalid>")],
        "a non-entity header stays the message's"
    );
    let framed = compose("multipart/mixed", &[sdp(), data()]).unwrap();
    let parts = entity_parts(Some(&framed.content_type), &[], &framed.body);
    assert_eq!(parts, [sdp(), data()]);
    assert!(entity_parts(Some("application/sdp"), &[], b"").is_empty());
    assert!(entity_parts(None, &[], b"v=0").is_empty());
}

/// A part serializes its bytes as base64 text and reads them back.
#[test]
fn a_part_serializes_its_bytes_as_base64() {
    let json = serde_json::to_value(data()).unwrap();
    assert_eq!(json["payload"], "dxUACg==");
    assert_eq!(serde_json::from_value::<MultipartPart>(json).unwrap(), data());
}

#[test]
fn every_entity_header_value_is_located_in_the_body() {
    let body =
        b"--b\r\nContent-Type: text/plain\r\nContent-ID:  <x@example.com> \r\n\r\nhi\r\n--b--\r\n";
    let parts = decompose(body, "b");
    let spans: Vec<(&str, &[u8])> =
        parts[0].entity_spans.iter().map(|(n, r)| (n.as_str(), &body[r.clone()])).collect();
    assert_eq!(
        spans,
        [("Content-Type", &b"text/plain"[..]), ("Content-ID", &b"<x@example.com>"[..])]
    );
}

#[test]
fn a_boundary_longer_than_seventy_characters_frames_no_part() {
    let framed = |boundary: &str| {
        format!("--{boundary}\r\nContent-Type: text/plain\r\n\r\nhi\r\n--{boundary}--\r\n")
    };
    let longest = "b".repeat(70);
    assert_eq!(decompose(framed(&longest).as_bytes(), &longest).len(), 1);
    let over = "b".repeat(71);
    assert!(decompose(framed(&over).as_bytes(), &over).is_empty());
}
