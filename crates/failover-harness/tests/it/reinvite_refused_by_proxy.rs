//! A re-INVITE the proxy refuses statelessly: its final carries the dialog's
//! To-tag, and its RFC 3261 §17.1.1.3 ACK, on the INVITE's branch, ends at the
//! proxy, whose memo of the reject recognises it: no worker sees it, and §14.1
//! leaves the session as if no re-INVITE had been issued, on a call a takeover
//! copy served included. An ACK past the memo's life, or reaching another proxy,
//! still reaches a worker. The rig holds the real proxy and the real workers.
//!
//! Beyond the failover sweep: a refusal by the proxy, which no sweep shape holds.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, Incident};
use scenario_harness::InDialogTxn;
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{
    crash_primary, establish, reboot_and_reclaim, Established, OFFER,
};
use crate::inflight_resend::{split_mut, REANSWER};

/// Nothing further reaches either party, then the call ends by alice's BYE and
/// leaves nothing behind.
async fn the_call_stays_up_then_hangs_up(est: &mut Established) {
    est.fh.advance(Duration::from_secs(2)).await;
    let at_bob = est.bob.take_queued().await;
    assert!(
        at_bob.is_none(),
        "bob hears nothing of a re-INVITE that never reached him (RFC 3261 §14.1), got {:?}",
        at_bob.map(|m| (m.cseq().seq(), m.cseq().method().as_str().to_string()))
    );
    let at_alice = est.alice.take_queued().await;
    assert!(
        at_alice.is_none(),
        "alice's session is unchanged (RFC 3261 §14.1), got {:?}",
        at_alice.map(|m| (m.cseq().seq(), m.cseq().method().as_str().to_string()))
    );
    scenario_harness::callflow::hangup(&mut est.dialog, &est.bob).await;
    settle_and_verify(est).await;
}

async fn settle_and_verify(est: &Established) {
    let (store, w_b1, w_b2, call_ref) = (&est.store, &est.w_b1, &est.w_b2, &est.call_ref);
    let drained = est
        .fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(call_ref).await
                && !w_b2.holds_any_trace(call_ref).await
        })
        .await;
    assert!(drained, "the call drains cleanly");
    est.fh.linger_peers(&[&est.alice, &est.bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[w_b1, w_b2], call_ref, store).await;
}

/// The node serving the call counted `acks` ACKs it ignored.
fn ignored(est: &Established, acks: u64) {
    let counted =
        est.w_b1.metrics().unexplained_acks_total() + est.w_b2.metrics().unexplained_acks_total();
    assert_eq!(counted, acks, "the ACK the worker ignored is counted");
}

