//! The service's form hook (`B2buaConfig::sdp_form`): the stack asks it about
//! every in-dialog description it sends and re-serializes the ones it names
//! (`sip_message::canonical_form`).
//!
//! The test policy re-serializes a peer's description once the call has a
//! restatement. A↔B relays as written; B transfers A to C, and from the realign
//! on every peer description leaves in canonical form — the direction moved
//! after `a=ptime` or stated where the author wrote none — on both legs.

use std::sync::Arc;

use b2bua::config::{SdpCrossing, SdpForm, SdpFormPolicy};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const CHARLIE_PORT: u16 = 5669;

/// The direction before `a=ptime`.
const ALICE_OFFER: &str = "v=0\r\no=alice 101 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\na=ptime:20\r\n";
const BOB_ANSWER: &str = "v=0\r\no=bob 202 1 IN IP4 127.0.0.2\r\ns=-\r\nc=IN IP4 127.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\na=ptime:20\r\n";
const CHARLIE_HELD_ANSWER: &str = "v=0\r\no=charlie 303 1 IN IP4 127.0.0.3\r\ns=-\r\nc=IN IP4 127.0.0.3\r\nt=0 0\r\nm=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=inactive\r\n";
/// No direction attribute.
const CHARLIE_ACTIVE: &str = "v=0\r\no=charlie 303 2 IN IP4 127.0.0.3\r\ns=-\r\nc=IN IP4 127.0.0.3\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=ptime:20\r\n";
const ALICE_REALIGNED: &str = "v=0\r\no=alice 101 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\na=ptime:20\r\n";

#[derive(Debug)]
struct CanonicalOnceRestated;

impl SdpFormPolicy for CanonicalOnceRestated {
    fn form(&self, crossing: &SdpCrossing) -> SdpForm {
        if crossing.by_peer && crossing.call_restated {
            SdpForm::Canonical
        } else {
            SdpForm::AsWritten
        }
    }
}

fn x_api_allow_c() -> String {
    format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
    )
}

/// `authored` after its `o=` line.
fn after_origin(authored: &str) -> String {
    authored.split_inclusive("\r\n").filter(|l| !l.starts_with("o=")).collect()
}

#[tokio::test]
async fn the_service_policy_forms_descriptions_after_the_calls_restatement() {
    let h = Harness::with_transit_delay("sdp-form-policy-transfer", 1);
    let alice = h.agent("alice", "127.0.0.1:5934").await;
    let bob = h.agent("bob", "127.0.0.1:5944").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5944)
        .tune(|c| c.sdp_form = Arc::new(CanonicalOnceRestated))
        .start(&h, "b2bua", "127.0.0.1:5954")
        .await;

    // Before any restatement every description relays as written.
    let mut call = alice.invite(&bob).with_sdp(ALICE_OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    assert_eq!(String::from_utf8_lossy(bob_uas.request().body()), ALICE_OFFER);
    bob_uas.respond(200, "OK").with_sdp(BOB_ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(String::from_utf8_lossy(ok.body()), BOB_ANSWER);
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>"))
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(CHARLIE_HELD_ANSWER).await;
    charlie.receive("ACK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    // The c-realign restates A's offer toward C: its direction after a=ptime.
    let mut c_realign = charlie.receive("INVITE").await;
    assert_eq!(
        after_origin(&String::from_utf8_lossy(c_realign.request().body())),
        "v=0\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=ptime:20\r\na=sendrecv\r\n",
    );
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE).await;
    charlie.receive("ACK").await;

    // The a-realign carries C's description with its direction stated.
    let mut a_realign = alice.receive("INVITE").await;
    assert_eq!(
        after_origin(&String::from_utf8_lossy(a_realign.request().body())),
        format!("{}a=sendrecv\r\n", after_origin(CHARLIE_ACTIVE)),
    );
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGNED).await;
    alice.receive("ACK").await;

    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let _ = h.finish().await;
}
