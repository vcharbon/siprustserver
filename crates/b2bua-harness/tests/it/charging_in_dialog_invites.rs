//! RFC 7315 §5.6 charging correlation on the INVITEs the stack sends inside a
//! dialog on its own behalf (`ChargingVectorFeature::in_dialog_invites`).
//! Such a re-INVITE carries the vector of the leg it travels: the one that
//! leg's dialog-creating INVITE carried (the originator's own on the
//! originator's leg), none where it carried none. A relayed re-INVITE carries
//! what its sender sent, a decision's statement outranks the stamp, and without
//! the option the stack's re-INVITEs carry none.

use std::sync::Arc;

use b2bua::decision::test_adapter::{route_to, route_to_processing_refer, route_to_with_18x};
use b2bua::decision::{default_call_refer, HeaderUpdate, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{hangup, settle_until, B2buaSut};
use call::features::{ChargingVectorFeature, RelayFirst18xStrategy, StatedHeaders};
use scenario_harness::{Agent, Dialog, Harness};
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::SipRequest;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const ORIGINATOR_OWN: &str = "icid-value=edge-0001;icid-generated-at=edge.example.com";

/// The vector lines `req` carries.
fn vectors(req: &SipRequest) -> Vec<String> {
    req.raw(HeaderName::from("P-Charging-Vector")).map(str::to_string).collect()
}

/// The REFER transfer's every message as seen at the three agents, past the
/// realign: the vectors on the originated INVITEs and on the stack's two
/// re-INVITEs.
struct Seen {
    bob_invite: Vec<String>,
    charlie_invite: Vec<String>,
    charlie_reinvite: Vec<String>,
    alice_reinvite: SipRequest,
}

/// Alice calls bob, bob REFERs her to charlie, the stack realigns charlie and
/// alice by re-INVITE, then alice hangs up. `originator` is the vector alice's
/// INVITE carries; `option` arms `in_dialog_invites` beside the charging arm.
async fn transfer(
    name: &str,
    ports: (u16, u16, u16, u16),
    originator: Option<&str>,
    option: bool,
) -> Seen {
    transfer_stating(name, ports, originator, option, None).await
}

/// [`transfer`] under a route stating `stated` beside the charging arm.
async fn transfer_stating(
    name: &str,
    ports: (u16, u16, u16, u16),
    originator: Option<&str>,
    option: bool,
    stated: Option<StatedHeaders>,
) -> Seen {
    let (alice_port, bob_port, charlie_port, b2bua_port) = ports;
    let h = Harness::new(name);
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_port}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{charlie_port}")).await;
    let engine = ScriptedDecisionEngine::builder()
        .fallback(move |_req| {
            let mut route = route_to_processing_refer("127.0.0.1", bob_port);
            route.features.charging_vector =
                Some(ChargingVectorFeature { generated_at: None, in_dialog_invites: option });
            route.features.stated_headers = stated.clone();
            NewCallResponse::Route(route)
        })
        .on_refer(default_call_refer)
        .build();
    let b2bua = B2buaSut::builder(Arc::new(engine))
        .start(&h, "b2bua", &format!("127.0.0.1:{b2bua_port}"))
        .await;

    let mut invite = alice.invite(&bob).with_sdp(OFFER);
    if let Some(own) = originator {
        invite = invite.with_header("P-Charging-Vector", own);
    }
    let mut call = invite.through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    let bob_invite = vectors(bob_uas.request());
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let x_api = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{charlie_port}}}}}"#
    );
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{charlie_port}>"))
        .with_header("X-Api-Call", &x_api)
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    let charlie_invite = vectors(charlie_uas.request());
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut c_realign = charlie.receive("INVITE").await;
    let charlie_reinvite = vectors(c_realign.request());
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ANSWER).await;
    charlie.receive("ACK").await;

    let mut a_realign = alice.receive("INVITE").await;
    let alice_reinvite = a_realign.request().clone();
    a_realign.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    alice.receive("ACK").await;
    hang_up(&mut alice_dialog, &bob, &charlie).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
    Seen { bob_invite, charlie_invite, charlie_reinvite, alice_reinvite }
}

/// Alice hangs up the transferred call: her BYE reaches charlie, and bob, the
/// released transferor, is BYE'd too.
async fn hang_up(alice_dialog: &mut Dialog, bob: &Agent, charlie: &Agent) {
    let mut bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
}

#[tokio::test(start_paused = true)]
async fn the_stacks_reinvites_carry_their_legs_vector_and_none_toward_an_originator_who_sent_none()
{
    let seen = transfer("charging-in-dialog-minted", (5961, 5962, 5963, 5964), None, true).await;
    assert_eq!(seen.bob_invite.len(), 1, "the arm mints on the dialled leg's INVITE");
    assert_eq!(seen.charlie_invite.len(), 1, "the arm mints on the transferee's INVITE");
    assert_ne!(seen.bob_invite, seen.charlie_invite, "each originated leg has its own vector");
    assert_eq!(
        seen.charlie_reinvite, seen.charlie_invite,
        "the re-INVITE toward the transferee carries the vector its INVITE carried"
    );
    assert!(vectors(&seen.alice_reinvite).is_empty(), "the originator sent none, so none rides");
}