/// Alice's re-INVITE with `line`, refused by the proxy with `status`, ACKed by
/// her transaction; the final is under the dialog's To-tag.
async fn refused_round(
    est: &mut Established,
    line: Option<(&str, &str)>,
    max_forwards: Option<u32>,
    status: u16,
) -> InDialogTxn {
    let remote_tag = est.dialog.remote_tag().to_string();
    let mut builder = est.dialog.send_request(InDialogMethod::Invite).with_sdp(OFFER);
    if let Some((name, value)) = line {
        builder = builder.with_header(name, value);
    }
    if let Some(hops) = max_forwards {
        builder = builder.max_forwards(hops);
    }
    let mut round = builder.send().await;
    let refused = round.expect(status).await;
    assert_eq!(
        refused.to().tag(),
        Some(remote_tag.as_str()),
        "the proxy's final carries the dialog's To-tag"
    );
    round
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_out_of_hops_leaves_the_call_up() {
    let mut est = establish("reinvite-refused-483").await;
    refused_round(&mut est, None, Some(0), 483).await;
    the_call_stays_up_then_hangs_up(&mut est).await;
    ignored(&est, 0);
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_requiring_an_unknown_extension_leaves_the_call_up() {
    let mut est = establish("reinvite-refused-420").await;
    refused_round(&mut est, Some(("Proxy-Require", "no-such-option")), None, 420).await;
    the_call_stays_up_then_hangs_up(&mut est).await;
    ignored(&est, 0);
}

/// The parse refusal: the proxy's 400 keeps the dialog's tag as its 483 does.
#[tokio::test(start_paused = true)]
async fn a_parse_refused_reinvite_leaves_the_call_up() {
    let mut est = establish("reinvite-refused-400").await;
    refused_round(&mut est, Some(("Expires", "soon")), None, 400).await;
    the_call_stays_up_then_hangs_up(&mut est).await;
    ignored(&est, 0);
}

/// The callee's own re-INVITE takes the same path.
#[tokio::test(start_paused = true)]
async fn the_callees_reinvite_out_of_hops_leaves_the_call_up() {
    let mut est = establish("reinvite-refused-callee-483").await;
    let mut round = est
        .bob_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REANSWER)
        .max_forwards(0)
        .send()
        .await;
    round.expect(483).await;
    the_call_stays_up_then_hangs_up(&mut est).await;
    ignored(&est, 0);
}

/// Control through the proxy: the re-INVITE reaches the worker, the callee
/// refuses it, the worker's server transaction takes her ACK, and a late copy
/// of that ACK past Timer I changes nothing.
#[tokio::test(start_paused = true)]
async fn a_refusal_from_the_callee_and_a_late_ack_copy_leave_the_call_up() {
    let mut est = establish("reinvite-refused-by-callee").await;
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = est.bob.receive("INVITE").await;
    at_bob.respond(488, "Not Acceptable Here").await;
    let refused = round.expect(488).await;
    est.fh.advance(Duration::from_secs(40)).await;
    round.ack_non_2xx(&refused).await.expect("the ACK's copy leaves");
    est.fh.advance(Duration::from_secs(1)).await;
    est.bob.take_queued().await;
    the_call_stays_up_then_hangs_up(&mut est).await;
    ignored(&est, 0);
}

/// On the copy itself: the node holding the call as its backup's copy, which
/// could not tell her ACK from that of a re-INVITE the dead primary relayed and
/// never wrote, never sees it.
#[tokio::test(start_paused = true)]
async fn on_the_copy_a_refused_reinvites_ack_leaves_the_call_up() {
    let mut est = establish("reinvite-refused-on-the-copy").await;
    est.fh.strike(Incident::PrimaryKilled);
    let (primary, backup) = split_mut(&est.primary_ord, &mut est.w_b1, &mut est.w_b2);
    crash_primary(primary, backup, &est.primary_ord, &est.proxy);
    est.fh.advance(Duration::from_millis(300)).await;

    refused_round(&mut est, None, Some(0), 483).await;
    the_call_stays_up_then_hangs_up(&mut est).await;
    ignored(&est, 0);
}

/// On the returned primary, whose reclaimed record holds a write the copy made
/// and may be behind what the copy sent: it never sees her ACK either.
#[tokio::test(start_paused = true)]
async fn on_a_primary_holding_a_copys_write_a_refused_reinvites_ack_leaves_the_call_up() {
    let mut est = establish("reinvite-refused-on-the-returned-primary").await;
    est.fh.strike(Incident::PrimaryKilled);
    let (primary, backup) = split_mut(&est.primary_ord, &mut est.w_b1, &mut est.w_b2);
    crash_primary(primary, backup, &est.primary_ord, &est.proxy);
    est.fh.advance(Duration::from_millis(300)).await;

    let mut info = est.dialog.send_request(InDialogMethod::Info).send().await;
    est.bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    est.fh.advance(Duration::from_millis(500)).await;

    let (primary, backup) = split_mut(&est.primary_ord, &mut est.w_b1, &mut est.w_b2);
    reboot_and_reclaim(&mut est.fh, primary, backup, &est.primary_ord, &est.proxy).await;

    refused_round(&mut est, None, Some(0), 483).await;
    the_call_stays_up_then_hangs_up(&mut est).await;
    ignored(&est, 0);
}
