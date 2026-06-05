//! End-to-end test of the scenario harness — a full one-call dialog
//! (CLAUDE.md migration ritual: "one basic real test that just tests the
//! harness", here grown to a complete INVITE transaction + in-dialog BYE).
//!
//! Flow (alice = UAC @ 5060, bob = UAS @ 5070), all over the recording-wrapped
//! simulated `SignalingNetwork`:
//!
//! ```text
//!   alice ──INVITE (CSeq 1 INVITE)──▶ bob
//!   alice ◀──180 Ringing (CSeq 1)─── bob
//!   alice ◀──200 OK     (CSeq 1)─── bob
//!   alice ──ACK   (CSeq 1 ACK)────▶ bob
//!   alice ──BYE   (CSeq 2 BYE)────▶ bob
//!   alice ◀──200 OK     (CSeq 2)─── bob
//! ```
//!
//! Nothing here builds a trace: pseudo-agents send/recv through the recording
//! layer, and we assert that (1) every `Expect` matched, (2) the **recording**
//! projects back into exactly the six delivered wire entries in order, (3) the
//! CSeq numbering/method is carried faithfully end to end (1 INVITE → 1 ACK →
//! 2 BYE; responses echo their request's CSeq), and (4) the renderers produce
//! the SVG / global.txt / per-endpoint / unified-renderer HTML from that recording.

use std::path::PathBuf;

use scenario_harness::{run, Match, Scenario};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const SDP_OFFER: &str = "v=0\r\no=alice 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const SDP_ANSWER: &str = "v=0\r\no=bob 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

const CALL_ID: &str = "call-abc@127.0.0.1";
const INVITE_BRANCH: &str = "z9hG4bK-alice-invite";
const ACK_BRANCH: &str = "z9hG4bK-alice-ack";
const BYE_BRANCH: &str = "z9hG4bK-alice-bye";

/// Build a valid SIP datagram, computing `Content-Length` from the body so the
/// strict parser the harness uses to match accepts it.
fn sip(start_line: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut s = String::new();
    s.push_str(start_line);
    s.push_str("\r\n");
    for (k, v) in headers {
        s.push_str(k);
        s.push_str(": ");
        s.push_str(v);
        s.push_str("\r\n");
    }
    s.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    s.push_str(body);
    s.into_bytes()
}

fn parse(raw: &[u8]) -> SipMessage {
    CustomParser::new()
        .parse(raw)
        .unwrap_or_else(|e| panic!("entry did not parse: {e}\n---\n{}", String::from_utf8_lossy(raw)))
}

/// Assert a request entry's method + CSeq.
fn assert_request(raw: &[u8], method: &str, cseq_seq: u32) {
    match parse(raw) {
        SipMessage::Request(r) => {
            assert_eq!(r.method, method, "method");
            assert_eq!(r.cseq.method, method, "CSeq method must equal request method");
            assert_eq!(r.cseq.seq, cseq_seq, "CSeq seq for {method}");
            assert_eq!(r.call_id, CALL_ID, "Call-ID continuity");
        }
        other => panic!("expected {method} request, got {other:?}"),
    }
}

/// Assert a response entry's status + the CSeq it echoes from its request.
fn assert_response(raw: &[u8], status: u16, cseq_seq: u32, cseq_method: &str) {
    match parse(raw) {
        SipMessage::Response(r) => {
            assert_eq!(r.status, status, "status");
            assert_eq!(r.cseq.seq, cseq_seq, "echoed CSeq seq for {status}");
            assert_eq!(r.cseq.method, cseq_method, "echoed CSeq method for {status}");
            assert_eq!(r.call_id, CALL_ID, "Call-ID continuity");
        }
        other => panic!("expected {status} response, got {other:?}"),
    }
}

