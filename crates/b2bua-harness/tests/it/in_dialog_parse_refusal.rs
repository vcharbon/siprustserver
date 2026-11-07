//! An in-dialog request the worker's parser refuses is answered 400 and never
//! reaches the callee, yet its CSeq moved the caller's dialog on (RFC 3261
//! §12.2.1.1): her next request carries the number after it, and the call ends
//! cleanly.

use std::time::Duration;

use b2bua_harness::B2buaScene;
use sip_message::SipMessage;

/// Two `Subject` lines on an INFO: a single-valued header stated twice
/// (RFC 3261 §7.3.1), which no builder writes, so alice's INFO is her ACK's
/// identity written by hand at the dialog's next CSeq.
#[tokio::test(start_paused = true)]
async fn a_refused_info_moves_the_dialog_cseq_and_the_call_ends_cleanly() {
    let s = B2buaScene::new("in-dialog-parse-refusal").await;
    let mut dialog = s.establish().await;
    let next = dialog.local_cseq() + 1;
    let ack =
        s.h.wire_entries()
            .into_iter()
            .find(|e| e.from == s.alice.addr() && e.raw.starts_with(b"ACK "))
            .expect("alice's ACK");
    let info = String::from_utf8(ack.raw)
        .expect("a text ACK")
        .replacen("ACK ", "INFO ", 1)
        .replace(&format!("CSeq: {} ACK", next - 1), &format!("CSeq: {next} INFO"))
        .replace(";branch=z9hG4bK", ";branch=z9hG4bK-info")
        .replace("Content-Length:", "Subject: first\r\nSubject: second\r\nContent-Length:");
    assert!(info.contains(&format!("CSeq: {next} INFO")), "{info}");
    s.alice.try_send_datagram(info.as_bytes(), s.b2bua.addr).await.expect("the INFO leaves");
    s.h.advance(Duration::from_secs(2)).await;
    let refused = s.alice.take_queued().await;
    assert!(
        matches!(&refused, Some(SipMessage::Response(r)) if r.status() == 400),
        "the worker answers 400, got {refused:?}"
    );
    let at_bob = s.bob.take_queued().await;
    assert!(at_bob.is_none(), "the refused INFO never reaches bob, got {at_bob:?}");
    dialog.set_local_cseq(next);
    s.hangup(&mut dialog).await;
    s.finish().await;
}
