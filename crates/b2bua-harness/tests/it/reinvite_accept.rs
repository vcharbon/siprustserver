//! The advertisement on a re-INVITE the stack originates. `Accept` (RFC 3261
//! §20.1): on a leg it dialled, the one the INVITE that dialled the leg
//! stated; toward the originator, none. `Allow` (§13.2.1: SHOULD be present in
//! an INVITE): the one the leg's dialog-creating INVITE carried, the INVITE the
//! stack sent on a leg it dialled, the originator's own toward her. Driven
//! through the REFER transfer, whose realign re-INVITEs the stack mints toward
//! the transfer target (a leg it dialled) and toward the originator.

use b2bua_harness::{settle_until, B2buaSut};

use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::SipRequest;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ALICE_REALIGN_ANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const CHARLIE_PORT: u16 = 5667;

/// The `Accept` lines of `req`, as sent.
fn accept(req: &SipRequest) -> Vec<String> {
    req.raw(HeaderName::Accept).map(str::to_string).collect()
}

/// The `Allow` lines of `req`, as sent.
fn allow(req: &SipRequest) -> Vec<String> {
    req.raw(HeaderName::Allow).map(str::to_string).collect()
}

/// The `Supported` lines of `req`, as sent.
fn supported(req: &SipRequest) -> Vec<String> {
    req.raw(HeaderName::Supported).map(str::to_string).collect()
}

/// What the transfer target's INVITE, its realign re-INVITE and the
/// originator's realign re-INVITE state as `Accept`.
struct Observed {
    target_invite: Vec<String>,
    target_reinvite: Vec<String>,
    originator_reinvite: Vec<String>,
    /// `Allow` and `Supported` on the same three INVITEs.
    target_invite_allow: Vec<String>,
    target_reinvite_allow: Vec<String>,
    originator_reinvite_allow: Vec<String>,
    reinvite_supported: Vec<String>,
}

/// Alice (stating `caller_accept`) ↔ Bob, Bob REFERs Alice to Charlie under a
/// decision stating `update_headers` (a JSON object, `{}` for none), the
/// transfer completes and Alice hangs up on Charlie.
async fn transfer(name: &str, caller_accept: Option<&str>, update_headers: &str) -> Observed {
    transfer_stating(name, caller_accept, None, update_headers).await
}

/// [`transfer`] with the caller's INVITE stating `caller_allow` too, and
/// `Supported: path` beside it.
async fn transfer_stating(
    name: &str,
    caller_accept: Option<&str>,
    caller_allow: Option<&str>,
    update_headers: &str,
) -> Observed {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", "127.0.0.1:5961").await;
    let bob = h.agent("bob", "127.0.0.1:5971").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5971)
        .start(&h, "b2bua", "127.0.0.1:5981")
        .await;

    let mut invite = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr);
    if let Some(value) = caller_accept {
        invite = invite.with_header("Accept", value);
    }
    if let Some(value) = caller_allow {
        invite = invite.with_header("Allow", value).with_header("Supported", "path");
    }
    let mut call = invite.send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>"))
        .with_header(
            "X-Api-Call",
            &format!(
                r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}},"update_headers":{update_headers}}}"#
            ),
        )
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    let target_invite = accept(charlie_uas.request());
    let target_invite_allow = allow(charlie_uas.request());
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut c_realign = charlie.receive("INVITE").await;
    let target_reinvite = accept(c_realign.request());
    let target_reinvite_allow = allow(c_realign.request());
    let mut reinvite_supported = supported(c_realign.request());
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ANSWER).await;
    charlie.receive("ACK").await;

    let mut a_realign = alice.receive("INVITE").await;
    let originator_reinvite = accept(a_realign.request());
    let originator_reinvite_allow = allow(a_realign.request());
    reinvite_supported.extend(supported(a_realign.request()));
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    alice.receive("ACK").await;

    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
    Observed {
        target_invite,
        target_reinvite,
        originator_reinvite,
        target_invite_allow,
        target_reinvite_allow,
        originator_reinvite_allow,
        reinvite_supported,
    }
}

/// The re-INVITE on the dialled leg restates the `Accept` its INVITE stated
/// there; the originator's re-INVITE states none.
#[tokio::test]
async fn a_reinvite_on_a_dialled_leg_restates_the_accept_its_invite_stated() {
    let seen = transfer(
        "reinvite-accept-restated",
        Some("application/sdp, text/plain;charset=utf-8"),
        "{}",
    )
    .await;
    assert_eq!(seen.target_invite, ["application/sdp, text/plain;charset=utf-8"]);
    assert_eq!(seen.target_reinvite, seen.target_invite, "the dialled leg's own Accept");
    assert_eq!(seen.originator_reinvite, Vec::<String>::new(), "none toward the originator");
}

/// A dialled leg whose INVITE stated no `Accept` is sent none on a re-INVITE.
#[tokio::test]
async fn a_dialled_leg_whose_invite_stated_no_accept_is_sent_none() {
    let seen = transfer("reinvite-accept-none", None, "{}").await;
    assert_eq!(seen.target_invite, Vec::<String>::new());
    assert_eq!(seen.target_reinvite, Vec::<String>::new());
    assert_eq!(seen.originator_reinvite, Vec::<String>::new());
}

/// The `Accept` restated is the one the dialling INVITE stated as it left, the
/// decision's header edit applied, not the originator's.
#[tokio::test]
async fn a_reinvite_restates_the_dialled_invite_accept_as_edited_not_the_originators() {
    let seen = transfer(
        "reinvite-accept-edited",
        Some("application/sdp, text/plain"),
        r#"{"Accept":"application/sdp, text/html"}"#,
    )
    .await;
    assert_eq!(seen.target_invite, ["application/sdp, text/html"], "the edit reaches the INVITE");
    assert_eq!(seen.target_reinvite, ["application/sdp, text/html"], "the edited Accept, restated");
    assert_eq!(seen.originator_reinvite, Vec::<String>::new());
}

/// The `Allow` a re-INVITE carries is the one the leg's dialog-creating INVITE
/// carried: on the dialled leg the INVITE the stack sent (the decision's edit
/// applied), toward the originator her own. Neither re-INVITE states the
/// option tags.
#[tokio::test]
async fn a_reinvite_restates_the_allow_its_dialog_was_created_with() {
    let seen = transfer_stating(
        "reinvite-allow-restated",
        None,
        Some("INVITE, ACK, BYE, CANCEL, OPTIONS"),
        r#"{"Allow":"INVITE, ACK, BYE, CANCEL, OPTIONS, INFO, REFER"}"#,
    )
    .await;
    assert_eq!(seen.target_invite_allow, ["INVITE, ACK, BYE, CANCEL, OPTIONS, INFO, REFER"]);
    assert_eq!(seen.target_reinvite_allow, seen.target_invite_allow, "the dialled leg's own");
    assert_eq!(
        seen.originator_reinvite_allow,
        ["INVITE, ACK, BYE, CANCEL, OPTIONS"],
        "the originator's own Allow"
    );
    assert_eq!(seen.reinvite_supported, Vec::<String>::new(), "no option tags of the stack's");
}

/// A leg whose dialog-creating INVITE carried no `Allow` is sent none.
#[tokio::test]
async fn a_dialog_created_without_allow_is_sent_none_on_a_reinvite() {
    let seen = transfer("reinvite-allow-none", None, "{}").await;
    assert_eq!(seen.target_reinvite_allow, Vec::<String>::new());
    assert_eq!(seen.originator_reinvite_allow, Vec::<String>::new());
}
