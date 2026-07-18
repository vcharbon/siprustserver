//! SUT-less pin tests for **template emission** — replaying a captured SIP
//! message near-verbatim through the fluent agent surface, over the
//! recording-wrapped simulated network (no B2BUA in the path).
//!
//! Each test drives a template send and asserts, at the receiver, that the
//! tier-1 dialog-critical fields were REGENERATED and are RFC-valid (fresh Via
//! branch, Call-ID, From/To tags, CSeq, Max-Forwards, Contact, and the reused
//! dialog identifiers in-dialog) while every FROZEN header arrived byte-for-byte
//! (value bytes, name casing, and an intentionally duplicated header preserved);
//! the recorded-trace RFC audit gates GREEN at `finish()`.
//!
//! Automatics: `100 Trying` is a `sip-txn`/B2BUA automatic, so it is exercised
//! in the SUT lanes (phase R), not here. The stack-owned automatics observable
//! SUT-less — the §17.1.1.3 ACK-to-final and the minted To-tag, neither present
//! in any template — are pinned in [`automatics_fire_though_absent_from_templates`].

use scenario_harness::{EmitOpts, Harness, MessageTemplate, TemplateHeader};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipHeader, SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

/// The wire name (original casing) of the single header on `headers` whose name
/// matches `name` case-insensitively — proves the CAPTURED casing survived.
fn casing_of<'a>(headers: &'a [SipHeader], name: &str) -> &'a str {
    let matches: Vec<&SipHeader> =
        headers.iter().filter(|h| h.name.eq_ignore_ascii_case(name)).collect();
    assert_eq!(matches.len(), 1, "expected exactly one {name} header, got {}", matches.len());
    matches[0].name.as_str()
}

/// Every value carried under `name` (case-insensitive), in wire order — proves a
/// duplicated frozen header kept both rows.
fn values_of<'a>(headers: &'a [SipHeader], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
        .collect()
}

/// Build a well-formed raw SIP datagram (Content-Length computed from the body).
fn sip(start_line: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut s = String::from(start_line);
    s.push_str("\r\n");
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    s.push_str(body);
    s.into_bytes()
}

fn parse(raw: &[u8]) -> SipMessage {
    CustomParser::new().parse(raw).expect("well-formed datagram")
}

