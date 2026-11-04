//! RFC 3581 §4 at the hard gate: a request whose top Via asks for `rport`
//! is answered with `rport=<source port>` on the response's top Via. A stack
//! under test that drops the ask fails the run; a simulated party that drops
//! it is reported, never gated.

use scenario_harness::{report, Harness, RunReport};
use sip_message::generators::{generate_response, GenerateResponseOpts, OutOfDialogMethod};
use sip_message::received::observed_source;
use sip_message::{CustomParser, SipMessage, SipParser};

/// Alice, behind a NAT, sends an OPTIONS to the SUT, which answers 480 with
/// its top Via stamped from the source when `stamps`.
async fn sut_answers_an_rport_ask(h: &Harness, stamps: bool) {
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let alice = h.agent("alice", "127.0.0.1:5060").await.behind_nat(h.next_addr());
    let (sut, sut_addr) = h.bind_sut("sut", "127.0.0.1:5080").await;

    let mut options =
        alice.request(OutOfDialogMethod::Options, &bob).through(sut_addr).send().await;
    let packet = sut.recv().await.expect("the OPTIONS reaches the SUT");
    let Ok(SipMessage::Request(req)) = CustomParser::new().parse(&packet.raw) else {
        panic!("a request");
    };
    let opts = GenerateResponseOpts {
        to_tag: Some("sut-tag".into()),
        incoming_source: stamps.then(|| observed_source(packet.src)),
        ..Default::default()
    };
    let answer =
        generate_response(&req, 480, "Temporarily Unavailable", &opts).expect("the SUT's answer");
    sut.send_to(answer.image(), packet.src).await.expect("the answer leaves");
    options.expect(480).await;
}

/// The report's severity of the rport-echo finding the run recorded.
fn rport_echo_advisory(run: &RunReport) -> Option<bool> {
    let doc = report::seq_doc(run);
    let finding = doc.anomalies.iter().find(|a| a.check == "rport-echo");
    finding.unwrap_or_else(|| panic!("an rport-echo finding: {:?}", doc.anomalies)).advisory
}

/// The report reads the SUT's finding as gating, as the gate does.
#[tokio::test(start_paused = true)]
async fn the_report_reads_a_sut_finding_as_gating() {
    let h = Harness::new("rport-echo-sut-report");
    sut_answers_an_rport_ask(&h, false).await;
    assert_eq!(rport_echo_advisory(&h.snapshot_report()), Some(false));
    h.waive(
        scenario_harness::WaiverScope::rule("rport-echo", "the report is the subject")
            .on_party("sut"),
    );
    h.finish().await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "red because RFC audit rule rport-echo fired (charged to sut)")]
async fn a_sut_that_drops_the_rport_ask_fails_the_run() {
    let h = Harness::new("rport-echo-sut-drops");
    sut_answers_an_rport_ask(&h, false).await;
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_sut_that_echoes_rport_passes() {
    let h = Harness::new("rport-echo-sut-echoes");
    sut_answers_an_rport_ask(&h, true).await;
    h.finish().await;
}

/// The SUT asks bob for `rport`; bob, a fake stack, drops the ask: reported,
/// not gated.
#[tokio::test(start_paused = true)]
async fn a_simulated_party_that_drops_the_rport_ask_is_only_reported() {
    const OPTIONS: &str = "OPTIONS sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-sut-rport;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:sut@127.0.0.1>;tag=s1\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: sut-rport@127.0.0.1\r\n\
CSeq: 1 OPTIONS\r\n\
Content-Length: 0\r\n\r\n";
    let h = Harness::new("rport-echo-peer-drops");
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (sut, _) = h.bind_sut("sut", "127.0.0.1:5080").await;
    sut.send_to(OPTIONS.as_bytes(), bob.addr()).await.expect("the OPTIONS leaves");
    bob.receive("OPTIONS").await.respond(200, "OK").await;
    sut.recv().await.expect("the 200 reaches the SUT");
    let run = h.finish().await;
    assert_eq!(rport_echo_advisory(&run), Some(true), "a simulated party's finding advises");
}
