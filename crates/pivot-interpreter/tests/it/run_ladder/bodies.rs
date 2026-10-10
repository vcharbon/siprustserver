//! Bodies: multipart and binary bodies on the wire and in the recording.

use super::*;

/// Rung five, the MULTIPART rung: an initial INVITE offering SDP beside a
/// binary deployment payload, in the shape the corpus carries.
///
/// The parts are decomposed on disk and the FRAMING is regenerated — §8.3
/// stores the container type with its boundary stripped — so what the rung
/// proves is BINARY IDENTITY: the body the callee takes off the wire equals the
/// captured one byte for byte once the regenerated boundary and the rewritten
/// SDP address and port are substituted back, and nothing else is allowed to
/// differ.
/// It asserts on the datagram the CALLEE received, not on the one the caller
/// composed, so the whole path is covered.
#[tokio::test(start_paused = true)]
async fn rung_five_a_multipart_invite_reaches_the_callee_with_both_parts_byte_exact() {
    let Some(case) = case_dir("bc-rc-multipart-indata-invite") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let parts_dir = case.base_dir.join("resources");
    let offer = std::fs::read(parts_dir.join("uac1_0_0.sdp")).expect("the offer part");
    let payload = std::fs::read(parts_dir.join("uac1_0_1.bin")).expect("the binary part");
    let scene = B2buaScene::new("pivot-multipart-indata").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=13).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }

    // The bytes the CALLEE took off the wire.
    let arrived = scene
        .bob
        .wire_view()
        .into_iter()
        .find(|entry| entry.raw.starts_with(b"INVITE "))
        .expect("the callee received the INVITE");
    let raw = arrived.raw;
    let head = String::from_utf8_lossy(&raw[..raw.len().min(1200)]).into_owned();
    let boundary = head
        .split("boundary=")
        .nth(1)
        .and_then(|rest| rest.split(['\r', ';']).next())
        .expect("the relayed Content-Type names its boundary")
        .to_string();
    assert!(head.contains("multipart/mixed"), "{head}");
    assert!(
        find_bytes(&raw, format!("--{boundary}--\r\n").as_bytes()).is_some(),
        "the closing delimiter is present"
    );

    // Each part's payload, byte for byte: the offer with the lane's media
    // address and booked port written in — and nothing else about it touched —
    // and the binary part exactly as the capture held it.
    let offered = String::from_utf8_lossy(&offer)
        .replace("c=IN IP4 198.51.100.7", "c=IN IP4 127.0.0.1")
        .replace("m=audio 30000 ", &format!("m=audio {LANE_RTP_BASE} "));
    assert!(
        find_bytes(&raw, offered.as_bytes()).is_some(),
        "the SDP part rides byte-exact under the lane's media address and booked port:\n{}",
        String::from_utf8_lossy(&raw)
    );
    assert!(
        find_bytes(&raw, b"c=IN IP4 198.51.100.7").is_none(),
        "the connection address is the lane's"
    );
    assert!(
        find_bytes(&raw, b"m=audio 30000 ").is_none(),
        "and the media port is the one the lane BOOKED, not the one the capture stored"
    );
    assert!(
        find_bytes(&raw, b"o=alice 1 1 IN IP4 198.51.100.7").is_some(),
        "and only the connection line is rewritten"
    );
    assert!(
        find_bytes(&raw, &payload).is_some(),
        "the binary part rides byte-exact: {payload:02x?}"
    );

    // The container's own type, regenerated boundary and NOTHING else: the
    // document stores `multipart/mixed`, so that is what frames the body.
    assert!(
        head.contains(&format!("Content-Type: multipart/mixed;boundary={boundary}\r\n")),
        "the container type is the stored one plus its regenerated boundary:\n{head}"
    );

    // THE BINARY-IDENTITY PROOF (§8.3). The body that arrived is the captured
    // body byte for byte, save exactly the THREE things replay regenerates: the
    // container boundary, the SDP part's connection address, and the SDP part's
    // media port. All three are substituted back below — each one substitution,
    // named — and the equality then proves nothing ELSE moved: not a MIME
    // parameter, not an entity header or its order, not a delimiter's CRLF, not
    // the closing marker. A rewrite token this interpreter starts honouring
    // joins the named list here or fails this assertion, which is the point.
    let geoloc = std::fs::read(parts_dir.join("uac1_0_2.xml")).expect("the location part");
    let mut captured: Vec<u8> = Vec::new();
    captured.extend_from_slice(
        b"--unique-boundary-1\r\n\
          Content-Type: application/sdp\r\n\
          Content-ID: <offer@example.invalid>\r\n\
          Content-Disposition: session\r\n\r\n",
    );
    captured.extend_from_slice(&offer);
    captured.extend_from_slice(
        b"\r\n--unique-boundary-1\r\n\
          Content-Type: application/vnd.example.indata\r\n\
          Content-ID: <indata@example.invalid>\r\n\
          Content-Transfer-Encoding: binary\r\n\
          Content-Disposition: signal;handling=optional\r\n\r\n",
    );
    captured.extend_from_slice(&payload);
    captured.extend_from_slice(
        b"\r\n--unique-boundary-1\r\n\
          Content-Type: application/pidf+xml;charset=utf-8\r\n\
          Content-ID: <geoloc@example.invalid>\r\n\
          Content-Disposition: render;handling=optional\r\n\r\n",
    );
    captured.extend_from_slice(&geoloc);
    captured.extend_from_slice(b"\r\n--unique-boundary-1--\r\n");

    let head_end = find_bytes(&raw, b"\r\n\r\n").expect("the datagram's head ends") + 4;
    let arrived_body = &raw[head_end..];
    let regenerated_boundary =
        replace_bytes(arrived_body, boundary.as_bytes(), b"unique-boundary-1");
    let rewritten_addr =
        replace_bytes(&regenerated_boundary, b"c=IN IP4 127.0.0.1", b"c=IN IP4 198.51.100.7");
    let rewritten_sdp = replace_bytes(
        &rewritten_addr,
        format!("m=audio {LANE_RTP_BASE} ").as_bytes(),
        b"m=audio 30000 ",
    );
    assert_eq!(
        String::from_utf8_lossy(&rewritten_sdp),
        String::from_utf8_lossy(&captured),
        "the emitted body differs from the captured one beyond its boundary, its SDP address \
         and its SDP media port"
    );
    assert_eq!(rewritten_sdp, captured, "the difference is not a UTF-8 artefact");

    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    scene.finish().await;
}