/// A template INVITE sent by a UAC arrives with FRESH, RFC-valid dialog fields
/// while every frozen header — a weird-cased header and an intentionally
/// duplicated one — is byte-equal at the receiver. Built via `from_message` so
/// the captured tier-1 headers are proven DROPPED, not merely overridden.
#[tokio::test]
async fn template_invite_regenerates_dialog_fields_and_freezes_headers() {
    let h = Harness::new("template-invite").describe(
        "A UAC replays a captured INVITE: tier-1 fields regenerated fresh, frozen \
         headers (odd casing + a duplicated header) byte-preserved at the callee",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // A captured INVITE carrying tier-1 headers (which MUST be regenerated) plus
    // frozen ones: a mixed-case P-Asserted-Identity and a duplicated X-Trace.
    let captured = parse(&sip(
        "INVITE sip:+15559999@capture.example SIP/2.0",
        &[
            ("Via", "SIP/2.0/UDP 203.0.113.7:5060;branch=z9hG4bK-CAPTURED"),
            ("Max-Forwards", "55"),
            ("From", "<sip:+15551234@capture.example>;tag=CAP-FROM"),
            ("To", "<sip:+15559999@capture.example>"),
            ("Call-ID", "captured-call-id@203.0.113.7"),
            ("CSeq", "9 INVITE"),
            ("Contact", "<sip:+15551234@203.0.113.7:5060>"),
            ("Subject", "Q3 planning"),
            ("p-AsSeRtEd-IdEnTiTy", "<sip:+15551234@capture.example>"),
            ("X-Trace", "hop-a"),
            ("X-Trace", "hop-b"),
            ("Content-Type", "application/sdp"),
        ],
        OFFER,
    ));
    let tmpl = MessageTemplate::from_message(&captured);

    let mut call = alice.invite(&bob).template(&tmpl, EmitOpts::default()).send().await;

    let mut uas = bob.receive("INVITE").await;
    let req = uas.request().clone();

    // --- tier-1 regenerated, RFC-valid, NOT the captured values ---------------
    let branch = req.via.first().branch.as_deref().expect("fresh Via branch");
    assert!(branch.starts_with("z9hG4bK"), "fresh magic-cookie branch, got {branch}");
    assert_ne!(branch, "z9hG4bK-CAPTURED", "the captured Via branch was regenerated");
    assert!(!req.call_id.is_empty(), "Call-ID present");
    assert_ne!(req.call_id, "captured-call-id@203.0.113.7", "Call-ID regenerated");
    assert!(req.from.tag.is_some(), "fresh From-tag present");
    assert_ne!(req.from.tag.as_deref(), Some("CAP-FROM"), "From-tag regenerated");
    assert!(req.to.tag.is_none(), "an initial INVITE carries no To-tag");
    assert_eq!(req.cseq.seq, 1, "CSeq regenerated to the fresh dialog's 1");
    assert_eq!(req.cseq.method, "INVITE");
    assert_eq!(values_of(&req.headers, "Max-Forwards"), vec!["70"], "Max-Forwards regenerated (not the captured 55)");
    // The Request-URI is the stack's (peer-addressed), not the captured R-URI.
    assert!(req.uri.contains("127.0.0.1:5070"), "R-URI regenerated to the peer, got {}", req.uri);
    assert_ne!(req.uri, "sip:+15559999@capture.example");

    // --- frozen headers byte-equal, casing + duplicate layout preserved -------
    assert_eq!(values_of(&req.headers, "Subject"), vec!["Q3 planning"]);
    assert_eq!(casing_of(&req.headers, "p-asserted-identity"), "p-AsSeRtEd-IdEnTiTy", "captured casing preserved");
    assert_eq!(values_of(&req.headers, "p-asserted-identity"), vec!["<sip:+15551234@capture.example>"]);
    assert_eq!(values_of(&req.headers, "X-Trace"), vec!["hop-a", "hop-b"], "both duplicated rows preserved in order");
    assert_eq!(req.body, OFFER.as_bytes(), "frozen SDP body byte-preserved");

    // --- carry the call to a clean, RFC-compliant teardown --------------------
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}

/// An in-dialog request (UPDATE) replayed from a template REUSES the established
/// dialog's identifiers (Call-ID, From/To tags) and advances the CSeq, while its
/// frozen headers are byte-preserved.
#[tokio::test]
async fn template_in_dialog_update_reuses_dialog_identifiers() {
    let h = Harness::new("template-update").describe(
        "An in-dialog UPDATE replayed from a template reuses the confirmed \
         dialog's Call-ID/tags and bumps CSeq; frozen headers byte-preserved",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // Establish alice <-> bob.
    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    let invite_req = uas.request().clone();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    let ok = call.expect(200).await;
    let alice_from_tag = invite_req.from.tag.clone().expect("From-tag");
    let bob_to_tag = ok.to.tag.clone().expect("minted To-tag");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Replay a captured UPDATE (offer) on the confirmed dialog.
    let update = MessageTemplate::request(
        Method::Update,
        vec![
            TemplateHeader::frozen("Content-Type", "application/sdp"),
            TemplateHeader::frozen("X-mIxEd-CaSe", "kept"),
            TemplateHeader::frozen("X-Dup", "1"),
            TemplateHeader::frozen("X-Dup", "2"),
        ],
        OFFER.as_bytes().to_vec(),
    );
    let mut upd_txn = dialog.send_template(&update, EmitOpts::default()).await;

    let mut ubob = bob.receive("UPDATE").await;
    let ureq = ubob.request().clone();
    // Dialog identifiers reused; CSeq advanced past the INVITE's 1.
    assert_eq!(ureq.call_id, invite_req.call_id, "Call-ID reused from the dialog");
    assert_eq!(ureq.from.tag.as_deref(), Some(alice_from_tag.as_str()), "From-tag reused");
    assert_eq!(ureq.to.tag.as_deref(), Some(bob_to_tag.as_str()), "To-tag = the peer's dialog tag");
    assert_eq!(ureq.cseq.seq, 2, "CSeq advanced to 2");
    assert_eq!(ureq.cseq.method, "UPDATE");
    // Frozen headers byte-preserved.
    assert_eq!(casing_of(&ureq.headers, "x-mixed-case"), "X-mIxEd-CaSe");
    assert_eq!(values_of(&ureq.headers, "X-Dup"), vec!["1", "2"]);
    assert_eq!(ureq.body, OFFER.as_bytes());

    ubob.respond(200, "OK").with_sdp(ANSWER).await;
    upd_txn.expect(200).await;

    // Teardown.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}

/// A UAS answers from a template RESPONSE: the To-tag is minted (regenerated)
/// while frozen unusual headers (odd casing + a duplicate) are byte-preserved at
/// the caller, and the templated answer body rides through.
#[tokio::test]
async fn template_response_freezes_unusual_headers() {
    let h = Harness::new("template-response").describe(
        "A UAS answers a call from a template 200: To-tag minted, frozen unusual \
         headers byte-preserved at the caller, templated answer body carried",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // A captured 200 with a mixed-case P-Charging-Vector and a duplicated X-Note.
    let answer = MessageTemplate::response(
        200,
        "OK",
        vec![
            TemplateHeader::frozen("Content-Type", "application/sdp"),
            TemplateHeader::frozen("P-cHaRgInG-vEcToR", "icid-value=abc123"),
            TemplateHeader::frozen("X-Note", "n1"),
            TemplateHeader::frozen("X-Note", "n2"),
        ],
        ANSWER.as_bytes().to_vec(),
    );
    uas.respond_template(&answer, EmitOpts::default()).send().await;

    let ok = call.expect(200).await;
    // To-tag regenerated (minted by the stack), not carried by the template.
    assert!(ok.to.tag.is_some(), "the 200's To-tag was minted (regenerated)");
    // Frozen unusual headers byte-preserved, casing + duplicate layout intact.
    assert_eq!(casing_of(&ok.headers, "p-charging-vector"), "P-cHaRgInG-vEcToR");
    assert_eq!(values_of(&ok.headers, "p-charging-vector"), vec!["icid-value=abc123"]);
    assert_eq!(values_of(&ok.headers, "X-Note"), vec!["n1", "n2"]);
    assert_eq!(ok.body, ANSWER.as_bytes(), "templated answer body carried");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}

/// The stack-owned automatics fire though they appear in NO template: a UAS
/// rejects a template INVITE from a template 486 (whose To-tag is minted, not
/// templated), and the UAC's §17.1.1.3 ACK-to-final fires — the hop ACK the test
/// never scripted arrives at the UAS.
#[tokio::test]
async fn automatics_fire_though_absent_from_templates() {
    let h = Harness::new("template-automatics").describe(
        "Template emission does not disturb the stack automatics: a UAC replays an \
         INVITE, a UAS replays a 486, and the §17.1.1.3 auto-ACK + minted To-tag \
         (neither in any template) still fire",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let invite = MessageTemplate::request(
        Method::Invite,
        vec![
            TemplateHeader::frozen("Content-Type", "application/sdp"),
            TemplateHeader::frozen("Subject", "urgent"),
        ],
        OFFER.as_bytes().to_vec(),
    );
    let mut call = alice.invite(&bob).template(&invite, EmitOpts::default()).send().await;

    let mut uas = bob.receive("INVITE").await;
    let busy = MessageTemplate::response(
        486,
        "Busy Here",
        vec![TemplateHeader::frozen("Retry-After", "30")],
        Vec::new(),
    );
    uas.respond_template(&busy, EmitOpts::default()).send().await;

    // The UAC auto-ACKs the non-2xx final (RFC 3261 §17.1.1.3) — an automatic
    // that appears in NO template.
    let rej = call.expect(486).await;
    assert!(rej.to.tag.is_some(), "the reject's To-tag was minted by the stack, not templated");
    assert_eq!(values_of(&rej.headers, "Retry-After"), vec!["30"], "frozen header preserved");

    // The stack-owned hop ACK reached the UAS though the test scripted no ACK.
    uas.expect_ack().await;

    h.finish().await;
}
