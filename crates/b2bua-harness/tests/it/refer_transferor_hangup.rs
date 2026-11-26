//! The transferor hangs up after a blind transfer (RFC 5589 §6.1, RFC 3515).
//!
//! Once the referrer B is told the transfer succeeded (NOTIFY sipfrag 200), its
//! dialog with the transferee is no longer part of the session: B's BYE ends
//! B's leg alone, and the transferred call A↔C stays up. When B has not hung
//! up by the time A↔C is bridged, the stack releases B itself; a BYE of B's
//! crossing that release is answered 200 (RFC 3261 §15.1.2).

use b2bua::config::{B2buaConfig, Cap};
use b2bua_harness::{settle_until, B2buaSut};
use call::{CallModelState, CdrEventType, MsgpackCodec};

use scenario_harness::{Agent, Dialog, Harness, InDialogTxn, ServerTxn};
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

/// The call's Call-ID and alice's From-tag.
const CALL_ID: &str = "transferor-hangup@127.0.0.1";
const FROM_TAG: &str = "transferor-hangup-tag";

/// A↔B established through the stack, B REFERs to C, C answers: B is told
/// the transfer succeeded and the stack re-INVITEs C onto A's session.
async fn transfer_reported_to_bob(name: &str, alice_port: u16, bob_port: u16) -> Transferred {
    transfer_reported_under(name, alice_port, bob_port, |_| {}).await
}

/// [`transfer_reported_to_bob`] on a stack whose configuration `tune` sets.
async fn transfer_reported_under(
    name: &str,
    alice_port: u16,
    bob_port: u16,
    tune: impl FnOnce(&mut B2buaConfig) + 'static,
) -> Transferred {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_port}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
        .tune(tune)
        .start(&h, "b2bua", &format!("127.0.0.1:{}", bob_port + 10))
        .await;

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
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

/// C hangs up the transferred call: the BYE reaches A, the call ends. Returns
/// the reaped stack, its CDR readable.
async fn charlie_hangs_up(mut t: Transferred) -> B2buaSut {
    let mut charlie_bye = t.charlie_dialog.bye().await;
    t.alice.receive("BYE").await.respond(200, "OK").await;
    charlie_bye.expect(200).await;
    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    let _ = t.h.finish().await;
    t.b2bua
}

/// The call's CDR events, as `(type, leg)` in the order written.
fn cdr_events(b2bua: &B2buaSut) -> Vec<(CdrEventType, String)> {
    b2bua
        .cdr_records()
        .iter()
        .flat_map(|r| r.events.iter().map(|e| (e.event_type, e.leg_id.clone())))
        .collect()
}

#[tokio::test]
async fn the_transferor_hanging_up_on_the_success_notify_leaves_the_transferred_call_up() {
    let mut t = transfer_reported_to_bob("transferor-bye-on-success-notify", 4861, 4871).await;
    let bob_leg = "b-1".to_string();

    let mut bob_bye = t.bob_dialog.bye().await;
    bob_bye.expect(200).await;

    bridge(&mut t).await;
    let b2bua = charlie_hangs_up(t).await;
    let events = cdr_events(&b2bua);
    let byes_of_bob: Vec<usize> =
        (0..events.len()).filter(|&i| events[i] == (CdrEventType::Bye, bob_leg.clone())).collect();
    let transfer_answered = events.iter().position(|e| *e == (CdrEventType::Answer, "a".into()));
    assert_eq!(byes_of_bob.len(), 1, "B's own hangup is written once: {events:?}");
    assert!(
        transfer_answered.is_some_and(|at| byes_of_bob[0] < at),
        "B's hangup is written before the transfer completes: {events:?}"
    );
}

/// C hangs up while the stack re-INVITEs it onto A's session: the transfer
/// target is gone, so the call ends and A and B are BYE'd.
#[tokio::test]
async fn the_transfer_targets_bye_while_realigned_ends_the_call() {
    let mut t = transfer_reported_to_bob("transfer-target-bye-during-c-realign", 4865, 4875).await;

    let mut charlie_bye = t.charlie_dialog.bye().await;
    charlie_bye.expect(200).await;
    t.alice.receive("BYE").await.respond(200, "OK").await;
    t.bob.receive("BYE").await.respond(200, "OK").await;
    let _ = &mut t.c_realign;

    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    let _ = t.h.finish().await;
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

/// The transferred call's callRef.
fn call_ref() -> String {
    call::derive_call_ref("w0", CALL_ID, FROM_TAG)
}

/// The encoded size of the transferred call's live record.
fn live_record_bytes(b2bua: &B2buaSut) -> u64 {
    let call = b2bua.live_call(&call_ref()).expect("the call is live");
    MsgpackCodec::new().encoded_len(&call).expect("the call encodes")
}

/// A stack whose calls keep their messages in the record, each entry with
/// its `Reason` values, under the record-size cap of `record_bytes`.
fn ringed(record_bytes: u64) -> impl FnOnce(&mut B2buaConfig) + 'static {
    move |c| {
        c.cdr.message_ring = 32;
        c.cdr.captured_headers = vec!["Reason".to_string()];
        c.caps.record_bytes = record_bytes;
    }
}

/// B hangs up stating why (RFC 3326) at a length the record's ring entry
/// keeps byte for byte.
async fn bob_hangs_up_stating_why(t: &mut Transferred) -> InDialogTxn {
    let reason = format!("SIP;cause=200;text=\"{}\"", "x".repeat(2048));
    t.bob_dialog.send_request(InDialogMethod::Bye).with_header("Reason", &reason).send().await
}

/// B's BYE ends B's leg of a call that goes on, and the record its turn
/// lands crosses the record-size cap. A BYE is a teardown, never refused: it
/// is answered 200 and no cap trips. The call's next held write, C's 200 to
/// the realigning re-INVITE, trips: A and C are BYEd, one CDR, the call
/// reaped.
#[tokio::test(start_paused = true)]
async fn the_transferors_bye_past_the_record_size_cap_lands_and_the_next_write_ends_the_call() {
    const NAME: &str = "transferor-bye-record-cap";
    let (before, after) = {
        let mut t = transfer_reported_under(NAME, 4866, 4876, ringed(1 << 30)).await;
        let before = t.b2bua.metrics().cap_peak_of(Cap::RecordBytes);
        bob_hangs_up_stating_why(&mut t).await.expect(200).await;
        let after = live_record_bytes(&t.b2bua);
        bridge(&mut t).await;
        charlie_hangs_up(t).await;
        (before, after)
    };
    assert!(after > before + 1, "B's BYE grows the record: {before} B, then {after} B");
    let cap = before + (after - before) / 2;

    let mut t = transfer_reported_under(NAME, 4866, 4876, ringed(cap)).await;
    bob_hangs_up_stating_why(&mut t).await.expect(200).await;
    let m = t.b2bua.metrics();
    assert!(Cap::ALL.iter().all(|c| m.cap_trips_of(*c) == 0), "no cap trips");
    let live = t.b2bua.live_call(&call_ref()).map(|c| c.state);
    assert_eq!(live, Some(CallModelState::Active), "B's BYE leaves the call up");

    t.c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    t.alice.receive("BYE").await.respond(200, "OK").await;
    t.charlie.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    assert_eq!(t.b2bua.cdr_records().len(), 1, "one CDR");
    assert_eq!(t.b2bua.metrics().cap_trips_of(Cap::RecordBytes), 1, "C's 200 tripped the cap");
    let _ = t.h.finish().await;
}