/// `haystack` with every occurrence of `from` replaced by `to`, over bytes a
/// string type cannot hold.
fn replace_bytes(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut at = 0usize;
    while at < haystack.len() {
        match find_bytes(&haystack[at..], from) {
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

/// Where `needle` sits inside `haystack`, for a payload no string comparison
/// can hold (a binary part is not UTF-8).
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// The one recorded line on `leg`, in `dir`, whose datagram starts with `prefix`.
fn recorded_line<'a>(
    lines: &'a [serde_json::Value],
    dir: &str,
    prefix: &[u8],
) -> &'a serde_json::Value {
    lines
        .iter()
        .find(|line| line["dir"] == dir && recorded_wire(line).starts_with(prefix))
        .unwrap_or_else(|| {
            panic!("no {dir} line starting with {:?}: {lines:#?}", String::from_utf8_lossy(prefix))
        })
}

/// A recorded datagram whose body is not UTF-8 is written in the extractor's
/// `head` + `body_b64` form, and nowhere on the line is a replacement
/// character: the bytes that crossed the wire are the bytes on disk. Hands
/// back the reassembled datagram.
fn assert_recorded_as_head_and_bytes(line: &serde_json::Value) -> Vec<u8> {
    assert!(
        line.get("raw").is_none(),
        "a datagram that is not UTF-8 is never written as text: {line}"
    );
    assert!(
        line.get("raw_b64").is_none(),
        "a UTF-8 head is written as text, only the body as base64: {line}"
    );
    let head = line["head"].as_str().unwrap_or_else(|| panic!("head is a string: {line}"));
    assert!(head.ends_with("\r\n\r\n"), "the head runs through the blank line: {head:?}");
    // A heuristic over THESE fixtures, which carry no legitimate U+FFFD: a
    // replacement character on the line can only be a byte a lossy decode lost.
    let text = serde_json::to_string(line).unwrap();
    assert!(!text.contains('\u{FFFD}'), "no byte was replaced on the way to disk: {text}");
    recorded_wire(line)
}

/// The BINARY-BODY rung: a linear answered call carrying, mid-dialog, an INFO
/// whose body is bytes that are not UTF-8, sent by the caller and expected by
/// the callee under a `frozen` resource.
///
/// What the rung proves is that the recording keeps the datagram byte for
/// byte on BOTH legs — the caller's `out` line and the callee's `in` line are
/// written as a UTF-8 head plus the body's base64, never as text with the
/// bytes that would not decode replaced.
#[tokio::test(start_paused = true)]
async fn a_binary_body_is_recorded_byte_for_byte_on_both_legs() {
    let body = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/resources/binary-body_info.bin"),
    )
    .expect("the binary resource");
    assert!(std::str::from_utf8(&body).is_err(), "the resource is not UTF-8 by construction");

    let scene = api_scene("pivot-binary-body").await;
    let (outcome, dir) = replay(&scene, "binary-body.v3.json").await;

    // The bytes the CALLEE took off the wire are the resource's, so whatever
    // the recording holds is measured against a datagram that was right.
    let arrived = scene
        .bob
        .wire_view()
        .into_iter()
        .find(|entry| entry.raw.starts_with(b"INFO "))
        .expect("the callee received the INFO");
    assert!(arrived.raw.ends_with(&body), "the INFO reached the callee byte-exact");

    let callee = recorded_lines(&dir, "B");
    let taken = assert_recorded_as_head_and_bytes(recorded_line(&callee, "in", b"INFO "));
    assert!(taken.ends_with(&body), "the callee's line ends with the body bytes: {taken:02x?}");
    assert_eq!(
        taken, arrived.raw,
        "the callee's line IS the datagram its socket took, byte for byte"
    );
    let caller = recorded_lines(&dir, "A");
    let sent = assert_recorded_as_head_and_bytes(recorded_line(&caller, "out", b"INFO "));
    assert!(sent.ends_with(&body), "the caller's line ends with the body bytes: {sent:02x?}");

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 17, "every step ran");
    scene.finish().await;
}

