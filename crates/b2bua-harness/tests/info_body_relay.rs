//! Arbitrary-MIME in-dialog INFO body survives the B2BUA relay (newkahneed-020).
//!
//! The `scenario-harness` `InDialogRequest::with_body(content_type, bytes)`
//! primitive lets a test drive an in-dialog request with a real, arbitrary-MIME
//! (binary-safe) body — an `application/orangeindata` SUP payload, a
//! `multipart/mixed` dual-part body, a `User-To-User` INFO, etc. — instead of
//! only an `application/sdp` body via `with_sdp`.
//!
//! These tests pin what the SUT's `relay-info` rule (`RelayToPeer`, no transform)
//! does with such a body: the relayed INFO reaching the peer leg must carry BOTH
//! the exact `Content-Type` AND the exact body bytes (the relay copies
//! `req.body` and the inbound `content-type` header verbatim — RFC 3261
//! §16.6/§12.2.1.1). This is the positive counterpart to the note's observation
//! that a *bodyless* `Content-Type` is dropped on re-relay (the generator only
//! stamps `Content-Type` when the body is non-empty): a real body survives.

use b2bua_harness::B2buaScene;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

/// An established call; alice drives an in-dialog INFO with an
/// `application/orangeindata` body; the relayed INFO reaching bob carries the
/// exact Content-Type and body bytes.
#[tokio::test(start_paused = true)]
async fn info_with_arbitrary_body_relays_content_type_and_bytes() {
    let s = B2buaScene::new("b2bua-info-body-relay").await;
    let mut dialog = s.establish().await;
    assert_eq!(s.b2bua.active_calls(), 1, "call established");

    const CT: &str = "application/orangeindata";
    let body: Vec<u8> = b"SUP:role=agent;priority=high;\x00\x01\x02payload".to_vec();

    // ── alice INFO with the arbitrary-MIME body ──
    let mut info = dialog
        .send_request(InDialogMethod::Info)
        .with_body(CT, body.clone())
        .send()
        .await;

    // ── the relayed INFO reaching bob must preserve Content-Type AND the bytes ──
    let mut bob_uas = s.bob.receive("INFO").await;
    {
        let req = bob_uas.request();
        assert_eq!(
            get_header(&req.headers, "Content-Type"),
            Some(CT),
            "relayed INFO carries the exact Content-Type",
        );
        assert_eq!(req.body, body, "relayed INFO carries the exact (binary-safe) body bytes");
        assert_eq!(
            get_header(&req.headers, "Content-Length"),
            Some(body.len().to_string().as_str()),
            "Content-Length matches the relayed body length",
        );
    }
    bob_uas.respond(200, "OK").await;
    info.expect(200).await;
    assert_eq!(s.b2bua.active_calls(), 1, "INFO transaction left the call up");

    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// The dual-part case the note calls out for INFO_UUI priority: a
/// `multipart/mixed` body bearing a `User-To-User` part and an orangeindata part
/// relays through verbatim (both the multipart Content-Type and the exact CRLF-
/// delimited bytes survive).
#[tokio::test(start_paused = true)]
async fn info_with_multipart_body_relays_verbatim() {
    let s = B2buaScene::new("b2bua-info-multipart-relay").await;
    let mut dialog = s.establish().await;

    const CT: &str = "multipart/mixed;boundary=uui-boundary";
    let body: Vec<u8> = concat!(
        "--uui-boundary\r\n",
        "Content-Type: application/vnd.3gpp.sms\r\n\r\n",
        "User-To-User=3132333435;encoding=hex\r\n",
        "--uui-boundary\r\n",
        "Content-Type: application/orangeindata\r\n\r\n",
        "SUP:priority=1;\r\n",
        "--uui-boundary--\r\n",
    )
    .as_bytes()
    .to_vec();

    let mut info = dialog
        .send_request(InDialogMethod::Info)
        .with_body(CT, body.clone())
        .send()
        .await;

    let mut bob_uas = s.bob.receive("INFO").await;
    {
        let req = bob_uas.request();
        assert_eq!(
            get_header(&req.headers, "Content-Type"),
            Some(CT),
            "multipart Content-Type survives",
        );
        assert_eq!(req.body, body, "dual-part body relays verbatim");
    }
    bob_uas.respond(200, "OK").await;
    info.expect(200).await;

    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}