#[tokio::test(start_paused = true)]
async fn the_stacks_reinvites_carry_the_originators_vector_where_one_arrived() {
    let seen = transfer(
        "charging-in-dialog-relayed",
        (5965, 5966, 5967, 5968),
        Some(ORIGINATOR_OWN),
        true,
    )
    .await;
    assert_eq!(
        seen.bob_invite,
        [ORIGINATOR_OWN],
        "the originator's vector relays to the dialled leg"
    );
    assert_eq!(seen.charlie_invite, [ORIGINATOR_OWN], "and to the transferee");
    assert_eq!(
        seen.charlie_reinvite,
        [ORIGINATOR_OWN],
        "the transferee's re-INVITE carries its leg's vector"
    );
    assert_eq!(
        vectors(&seen.alice_reinvite),
        [ORIGINATOR_OWN],
        "the originator's re-INVITE carries its own"
    );
}

#[tokio::test(start_paused = true)]
async fn without_the_option_the_stacks_reinvites_carry_none() {
    let seen = transfer("charging-in-dialog-off", (5969, 5970, 5971, 5972), None, false).await;
    assert_eq!(seen.bob_invite.len(), 1, "the arm still mints on an originated leg's INVITE");
    assert!(seen.charlie_reinvite.is_empty(), "no vector on the transferee's re-INVITE");
    assert!(vectors(&seen.alice_reinvite).is_empty(), "no vector on the originator's re-INVITE");
}

fn armed() -> Option<ChargingVectorFeature> {
    Some(ChargingVectorFeature { generated_at: None, in_dialog_invites: true })
}

#[tokio::test(start_paused = true)]
async fn a_statement_removing_the_vector_outranks_the_stamp() {
    let mut stated = StatedHeaders::default();
    stated.every_message.insert("P-Charging-Vector".into(), HeaderUpdate::Remove);
    let seen = transfer_stating(
        "charging-in-dialog-stated-removal",
        (5973, 5974, 5975, 5976),
        None,
        true,
        Some(stated),
    )
    .await;
    assert!(seen.bob_invite.is_empty() && seen.charlie_invite.is_empty(), "the removal mints none");
    assert!(seen.charlie_reinvite.is_empty(), "the removal reaches the transferee's re-INVITE");
    assert!(vectors(&seen.alice_reinvite).is_empty(), "and the originator's");
}

#[tokio::test(start_paused = true)]
async fn a_relayed_reinvite_carries_what_its_sender_sent() {
    let h = Harness::new("charging-in-dialog-relayed-reinvite");
    let alice = h.agent("alice", "127.0.0.1:5977").await;
    let bob = h.agent("bob", "127.0.0.1:5978").await;
    let engine = ScriptedDecisionEngine::builder()
        .fallback(|_req| {
            let mut route = route_to("127.0.0.1", 5978);
            route.features.charging_vector = armed();
            NewCallResponse::Route(route)
        })
        .build();
    let b2bua = B2buaSut::builder(Arc::new(engine)).start(&h, "b2bua", "127.0.0.1:5979").await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    assert_eq!(vectors(bob_uas.request()).len(), 1, "the arm mints on the dialled INVITE");
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reoffer = alice_dialog.request(InDialogMethod::Invite, Some(ALICE_REANSWER)).await;
    let mut relayed = bob.receive("INVITE").await;
    assert!(vectors(relayed.request()).is_empty(), "the relayed re-INVITE gains no vector");
    relayed.respond(200, "OK").with_sdp(ANSWER).await;
    reoffer.expect(200).await;
    alice_dialog.ack(None).await;
    bob.receive("ACK").await;
    hangup(&mut alice_dialog, &bob).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

const EARLY: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const FINAL_DIFF: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

#[tokio::test(start_paused = true)]
async fn a_resync_toward_the_originator_carries_its_own_vector() {
    let h = Harness::new("charging-in-dialog-promote-resync");
    let alice = h.agent("alice", "127.0.0.1:5980").await;
    let bob = h.agent("bob", "127.0.0.1:5981").await;
    let engine = ScriptedDecisionEngine::builder()
        .fallback(|_req| {
            let mut route =
                route_to_with_18x("127.0.0.1", 5981, RelayFirst18xStrategy::PromotePemTo200);
            route.features.charging_vector = armed();
            NewCallResponse::Route(route)
        })
        .build();
    let b2bua = B2buaSut::builder(Arc::new(engine)).start(&h, "b2bua", "127.0.0.1:5982").await;
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("P-Charging-Vector", ORIGINATOR_OWN)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    uas.respond(200, "OK").with_sdp(FINAL_DIFF).await;
    bob.receive("ACK").await;

    let mut resync = alice.receive("INVITE").await;
    assert_eq!(
        vectors(resync.request()),
        [ORIGINATOR_OWN],
        "the resync carries the originator's own"
    );
    resync.respond(200, "OK").with_sdp(EARLY).await;
    alice.receive("ACK").await;
    hangup(&mut dialog, &bob).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