/// The same rung read from the CHECK side: the callee's expect asserts the
/// body's bytes as `body.b64`, the base64 of the resource file, and the run is
/// green — a binary body is confronted, not merely present.
#[tokio::test(start_paused = true)]
async fn a_binary_body_check_reads_its_bytes_as_base64_and_the_run_is_green() {
    let scene = api_scene("pivot-binary-body-check").await;
    let (outcome, dir) = replay(&scene, "binary-body.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 17, "every step ran");
    assert!(
        outcome.verdict.completed_steps.contains(&"s11".to_string()),
        "the callee's INFO expect completed with its body check: {:#?}",
        outcome.verdict.failures
    );
    assert!(
        outcome.verdict.failures.is_empty(),
        "the run is green: {:#?}",
        outcome.verdict.failures
    );
    scene.finish().await;
}

/// The MULTIPART variant: the initial INVITE carries an SDP part beside a
/// binary part, expected on the callee as a `multipart` body. The callee's
/// recorded line carries the body's LAYOUT — the extractor's `body.parts`,
/// offset and length per part — so a reader locates each part in the recorded
/// bytes without splitting on a boundary, and the binary part's bytes are the
/// resource's.
#[tokio::test(start_paused = true)]
async fn a_multipart_body_s_recording_locates_its_parts_and_keeps_the_binary_one() {
    let resources = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/resources");
    let blob =
        std::fs::read(resources.join("binary-multipart_part1.bin")).expect("the binary part");
    assert!(std::str::from_utf8(&blob).is_err(), "the part is not UTF-8 by construction");

    let scene = api_scene("pivot-binary-multipart").await;
    let (outcome, dir) = replay(&scene, "binary-multipart.v3.json").await;

    let callee = recorded_lines(&dir, "B");
    let line = recorded_line(&callee, "in", b"INVITE ");
    let wire = assert_recorded_as_head_and_bytes(line);
    let head_len = wire.len() - line["body"]["len"].as_u64().expect("body.len") as usize;
    let body = &wire[head_len..];
    assert!(
        std::str::from_utf8(&wire[..head_len]).is_ok(),
        "the head runs to the blank line and is text"
    );
    let layout = &line["body"];
    assert_eq!(layout["content_type"], "multipart/mixed", "{layout}");
    let parts = layout["parts"].as_array().unwrap_or_else(|| panic!("body.parts: {line}"));
    assert_eq!(parts.len(), 2, "both parts located: {parts:#?}");
    let part = |n: usize| {
        let offset = parts[n]["offset"].as_u64().unwrap() as usize;
        let len = parts[n]["len"].as_u64().unwrap() as usize;
        &body[offset..offset + len]
    };
    assert_eq!(parts[0]["content_type"], "application/sdp");
    assert!(part(0).starts_with(b"v=0\r\n"), "the SDP part is located: {:?}", part(0));
    assert_eq!(parts[1]["content_type"], "application/vnd.example.blob");
    assert_eq!(part(1), &blob[..], "the binary part is the resource byte for byte");

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    scene.finish().await;
}
