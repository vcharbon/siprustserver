//! RFC 3261 §18.2.2 at the hard gate: a response over UDP goes to the address
//! its own top Via names. A stack under test that sends it elsewhere fails the
//! run; a simulated party that does is reported, never gated.

use scenario_harness::{report, Harness, RunReport};
use sip_message::generators::{generate_response, GenerateResponseOpts, OutOfDialogMethod};
use sip_message::{CustomParser, SipMessage, SipParser};

/// Where the SUT sends a copy of its answer no one listens for.
#[derive(Clone, Copy)]
enum Stray {
    None,
    /// Before the copy to her Via target.
    First,
    /// After it: a repeat sent where its first copy was not.
    After,
}

/// Alice sends an OPTIONS to the SUT, which answers 480 to her Via target,
/// with a copy to an address no one listens on where `stray` says.
async fn sut_answers(h: &Harness, stray: Stray) {
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let (sut, sut_addr) = h.bind_sut("sut", "127.0.0.1:5080").await;

    let mut options =
        alice.request(OutOfDialogMethod::Options, &bob).through(sut_addr).send().await;
    let packet = sut.recv().await.expect("the OPTIONS reaches the SUT");
    let Ok(SipMessage::Request(req)) = CustomParser::new().parse(&packet.raw) else {
        panic!("a request");
    };
    let opts = GenerateResponseOpts { to_tag: Some("sut-tag".into()), ..Default::default() };
    let answer =
        generate_response(&req, 480, "Temporarily Unavailable", &opts).expect("the SUT's answer");
    if matches!(stray, Stray::First) {
        sut.send_to(answer.image(), h.next_addr()).await.expect("the stray copy leaves");
    }
    sut.send_to(answer.image(), packet.src).await.expect("the answer leaves");
    options.expect(480).await;
    if matches!(stray, Stray::After) {
        sut.send_to(answer.image(), h.next_addr()).await.expect("the stray copy leaves");
    }
}

/// The report's severity of the response-destination finding the run recorded.
fn response_destination_advisory(run: &RunReport) -> Option<bool> {
    let doc = report::seq_doc(run);
    let finding = doc.anomalies.iter().find(|a| a.check == "response-destination");
    finding
        .unwrap_or_else(|| panic!("a response-destination finding: {:?}", doc.anomalies))
        .advisory
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "red because RFC audit rule response-destination fired (charged to sut)")]
async fn a_sut_answering_past_the_via_target_fails_the_run() {
    let h = Harness::new("response-destination-sut-strays");
    sut_answers(&h, Stray::First).await;
    h.finish().await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "red because RFC audit rule response-destination fired (charged to sut)")]
async fn a_sut_repeating_its_answer_past_the_via_target_fails_the_run() {
    let h = Harness::new("response-destination-sut-strays-after");
    sut_answers(&h, Stray::After).await;
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_sut_answering_the_via_target_passes() {
    let h = Harness::new("response-destination-sut-aims");
    sut_answers(&h, Stray::None).await;
    h.finish().await;
}

/// The SUT asks bob for OPTIONS; bob, a fake stack, first sends his 200 to an
/// address no one listens on: reported, not gated.
#[tokio::test(start_paused = true)]
async fn a_simulated_party_answering_past_the_via_target_is_only_reported() {
    const OPTIONS: &str = "OPTIONS sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-sut-dest\r\n\
Max-Forwards: 70\r\n\
From: <sip:sut@127.0.0.1>;tag=s1\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: sut-dest@127.0.0.1\r\n\
CSeq: 1 OPTIONS\r\n\
Content-Length: 0\r\n\r\n";
    let h = Harness::new("response-destination-peer-strays");
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (sut, _) = h.bind_sut("sut", "127.0.0.1:5080").await;
    sut.send_to(OPTIONS.as_bytes(), bob.addr()).await.expect("the OPTIONS leaves");
    let mut at_bob = bob.receive("OPTIONS").await;
    let opts = GenerateResponseOpts { to_tag: Some("bob-tag".into()), ..Default::default() };
    let stray = generate_response(at_bob.request(), 200, "OK", &opts).expect("bob's answer");
    // RFC 3261 §11.2: a 200 to OPTIONS states what bob accepts.
    let stray = String::from_utf8_lossy(stray.image()).replacen(
        "Content-Length:",
        "Allow: OPTIONS\r\nAccept: application/sdp\r\nSupported: timer\r\nContent-Length:",
        1,
    );
    bob.try_send_datagram(stray.as_bytes(), h.next_addr()).await.expect("the stray copy leaves");
    at_bob.respond(200, "OK").await;
    sut.recv().await.expect("the 200 reaches the SUT");
    let run = h.finish().await;
    assert_eq!(
        response_destination_advisory(&run),
        Some(true),
        "a simulated party's finding advises"
    );
}
