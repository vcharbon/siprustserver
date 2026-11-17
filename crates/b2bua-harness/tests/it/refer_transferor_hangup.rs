//! The transferor hangs up after a blind transfer (RFC 5589 §6.1, RFC 3515).
//!
//! Once the referrer B is told the transfer succeeded (NOTIFY sipfrag 200), its
//! dialog with the transferee is no longer part of the session: B's BYE ends
//! B's leg alone, and the transferred call A↔C stays up. When B has not hung
//! up by the time A↔C is bridged, the stack releases B itself; a BYE of B's
//! crossing that release is answered 200 (RFC 3261 §15.1.2).

use b2bua_harness::{settle_until, B2buaSut};

use scenario_harness::{Agent, Dialog, Harness, ServerTxn};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_FIRST_ANSWER: &str = "v=0\r\no=charlie 9 8 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_ACTIVE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ALICE_REALIGN_ANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const CHARLIE_PORT: u16 = 4867;

/// The parties of a transfer whose success NOTIFY B has just answered, C's
/// session-continuing re-INVITE received and not yet answered.
struct Transferred {
    h: Harness,
    b2bua: B2buaSut,
    alice: Agent,
    bob: Agent,
    charlie: Agent,
    alice_dialog: Dialog,
    bob_dialog: Dialog,
    charlie_dialog: Dialog,
    c_realign: ServerTxn,
}

/// A↔B established through the stack, B REFERs to C, C answers: B is told
/// the transfer succeeded and the stack re-INVITEs C onto A's session.
async fn transfer_reported_to_bob(name: &str, alice_port: u16, bob_port: u16) -> Transferred {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_port}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
        .start(&h, "b2bua", &format!("127.0.0.1:{}", bob_port + 10))
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let refer_to = format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>");
    let x_api_call = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
    );
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to)
        .with_header("X-Api-Call", &x_api_call)
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(CHARLIE_FIRST_ANSWER).await;
    charlie.receive("ACK").await;
    let charlie_dialog = charlie_uas.dialog();

    let mut success = bob.receive("NOTIFY").await;
    let sipfrag = String::from_utf8_lossy(success.request().body()).into_owned();
    assert!(sipfrag.starts_with("SIP/2.0 200"), "B is told the transfer succeeded: {sipfrag:?}");
    success.respond(200, "OK").await;

    let c_realign = charlie.receive("INVITE").await;
    Transferred {
        h,
        b2bua,
        alice,
        bob,
        charlie,
        alice_dialog,
        bob_dialog,
        charlie_dialog,
        c_realign,
    }
}

/// C answers the session-continuing re-INVITE, then A answers hers: A↔C bridged.
async fn realign(t: &mut Transferred) -> ServerTxn {
    t.c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    t.charlie.receive("ACK").await;
    t.alice.receive("INVITE").await
}

async fn bridge(t: &mut Transferred) {
    let mut a_realign = realign(t).await;
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    t.alice.receive("ACK").await;
}

/// C hangs up the transferred call: the BYE reaches A, the call ends.
async fn charlie_hangs_up(mut t: Transferred) {
    let mut charlie_bye = t.charlie_dialog.bye().await;
    t.alice.receive("BYE").await.respond(200, "OK").await;
    charlie_bye.expect(200).await;
    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    let _ = t.alice_dialog;
    let _ = t.h.finish().await;
}

#[tokio::test]
async fn the_transferor_hanging_up_on_the_success_notify_leaves_the_transferred_call_up() {
    let mut t = transfer_reported_to_bob("transferor-bye-on-success-notify", 4861, 4871).await;

    let mut bob_bye = t.bob_dialog.bye().await;
    bob_bye.expect(200).await;

    bridge(&mut t).await;
    charlie_hangs_up(t).await;
}

#[tokio::test]
async fn the_transferor_hanging_up_while_the_transferee_is_realigned_leaves_the_transferred_call_up(
) {
    let mut t = transfer_reported_to_bob("transferor-bye-during-a-realign", 4862, 4872).await;
    let mut a_realign = realign(&mut t).await;

    let mut bob_bye = t.bob_dialog.bye().await;
    bob_bye.expect(200).await;

    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    t.alice.receive("ACK").await;
    charlie_hangs_up(t).await;
}

#[tokio::test]
async fn the_transferor_still_up_when_the_transfer_completes_is_released() {
    let mut t = transfer_reported_to_bob("transferor-released-on-completion", 4863, 4873).await;

    bridge(&mut t).await;
    t.bob.receive("BYE").await.respond(200, "OK").await;

    let mut alice_bye = t.alice_dialog.bye().await;
    t.charlie.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    let _ = t.h.finish().await;
}

/// B's BYE leaves after the stack's release of B left, before B reads it: the
/// two BYEs cross on B's dialog and each is answered 200.
#[tokio::test]
async fn the_transferors_bye_crossing_its_release_leaves_the_transferred_call_up() {
    let mut t = transfer_reported_to_bob("transferor-bye-crosses-release", 4864, 4874).await;

    bridge(&mut t).await;
    let mut bob_bye = t.bob_dialog.bye().await;
    t.bob.receive("BYE").await.respond(200, "OK").await;
    bob_bye.expect(200).await;

    charlie_hangs_up(t).await;
}