#[tokio::test]
async fn alice_calls_bob_full_dialog() {
    // --- messages ---------------------------------------------------------
    let invite = sip(
        "INVITE sip:bob@127.0.0.1:5070 SIP/2.0",
        &[
            ("Via", &format!("SIP/2.0/UDP 127.0.0.1:5060;branch={INVITE_BRANCH}")),
            ("Max-Forwards", "70"),
            ("From", "<sip:alice@127.0.0.1>;tag=alicetag"),
            ("To", "<sip:bob@127.0.0.1>"),
            ("Call-ID", CALL_ID),
            ("CSeq", "1 INVITE"),
            ("Contact", "<sip:alice@127.0.0.1:5060>"),
            ("Content-Type", "application/sdp"),
        ],
        SDP_OFFER,
    );
    let ringing = sip(
        "SIP/2.0 180 Ringing",
        &[
            ("Via", &format!("SIP/2.0/UDP 127.0.0.1:5060;branch={INVITE_BRANCH}")),
            ("From", "<sip:alice@127.0.0.1>;tag=alicetag"),
            ("To", "<sip:bob@127.0.0.1>;tag=bobtag"),
            ("Call-ID", CALL_ID),
            ("CSeq", "1 INVITE"),
            ("Contact", "<sip:bob@127.0.0.1:5070>"),
        ],
        "",
    );
    let ok_invite = sip(
        "SIP/2.0 200 OK",
        &[
            ("Via", &format!("SIP/2.0/UDP 127.0.0.1:5060;branch={INVITE_BRANCH}")),
            ("From", "<sip:alice@127.0.0.1>;tag=alicetag"),
            ("To", "<sip:bob@127.0.0.1>;tag=bobtag"),
            ("Call-ID", CALL_ID),
            ("CSeq", "1 INVITE"),
            ("Contact", "<sip:bob@127.0.0.1:5070>"),
            ("Content-Type", "application/sdp"),
        ],
        SDP_ANSWER,
    );
    let ack = sip(
        "ACK sip:bob@127.0.0.1:5070 SIP/2.0",
        &[
            ("Via", &format!("SIP/2.0/UDP 127.0.0.1:5060;branch={ACK_BRANCH}")),
            ("Max-Forwards", "70"),
            ("From", "<sip:alice@127.0.0.1>;tag=alicetag"),
            ("To", "<sip:bob@127.0.0.1>;tag=bobtag"),
            ("Call-ID", CALL_ID),
            ("CSeq", "1 ACK"),
        ],
        "",
    );
    let bye = sip(
        "BYE sip:bob@127.0.0.1:5070 SIP/2.0",
        &[
            ("Via", &format!("SIP/2.0/UDP 127.0.0.1:5060;branch={BYE_BRANCH}")),
            ("Max-Forwards", "70"),
            ("From", "<sip:alice@127.0.0.1>;tag=alicetag"),
            ("To", "<sip:bob@127.0.0.1>;tag=bobtag"),
            ("Call-ID", CALL_ID),
            ("CSeq", "2 BYE"),
        ],
        "",
    );
    let ok_bye = sip(
        "SIP/2.0 200 OK",
        &[
            ("Via", &format!("SIP/2.0/UDP 127.0.0.1:5060;branch={BYE_BRANCH}")),
            ("From", "<sip:alice@127.0.0.1>;tag=alicetag"),
            ("To", "<sip:bob@127.0.0.1>;tag=bobtag"),
            ("Call-ID", CALL_ID),
            ("CSeq", "2 BYE"),
        ],
        "",
    );

    // --- scenario ---------------------------------------------------------
    let mut scn = Scenario::new("alice-calls-bob");
    let alice = scn.agent("alice", "127.0.0.1:5060");
    let bob = scn.agent("bob", "127.0.0.1:5070");
    scn.send(alice, bob, invite);
    scn.expect(bob, Match::method("INVITE"));
    scn.send(bob, alice, ringing);
    scn.expect(alice, Match::status(180));
    scn.send(bob, alice, ok_invite);
    scn.expect(alice, Match::status(200));
    scn.send(alice, bob, ack);
    scn.expect(bob, Match::method("ACK"));
    scn.send(alice, bob, bye);
    scn.expect(bob, Match::method("BYE"));
    scn.send(bob, alice, ok_bye);
    scn.expect(alice, Match::status(200));
    let scn = scn.describe(
        "Full one-call dialog: INVITE / 180 / 200 / ACK / BYE / 200. Verifies \
         CSeq numbering (1 INVITE → 1 ACK → 2 BYE, responses echo their \
         request) survives the send → record → project → render pipeline.",
    );

    let report = run(&scn).await;

    // 1. Every expectation matched.
    assert!(report.passed(), "expects did not all pass: {:#?}", report.expects);
    assert_eq!(report.expects.len(), 6);

    // 2. The recording projects six delivered entries in send order.
    let entries = report.entries();
    assert_eq!(entries.len(), 6, "entries: {entries:#?}");
    assert!(entries.iter().all(|e| e.delivered), "a message was not delivered");

    let alice_addr = "127.0.0.1:5060".parse().unwrap();
    let bob_addr = "127.0.0.1:5070".parse().unwrap();
    let dirs: Vec<_> = entries.iter().map(|e| (e.from, e.to)).collect();
    assert_eq!(
        dirs,
        vec![
            (alice_addr, bob_addr), // INVITE
            (bob_addr, alice_addr), // 180
            (bob_addr, alice_addr), // 200 (INVITE)
            (alice_addr, bob_addr), // ACK
            (alice_addr, bob_addr), // BYE
            (bob_addr, alice_addr), // 200 (BYE)
        ]
    );

    // 3. CSeq is carried faithfully through the whole pipeline.
    assert_request(&entries[0].raw, "INVITE", 1);
    assert_response(&entries[1].raw, 180, 1, "INVITE");
    assert_response(&entries[2].raw, 200, 1, "INVITE");
    assert_request(&entries[3].raw, "ACK", 1); // 2xx ACK reuses the INVITE CSeq number
    assert_request(&entries[4].raw, "BYE", 2); // new in-dialog request increments CSeq
    assert_response(&entries[5].raw, 200, 2, "BYE");

    // Both named lanes recorded.
    let scenario = report.scenario();
    assert_eq!(scenario.lanes.len(), 2);

    // 4. Render and assert on the artifacts.
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("alice-calls-bob");
    let _ = std::fs::remove_dir_all(&out);
    let written = scenario_harness::report::write_all(&report, &out).expect("write reports");
    assert!(!written.is_empty());

    let svg = std::fs::read_to_string(out.join("alice-calls-bob.svg")).unwrap();
    assert!(svg.contains("INVITE") && svg.contains("180 Ringing") && svg.contains("200 OK"));
    assert!(svg.contains("ACK") && svg.contains("BYE"));
    // Arrows are clickable targets (index carried for the HTML handler).
    assert!(svg.contains(r#"data-trace-index="0""#));
    assert!(svg.contains("cursor:pointer"));

    let global = std::fs::read_to_string(out.join("alice-calls-bob.global.txt")).unwrap();
    assert!(global.contains("INVITE sip:bob@127.0.0.1:5070 SIP/2.0"));
    assert!(global.contains("180 Ringing"));
    assert!(global.contains("ACK sip:bob@127.0.0.1:5070 SIP/2.0"));
    assert!(global.contains("BYE sip:bob@127.0.0.1:5070 SIP/2.0"));
    assert!(global.contains("CSeq: 1 ACK"));
    assert!(global.contains("CSeq: 2 BYE"));

    // Per-endpoint views exist; both peers saw the whole dialog.
    let alice_txt = std::fs::read_to_string(out.join("ext/alice.txt")).unwrap();
    let bob_txt = std::fs::read_to_string(out.join("ext/bob.txt")).unwrap();
    assert!(alice_txt.contains("alice (endpoint, network=ext)"));
    assert!(bob_txt.contains("bob (endpoint, network=ext)"));
    assert!(alice_txt.contains("INVITE") && alice_txt.contains("BYE"));

    // HTML: produced by the SHARED unified renderer (seq-report) — a two-pane
    // layout (a scrollable inline-SVG diagram on the left, a FIXED Message-Detail
    // panel on the right), a plane legend, and per-message wire text in HIDDEN
    // payload blocks. Each diagram message is a clickable `<g class="seq-msg"
    // data-idx="N">` whose `#evt-N` payload the click `<script>` copies into the
    // `.detail-body`. Assert that interactive markup meaningfully.
    let html = std::fs::read_to_string(out.join("alice-calls-bob.html")).unwrap();
    assert!(html.contains("<svg"), "html did not embed the sequence diagram");
    assert!(html.contains("class=\"legend\""), "html missing plane legend");
    assert!(html.contains("class=\"detail-panel\""), "html missing fixed detail panel");
    assert!(html.contains("class=\"detail-body\""), "html missing scrollable detail body");
    // The diagram's messages are clickable `.seq-msg` groups; the first one
    // carries data-idx="0" and its payload lives in the hidden #evt-0 block.
    assert!(html.contains("class=\"seq-msg seq-sip\""), "html missing SIP .seq-msg groups");
    assert!(html.contains("data-idx=\"0\""), "first message not keyed data-idx=0");
    assert!(html.contains("id=\"evt-0\""), "html missing #evt-0 payload block");
    // The click handler wires `.seq-msg` clicks into the `.detail-body`.
    assert!(
        html.contains("querySelectorAll('.seq-msg')")
            && html.contains("querySelector('.detail-body').innerHTML"),
        "html missing .seq-msg → .detail-body click wiring"
    );
    assert!(
        html.contains("INVITE sip:bob@127.0.0.1:5070 SIP/2.0"),
        "html missing wire text"
    );
}
