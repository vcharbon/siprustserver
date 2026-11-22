//! A load call's dialog judges read the call's departure log, which its loss
//! model's drops and holds shape: a request a UA sent whose first departure
//! followed the UA's own final reached the SUT behind that final, so a 2xx
//! to it is a `dialog-ended-answered` finding of the call.

use std::time::Duration;

use scenario_harness::dialogs::{Departures, DIALOG_ENDED_ANSWERED};
use scenario_harness::AgentBinder;
use sip_message::generators::InDialogMethod;
use sip_message::sniff;

/// bob rings alice's call, sends her an INFO on the early dialog and refuses
/// the call 487; alice answers the INFO 200. The call's log has the INFO
/// leave before the 487 when `info_first`, else after. Returns the rules of
/// the call's findings; bob's scripted expect of the 200 fails on a verdict.
async fn findings_when(info_first: bool) -> Vec<String> {
    let binder = AgentBinder::fake(1, Duration::from_secs(5), false);
    let log = Departures::default();
    binder.judge_departures(log.clone());
    let alice = binder.agent("alice", "127.0.0.1:5060").await;
    let bob = binder.agent("bob", "127.0.0.1:5070").await;
    let mut call = alice.invite(&bob).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").send().await;
    call.expect(180).await;
    let mut bob_dialog = uas.dialog();
    let (mut info, sent) = bob_dialog
        .send_request(InDialogMethod::Info)
        .try_send_with_request()
        .await
        .expect("the INFO leaves");
    let mut at_alice = alice.receive("INFO").await;
    uas.respond(487, "Request Terminated").await;
    call.expect(487).await;
    uas.expect_ack().await;
    let refusal = format!(
        "SIP/2.0 487 Request Terminated\r\nCall-ID: {}\r\nCSeq: {} INVITE\r\n\r\n",
        sent.call_id().as_str(),
        uas.request().cseq().seq()
    );
    let departed = match info_first {
        true => [sent.image(), refusal.as_bytes()],
        false => [refusal.as_bytes(), sent.image()],
    };
    departed.iter().for_each(|raw| log.note(&sniff::identity(raw)));
    at_alice.respond(200, "OK").await;
    let answered = info.try_expect(200).await;
    assert_eq!(answered.is_ok(), info_first, "the scripted expect fails on the verdict");
    binder.rfc_findings(&[]).findings.into_iter().map(|f| f.rule).collect()
}

/// The INFO left before the 487: alice took it on the live dialog and her
/// 200 is no finding. It left after: her 200 answers a dialog gone.
#[tokio::test(start_paused = true)]
async fn a_2xx_to_a_request_that_departed_behind_the_uas_final_is_a_finding() {
    assert!(findings_when(true).await.is_empty());
    assert_eq!(findings_when(false).await, [DIALOG_ENDED_ANSWERED]);
}
