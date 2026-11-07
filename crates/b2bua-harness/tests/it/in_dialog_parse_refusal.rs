//! An in-dialog request the worker's parser refuses is answered 400 and never
//! reaches the callee, yet its CSeq moved the caller's dialog on (RFC 3261
//! §12.2.1.1): her next request carries the number after it, and the call ends
//! cleanly.

use std::time::Duration;

use b2bua_harness::B2buaScene;
use sip_message::generators::InDialogMethod;

/// Two `Subject` lines on an INFO: a single-valued header stated twice
/// (RFC 3261 §7.3.1).
#[tokio::test(start_paused = true)]
async fn a_refused_info_moves_the_dialog_cseq_and_the_call_ends_cleanly() {
    let s = B2buaScene::new("in-dialog-parse-refusal").await;
    let mut dialog = s.establish().await;
    let mut info = dialog
        .send_request(InDialogMethod::Info)
        .with_header("Subject", "first")
        .with_header("Subject", "second")
        .send()
        .await;
    info.expect(400).await;
    s.h.advance(Duration::from_secs(2)).await;
    let at_bob = s.bob.take_queued().await;
    assert!(at_bob.is_none(), "the refused INFO never reaches bob, got {at_bob:?}");
    s.hangup(&mut dialog).await;
    s.finish().await;
}
