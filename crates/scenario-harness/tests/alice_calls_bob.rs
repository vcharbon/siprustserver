//! End-to-end smoke test of the scenario harness — the "one basic real test
//! that just tests the harness" (CLAUDE.md migration ritual).
//!
//! Alice INVITEs Bob over the recording-wrapped simulated `SignalingNetwork`,
//! Bob 200-OKs. Nothing here builds a trace: pseudo-agents send/recv through the
//! recording layer, and we assert that
//!   1. both `Expect`s matched (the driver works),
//!   2. the **recording** projects back into exactly the two delivered wire
//!      entries (`sip_net::to_sip_entries` — the record is the source of truth),
//!   3. the renderers produce the SVG diagram, the `global.txt`, and the
//!      per-endpoint `ext/<agent>.txt` views from that recording.
//!
//! This is the smallest exercise of the whole harness; the dialog/transaction
//! machinery the source DSL carried is out of scope until those layers land
//! (see MIGRATION_STATUS.md).

use std::path::PathBuf;

use scenario_harness::{run, Match, Scenario};

const SDP_OFFER: &str = "v=0\r\no=alice 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const SDP_ANSWER: &str = "v=0\r\no=bob 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

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

#[tokio::test]
async fn alice_calls_bob_end_to_end() {
    let invite = sip(
        "INVITE sip:bob@127.0.0.1:5070 SIP/2.0",
        &[
            ("Via", "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-alice-invite-1"),
            ("Max-Forwards", "70"),
            ("From", "<sip:alice@127.0.0.1>;tag=alicetag"),
            ("To", "<sip:bob@127.0.0.1>"),
            ("Call-ID", "call-abc@127.0.0.1"),
            ("CSeq", "1 INVITE"),
            ("Contact", "<sip:alice@127.0.0.1:5060>"),
            ("Content-Type", "application/sdp"),
        ],
        SDP_OFFER,
    );

    let ok = sip(
        "SIP/2.0 200 OK",
        &[
            ("Via", "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-alice-invite-1"),
            ("From", "<sip:alice@127.0.0.1>;tag=alicetag"),
            ("To", "<sip:bob@127.0.0.1>;tag=bobtag"),
            ("Call-ID", "call-abc@127.0.0.1"),
            ("CSeq", "1 INVITE"),
            ("Contact", "<sip:bob@127.0.0.1:5070>"),
            ("Content-Type", "application/sdp"),
        ],
        SDP_ANSWER,
    );

    let mut scn = Scenario::new("alice-calls-bob");
    let alice = scn.agent("alice", "127.0.0.1:5060");
    let bob = scn.agent("bob", "127.0.0.1:5070");
    scn.send(alice, bob, invite);
    scn.expect(bob, Match::method("INVITE"));
    scn.send(bob, alice, ok);
    scn.expect(alice, Match::status(200));
    let scn = scn.describe(
        "Smallest happy-path: alice INVITEs bob, bob 200-OKs. Exercises the \
         harness end to end over the recording-wrapped simulated network.",
    );

    let report = run(&scn).await;

    // 1. The driver matched both expectations.
    assert!(report.passed(), "expects did not all pass: {:#?}", report.expects);
    assert_eq!(report.expects.len(), 2);

    // 2. The recording projects back into exactly the two delivered messages,
    //    in send order, with both halves (send + recv) paired.
    let entries = report.entries();
    assert_eq!(entries.len(), 2, "entries: {entries:#?}");
    assert!(entries.iter().all(|e| e.delivered), "a message was not delivered");

    let alice_addr = "127.0.0.1:5060".parse().unwrap();
    let bob_addr = "127.0.0.1:5070".parse().unwrap();
    assert_eq!(entries[0].from, alice_addr);
    assert_eq!(entries[0].to, bob_addr);
    assert!(entries[0].received_ms.is_some());
    assert_eq!(entries[1].from, bob_addr);
    assert_eq!(entries[1].to, alice_addr);

    // The lane registry recorded both named agents.
    let scenario = report.scenario();
    assert_eq!(scenario.lanes.len(), 2);
    assert!(scenario.lanes.iter().any(|l| l.names.contains(&"alice".to_string())));
    assert!(scenario.lanes.iter().any(|l| l.names.contains(&"bob".to_string())));

    // 3. Render the three report flavours and assert on their content.
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("alice-calls-bob");
    let _ = std::fs::remove_dir_all(&out);
    let written = scenario_harness::report::write_all(&report, &out).expect("write reports");
    assert!(!written.is_empty());

    // SVG: both lanes labelled, both arrow captions present.
    let svg = std::fs::read_to_string(out.join("alice-calls-bob.svg")).unwrap();
    assert!(svg.contains("127.0.0.1:5060"), "svg missing alice lane");
    assert!(svg.contains("127.0.0.1:5070"), "svg missing bob lane");
    assert!(svg.contains("alice") && svg.contains("bob"), "svg missing lane names");
    assert!(svg.contains("INVITE"), "svg missing INVITE label");
    assert!(svg.contains("200 OK"), "svg missing 200 OK label");

    // global.txt: the wire text of both messages (exact bytes, not re-serialised).
    let global = std::fs::read_to_string(out.join("alice-calls-bob.global.txt")).unwrap();
    assert!(global.contains("Global (all endpoints)"));
    assert!(global.contains("INVITE sip:bob@127.0.0.1:5070 SIP/2.0"), "global missing INVITE wire");
    assert!(global.contains("SIP/2.0 200 OK"), "global missing 200 wire");
    assert!(global.contains("o=bob 2890"), "global missing SDP answer body");

    // Per-endpoint views exist under the ext/ fabric folder.
    let alice_txt = std::fs::read_to_string(out.join("ext/alice.txt")).unwrap();
    let bob_txt = std::fs::read_to_string(out.join("ext/bob.txt")).unwrap();
    assert!(alice_txt.contains("alice (endpoint, network=ext)"));
    assert!(bob_txt.contains("bob (endpoint, network=ext)"));
    // Both endpoints saw both messages (alice sent INVITE + received 200).
    assert!(alice_txt.contains("INVITE") && alice_txt.contains("200 OK"));

    // HTML wrapper embeds the diagram.
    let html = std::fs::read_to_string(out.join("alice-calls-bob.html")).unwrap();
    assert!(html.contains("<svg"), "html did not embed the svg");
    assert!(html.contains("SIP Exchange Report"));
}
