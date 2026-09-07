//! **A PRACK across a node kill** (issue 263): the reliable-provisional books
//! are replicated so a PRACK arriving after a takeover still translates — and
//! the session description riding that PRACK still crosses.
//!
//! `crates/call/src/model/record.rs` states that contract on
//! `reliable_provisionals`; `crates/call/tests/codec_roundtrip.rs` proves the
//! bytes survive `MsgpackCodec`. Neither proves a survivor node answering a
//! real PRACK on the wire, which is what these cells do.
//!
//! The shape is the **delayed offer** (RFC 3264 §4): alice's INVITE carries no
//! body, bob's reliable 183 carries the OFFER, and alice's PRACK carries the
//! ANSWER. So the cell proves both halves at once — the survivor translates
//! `(a_tag, a_rseq) → (b_leg, b_rseq)` from hydrated books, AND relays the
//! answer body onward to the callee.
//!
//! ```text
//!   INVITE(no SDP) → 183(Require:100rel, RSeq, offer) → ✗ primary killed ✗
//!                  → PRACK(RAck, answer) → survivor hydrates → PRACK@bob
//!                  → 200(PRACK) → 200(INVITE) → ACK → BYE → 200(BYE)
//! ```
//!
//! The negative half is its sibling: a post-takeover PRACK naming a number this
//! stack never showed must still draw 481 — hydrated books refuse as well as
//! they admit.
//!
//! The `a_cseq: None` hydration branch is NOT reachable here: both nodes run the
//! same build, so both record it. Its proof stays the two codec round-trips.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use call::{CallBodyCodec, MsgpackCodec, ReliableProvisional};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, PartitionRole,
    ProxySut, ReplicatedB2buaSut, WorkerHealth,
};
use sip_message::generators::InDialogMethod;
use sip_message::header::{RAck, RSeq};
use sip_message::types::SipResponse;
use sip_message::Method;
use sip_net::PreIngressAction;

/// Bob's offer, carried on the reliable 183 (delayed-offer INVITE).
const OFFER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
/// Alice's answer, carried on the PRACK — the body that must cross the kill.
const ANSWER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// The `RSeq` bob states on his reliable 183 — the responder-side number the
/// survivor must translate the caller's `RAck` back onto.
const BOB_RSEQ: u32 = 1;

fn rseq_of(resp: &SipResponse) -> u32 {
    resp.header::<RSeq>().expect("an RSeq").expect("readable RSeq").value()
}

/// The replicated `ReliableProvisional` entries the backup holds for `call_ref`.
async fn replicated_provisionals(
    backup: &ReplicatedB2buaSut,
    primary: &str,
    call_ref: &str,
) -> Vec<ReliableProvisional> {
    let body = backup
        .get(PartitionRole::Backup, primary, call_ref)
        .await
        .expect("the backup holds a replica body for the early call");
    MsgpackCodec::new()
        .decode(&body)
        .expect("replica body decodes")
        .reliable_provisionals
}

/// Reboot the crashed primary EMPTY at a higher gen + new pod IP, re-learn its
/// address, drive it ready and let the go-active reclaim run — so the call's CDR
/// authority is back to discharge the terminal the survivor deferred.
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    primary_ord: &str,
    proxy: &ProxySut,
) {
    fh.mark(primary_ord, None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = primary.reboot().await;
    proxy.set_address(primary_ord, new_addr);
    fh.note_worker_rebound(primary_ord, new_addr);
    survivor.simulate_peer_added(primary_ord);
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary {primary_ord} became ready");
    proxy.set_health(primary_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(10)).await;
}

/// A delayed-offer PRACK that lands on the SURVIVOR after the primary is killed
/// translates onto the callee's own `RSeq` **and carries the answer onward**;
/// the call then answers, ACKs, and tears down with exactly one CDR and no
/// residue anywhere in the cluster.
#[tokio::test(start_paused = true)]
async fn prack_after_takeover_translates_the_rack_and_relays_the_answer() {
    let mut fh = FailoverHarness::new("prack-takeover-delayed-offer", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── STEP 1: delayed-offer INVITE, answered RELIABLY with the offer ───────
    let mut call = alice
        .invite(&bob)
        .with_header("Supported", "100rel")
        .through(proxy.addr())
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    assert!(uas.request().body().is_empty(), "delayed offer: no SDP on the INVITE");

    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let (primary, survivor): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(OFFER)
        .await;
    let p183 = call.expect(183).await;
    let a_rseq = rseq_of(&p183);
    let a_cseq = p183.cseq().seq();
    let a_tag = p183.to().tag().expect("the a-facing early dialog tag").to_string();
    assert_eq!(p183.body(), OFFER.as_bytes(), "the offer reaches alice on the 183");

    // ── STEP 2: the books are REPLICATED before the kill ─────────────────────
    // The cell must test the reachable case: an entry that never replicated
    // leaves the survivor nothing to translate, and that case has no fix.
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = survivor
        .scan_one_backed_up(&pri_ord)
        .await
        .expect("the early call replicated to the backup");
    let entries = replicated_provisionals(survivor, &pri_ord, &call_ref).await;
    assert_eq!(
        entries.len(),
        1,
        "the backup must hold the reliable provisional's mapping at the moment of \
         the kill; got {entries:?}",
    );
    let entry = &entries[0];
    assert_eq!(entry.a_tag, a_tag, "the replicated entry names the dialog the number was shown in");
    assert_eq!(entry.a_rseq, a_rseq as i64, "the replicated entry carries the a-facing RSeq");
    assert_eq!(entry.b_rseq, BOB_RSEQ as i64, "the replicated entry carries bob's own RSeq");
    assert_eq!(
        entry.a_cseq,
        Some(a_cseq as i64),
        "the replicated entry carries the a-facing INVITE CSeq the RAck names",
    );

    // ── STEP 3: kill the primary ─────────────────────────────────────────────
    fh.mark(&pri_ord, None, "crash", "primary down mid-early-dialog");
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;
    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();


    // The §3 ladder repeats the still-un-PRACKed provisional at T1 (500 ms), so
    // the replication window above let exactly one rung fire. Absorb it here —
    // a caller ignores the duplicate — so the PRACK transaction below reads its
    // own response and not a stale provisional.
    let rung = call.expect(183).await;
    assert_eq!(rseq_of(&rung), a_rseq, "the §3 ladder repeats the SAME number it showed");

    // ── STEP 4: alice PRACKs the number she was shown, carrying the ANSWER ───
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&a_tag)
        .with_rack(&format!("{a_rseq} {a_cseq} INVITE"))
        .with_sdp(ANSWER)
        .send()
        .await;

    let mut prack_at_bob = bob.receive("PRACK").await;
    let relayed = prack_at_bob
        .request()
        .header::<RAck>()
        .expect("the relayed PRACK keeps an RAck")
        .expect("readable RAck");
    assert_eq!(
        relayed.rseq(),
        BOB_RSEQ,
        "the survivor translated the caller's RSeq back onto bob's own from hydrated books",
    );
    assert_eq!(relayed.method(), &Method::Invite, "the RAck acknowledges the INVITE transaction");
    assert_eq!(
        prack_at_bob.request().body(),
        ANSWER.as_bytes(),
        "the answer riding the PRACK crosses the takeover to the callee (RFC 3264 §4)",
    );

    prack_at_bob.respond(200, "OK").await;
    prack.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert!(
        survivor.metrics().repl_takeover_hydrated_total() > hydrated_before,
        "the PRACK hydrated the call onto the survivor (takeover fired)",
    );

    // ── STEP 5: the call answers on the survivor and tears down cleanly ──────
    // The offer/answer completed on the PRACK, so the 200 and the ACK are
    // bodyless (RFC 3264 §4).
    uas.respond(200, "OK").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(300)).await;

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    // ── STEP 6: the primary returns and discharges ───────────────────────────
    // The killed primary is the call's sole CDR authority (ADR-0020 X3): the
    // survivor defers the terminal, and the CDR only exists once the primary
    // reboots inside its budget and reclaims — the `c6` shape.
    fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;

    assert_eq!(
        total_cdrs_for(&[&w_b1, &w_b2], &call_ref),
        1,
        "exactly one CDR for the failed-over PRACK call across the cluster",
    );
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The refusing half: after the takeover, a PRACK naming an `RSeq` this stack
/// never showed draws 481 from the survivor — the hydrated books refuse as well
/// as they admit — and nothing reaches the callee.
#[tokio::test(start_paused = true)]
async fn prack_naming_an_unshown_rseq_after_takeover_draws_481() {
    let mut fh = FailoverHarness::new("prack-takeover-unshown-rseq", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    let mut call = alice
        .invite(&bob)
        .with_header("Supported", "100rel")
        .through(proxy.addr())
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let (primary, survivor): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(OFFER)
        .await;
    let p183 = call.expect(183).await;
    let a_rseq = rseq_of(&p183);
    let a_cseq = p183.cseq().seq();
    let a_tag = p183.to().tag().expect("the a-facing early dialog tag").to_string();

    fh.advance(Duration::from_millis(500)).await;
    let call_ref = survivor
        .scan_one_backed_up(&pri_ord)
        .await
        .expect("the early call replicated to the backup");

    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;

    // The §3 ladder repeats the still-un-PRACKed provisional at T1 (500 ms), so
    // the replication window above let exactly one rung fire. Absorb it here —
    // a caller ignores the duplicate — so the PRACK transaction below reads its
    // own response and not a stale provisional.
    let rung = call.expect(183).await;
    assert_eq!(rseq_of(&rung), a_rseq, "the §3 ladder repeats the SAME number it showed");

    // An RSeq one past the one that was actually shown: no book, anywhere, maps it.
    let mut bogus = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&a_tag)
        .with_rack(&format!("{} {a_cseq} INVITE", a_rseq + 1))
        .with_sdp(ANSWER)
        .send()
        .await;
    assert!(
        bob.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "an untranslatable PRACK must never reach the callee",
    );
    let refused = bogus.expect(481).await;
    assert_eq!(refused.status(), 481, "the survivor refuses a number it never showed");

    // The refusal costs the call nothing: alice PRACKs the number she WAS shown,
    // it translates on the same hydrated books, and only then may bob answer —
    // RFC 3262 §3 forbids a 2xx while a reliable 1xx carrying SDP is unacked.
    // Hydrating re-armed the §3 ladder on the survivor, so one more rung is in
    // flight behind the 481; absorb it before the good PRACK's own transaction.
    let rung = call.expect(183).await;
    assert_eq!(rseq_of(&rung), a_rseq, "the re-armed ladder repeats the SAME number");

    let mut good = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&a_tag)
        .with_rack(&format!("{a_rseq} {a_cseq} INVITE"))
        .with_sdp(ANSWER)
        .send()
        .await;
    let mut good_at_bob = bob.receive("PRACK").await;
    good_at_bob.respond(200, "OK").await;
    good.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;

    uas.respond(200, "OK").await;
    // Strict: the PRACK retired the provisional, so the §3 ladder has ceased and
    // the answer is the very next thing the caller sees. A rung here would be a
    // ladder that outlived its acknowledgement.
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(300)).await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(
        total_cdrs_for(&[&w_b1, &w_b2], &call_ref),
        1,
        "the refused PRACK cost the call nothing: still exactly one CDR",
    );
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// A NON-2xx final on the b-leg INVITE — a client transaction the DEAD primary
/// originated, so one the survivor's own transaction layer never held — is
/// ACKed hop-by-hop toward the callee (RFC 3261 §17.1.1.3) from the leg's
/// replicated INVITE handle, and relayed to the caller. Neither peer is left on
/// a timer: no Timer H ladder at the callee, no Timer B wedge at the caller.
///
/// The PRACK here is the CORRECT one: the takeover, not the refusal in the
/// sibling cell, is what this cell isolates.
#[tokio::test(start_paused = true)]
async fn non_2xx_b_leg_final_after_a_takeover_reaches_the_caller_and_acks_the_callee() {
    let mut fh = FailoverHarness::new("prack-takeover-non-2xx-final", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    let mut call = alice
        .invite(&bob)
        .with_header("Supported", "100rel")
        .through(proxy.addr())
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    // The b-leg INVITE as the CALLEE sees it — the transaction his ACK must name.
    let b_branch = uas.request().top_via().branch().map(str::to_owned);
    let b_cseq = uas.request().cseq().seq();
    let (primary, survivor): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(OFFER)
        .await;
    let p183 = call.expect(183).await;
    let a_rseq = rseq_of(&p183);
    let a_cseq = p183.cseq().seq();
    let a_tag = p183.to().tag().expect("the a-facing early dialog tag").to_string();

    fh.advance(Duration::from_millis(500)).await;
    let call_ref = survivor
        .scan_one_backed_up(&pri_ord)
        .await
        .expect("the early call replicated to the backup");

    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;

    let rung = call.expect(183).await;
    assert_eq!(rseq_of(&rung), a_rseq, "the §3 ladder repeats the SAME number it showed");

    // A CORRECT PRACK, so the takeover hydrates exactly as the first cell's does.
    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&a_tag)
        .with_rack(&format!("{a_rseq} {a_cseq} INVITE"))
        .with_sdp(ANSWER)
        .send()
        .await;
    let mut prack_at_bob = bob.receive("PRACK").await;
    // Bob's own tag on the b-leg dialog — the one his ACK must be addressed under.
    let b_tag = prack_at_bob.request().to().tag().map(str::to_owned);
    prack_at_bob.respond(200, "OK").await;
    prack.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert!(
        survivor.metrics().repl_takeover_hydrated_total() > hydrated_before,
        "the call is served by the survivor when the reject arrives (takeover fired)",
    );

    // Bob now REJECTS the INVITE the survivor is relaying for. The PRACK retired
    // the provisional, so the reject is the next thing the caller sees.
    uas.respond(486, "Busy Here").await;
    let rejected = call.expect(486).await;
    assert_eq!(rejected.status(), 486, "the callee's reject must reach the caller");

    // Alice's own §17.1.1.3 ACK rides `expect`; the survivor owes bob his, on the
    // INVITE's own branch and CSeq, addressed to the tag the reject carried.
    fh.advance(Duration::from_millis(500)).await;
    let ack = bob.receive("ACK").await;
    let acked = ack.request();
    assert_eq!(
        acked.top_via().branch(),
        b_branch.as_deref(),
        "the ACK rides the INVITE's own top-Via branch (§17.1.1.3)",
    );
    assert_eq!(acked.cseq().seq(), b_cseq, "the ACK echoes the INVITE CSeq");
    assert_eq!(acked.cseq().method(), Method::Ack, "and states ACK as its method");
    assert_eq!(
        acked.to().tag(),
        b_tag.as_deref(),
        "the ACK is addressed under the callee's own dialog tag",
    );
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        bob.try_receive_tolerating("ACK", &[]).await.is_none(),
        "exactly one ACK — the txn layer's copy and the takeover copy must not both leave",
    );

    fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(
        total_cdrs_for(&[&w_b1, &w_b2], &call_ref),
        1,
        "the rejected call is billed exactly once, by the reclaiming primary",
    );
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}


/// The SURVIVOR's non-2xx final is answered by an a-leg server transaction it
/// never received the INVITE of: its Timer G re-sends the final when the first
/// copy is lost, and the caller's RFC 3261 §17.1.1.3 ACK is relayed onward to
/// that survivor — the node that sent the final and holds the transaction being
/// acknowledged — not to the node the INVITE was forwarded to before the kill.
/// That ACK quenches the ladder (no third copy), and the call reaps with one CDR.
#[tokio::test(start_paused = true)]
async fn the_callers_ack_for_a_post_takeover_reject_reaches_the_survivor_that_sent_it() {
    let mut fh = FailoverHarness::new("prack-takeover-reject-ack-hop", &["b1", "b2"]);

    // Deterministic loss model on alice's bind: swallow the FIRST 486 copy and
    // count every one, so the tail asserts both the Timer G recovery AND the
    // post-ACK quench (exactly two copies ever reach the bind).
    let copies_at_alice = Arc::new(AtomicUsize::new(0));
    let counter = copies_at_alice.clone();
    let alice = fh
        .agent_with_pre_ingress(
            "alice",
            ALICE,
            Arc::new(move |bytes: &[u8], _src, _depth| {
                if bytes.starts_with(b"SIP/2.0 486")
                    && counter.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return PreIngressAction::Drop;
                }
                PreIngressAction::Accept
            }),
        )
        .await;
    let bob = fh.agent("bob", BOB).await;

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    let mut call = alice
        .invite(&bob)
        .with_header("Supported", "100rel")
        .through(proxy.addr())
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    // The b-leg INVITE as the CALLEE sees it — the transaction his ACK must name.
    let b_branch = uas.request().top_via().branch().map(str::to_owned);
    let b_cseq = uas.request().cseq().seq();
    let (primary, survivor): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(OFFER)
        .await;
    let p183 = call.expect(183).await;
    let a_rseq = rseq_of(&p183);
    let a_cseq = p183.cseq().seq();
    let a_tag = p183.to().tag().expect("the a-facing early dialog tag").to_string();

    fh.advance(Duration::from_millis(500)).await;
    let call_ref = survivor
        .scan_one_backed_up(&pri_ord)
        .await
        .expect("the early call replicated to the backup");

    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;

    let rung = call.expect(183).await;
    assert_eq!(rseq_of(&rung), a_rseq, "the §3 ladder repeats the SAME number it showed");

    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&a_tag)
        .with_rack(&format!("{a_rseq} {a_cseq} INVITE"))
        .with_sdp(ANSWER)
        .send()
        .await;
    let mut prack_at_bob = bob.receive("PRACK").await;
    // Bob's own tag on the b-leg dialog — the one his ACK must be addressed under.
    let b_tag = prack_at_bob.request().to().tag().map(str::to_owned);
    prack_at_bob.respond(200, "OK").await;
    prack.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert!(
        survivor.metrics().repl_takeover_hydrated_total() > hydrated_before,
        "the call is served by the survivor when the reject arrives (takeover fired)",
    );

    // Bob REJECTS; the survivor relays the final to the caller. Her first copy
    // is swallowed by the loss hook, so only the survivor's own Timer G (~500 ms
    // after the final, plus the hops' transit) can put the reject in front of
    // her again — and the ladder's next rung (~1.5 s) is still ahead.
    uas.respond(486, "Busy Here").await;
    fh.advance(Duration::from_millis(1_000)).await;
    assert_eq!(
        copies_at_alice.load(Ordering::SeqCst),
        2,
        "the dropped first copy + the survivor's Timer G retransmit at alice's bind",
    );
    let rejected = call.expect(486).await;
    assert_eq!(rejected.status(), 486, "the callee's reject reaches the caller despite the loss");
    assert_eq!(
        survivor.server_final_retransmits(),
        1,
        "the retransmit is the survivor's a-leg server transaction speaking",
    );

    // The survivor still owes bob his own b-leg ACK, on the INVITE's branch and
    // CSeq, addressed under the tag the reject carried.
    fh.advance(Duration::from_millis(500)).await;
    let ack = bob.receive("ACK").await;
    let acked = ack.request();
    assert_eq!(
        acked.top_via().branch(),
        b_branch.as_deref(),
        "the ACK rides the INVITE's own top-Via branch (§17.1.1.3)",
    );
    assert_eq!(acked.cseq().seq(), b_cseq, "the ACK echoes the INVITE CSeq");
    assert_eq!(acked.cseq().method(), Method::Ack, "and states ACK as its method");
    assert_eq!(
        acked.to().tag(),
        b_tag.as_deref(),
        "the ACK is addressed under the callee's own dialog tag",
    );

    // ── The hop the caller's ACK takes out of the proxy ──────────────────────
    let proxy_addr: SocketAddr = PROXY.parse().expect("proxy sip addr");
    let bob_addr: SocketAddr = BOB.parse().expect("bob sip addr");
    let survivor_addr: SocketAddr =
        if pri_ord == "b1" { B2 } else { B1 }.parse().expect("survivor sip addr");
    let a_leg_acks: Vec<_> = fh
        .sip_entries()
        .into_iter()
        .filter(|e| e.from == proxy_addr && e.to != bob_addr && e.raw.starts_with(b"ACK "))
        .collect();
    assert_eq!(
        a_leg_acks.len(),
        1,
        "the caller's ACK is relayed toward a worker exactly once; got {:?}",
        a_leg_acks.iter().map(|e| e.to).collect::<Vec<_>>(),
    );
    assert_eq!(
        a_leg_acks[0].to, survivor_addr,
        "the ACK for a relayed reject follows the final's sender — the survivor — not the \
         target the INVITE was forwarded to (RFC 3261 §17.1.1.3)",
    );
    assert!(a_leg_acks[0].delivered, "and it reaches that node's bind");

    // Advance PAST the ladder's next rung (~1.5 s after the final): a third copy
    // at alice's bind means the relayed ACK failed to quench the survivor's Timer G.
    fh.advance(Duration::from_millis(1_700)).await;
    assert_eq!(
        copies_at_alice.load(Ordering::SeqCst),
        2,
        "no further final reaches the caller once her ACK has been relayed to the node that \
         sent it",
    );
    assert!(
        bob.try_receive_tolerating("ACK", &[]).await.is_none(),
        "exactly one ACK — the txn layer's copy and the takeover copy must not both leave",
    );

    fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(
        total_cdrs_for(&[&w_b1, &w_b2], &call_ref),
        1,
        "the rejected call is billed exactly once, by the reclaiming primary",
    );
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The ACK a takeover node composes for a b-leg transaction it never held is
/// protected against loss exactly as the transaction layer protects its own
/// (RFC 3261 §17.1.1.3, Timer D): when the first copy is swallowed on the
/// callee's bind, the callee's retransmitted final draws the SAME ACK again —
/// same branch, same CSeq, same dialog tag — so his server transaction
/// completes instead of laddering to Timer H, and the retransmit is absorbed by
/// the survivor's transaction layer rather than re-serving the released call.
#[tokio::test(start_paused = true)]
async fn a_lost_takeover_ack_is_resent_when_the_callee_retransmits_its_final() {
    let mut fh = FailoverHarness::new("prack-takeover-ack-loss", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;

    // Deterministic loss model on BOB's bind: swallow the FIRST ACK datagram
    // and count every one. The only ACKs that ever reach this bind are the
    // survivor's composed copies — the caller's own ACK rides the a-leg back to
    // the node that sent the final and stops there.
    let acks_at_bob = Arc::new(AtomicUsize::new(0));
    let counter = acks_at_bob.clone();
    let bob = fh
        .agent_with_pre_ingress(
            "bob",
            BOB,
            Arc::new(move |bytes: &[u8], _src, _depth| {
                if bytes.starts_with(b"ACK ") && counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    return PreIngressAction::Drop;
                }
                PreIngressAction::Accept
            }),
        )
        .await;

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    let mut call = alice
        .invite(&bob)
        .with_header("Supported", "100rel")
        .through(proxy.addr())
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    // The b-leg INVITE as the CALLEE sees it — the transaction his ACK must name.
    let b_branch = uas.request().top_via().branch().map(str::to_owned);
    let b_cseq = uas.request().cseq().seq();
    let (primary, survivor): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(OFFER)
        .await;
    let p183 = call.expect(183).await;
    let a_rseq = rseq_of(&p183);
    let a_cseq = p183.cseq().seq();
    let a_tag = p183.to().tag().expect("the a-facing early dialog tag").to_string();

    fh.advance(Duration::from_millis(500)).await;
    let call_ref = survivor
        .scan_one_backed_up(&pri_ord)
        .await
        .expect("the early call replicated to the backup");

    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;

    let rung = call.expect(183).await;
    assert_eq!(rseq_of(&rung), a_rseq, "the §3 ladder repeats the SAME number it showed");

    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&a_tag)
        .with_rack(&format!("{a_rseq} {a_cseq} INVITE"))
        .with_sdp(ANSWER)
        .send()
        .await;
    let mut prack_at_bob = bob.receive("PRACK").await;
    // Bob's own tag on the b-leg dialog — the one his ACK must be addressed under.
    let b_tag = prack_at_bob.request().to().tag().map(str::to_owned);
    prack_at_bob.respond(200, "OK").await;
    prack.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert!(
        survivor.metrics().repl_takeover_hydrated_total() > hydrated_before,
        "the call is served by the survivor when the reject arrives (takeover fired)",
    );

    // ── Bob REJECTS; the survivor composes his ACK and the bind swallows it ──
    uas.respond(486, "Busy Here").await;
    let rejected = call.expect(486).await;
    assert_eq!(rejected.status(), 486, "the callee's reject must reach the caller");

    fh.advance(Duration::from_millis(100)).await;
    assert_eq!(
        acks_at_bob.load(Ordering::SeqCst),
        1,
        "the takeover-composed ACK reached bob's bind and was swallowed there",
    );
    assert!(
        bob.try_receive_tolerating("ACK", &[]).await.is_none(),
        "the swallowed ACK never reaches bob's transaction layer",
    );
    let hydrated_at_reject = survivor.metrics().repl_takeover_hydrated_total();

    // ── Bob's Timer G rung (RFC 3261 §17.2.1): the final, again, unacked ─────
    fh.advance(Duration::from_millis(500)).await;
    uas.respond(486, "Busy Here").await;
    fh.advance(Duration::from_millis(500)).await;

    assert_eq!(
        acks_at_bob.load(Ordering::SeqCst),
        2,
        "the callee's retransmitted final must draw a second ACK from the survivor \
         (RFC 3261 §17.1.1.3 / Timer D re-ACK)",
    );
    let ack = bob.receive("ACK").await;
    let acked = ack.request();
    assert_eq!(
        acked.top_via().branch(),
        b_branch.as_deref(),
        "the re-ACK rides the INVITE's own top-Via branch, so the callee's server \
         transaction completes on it (§17.1.1.3)",
    );
    assert_eq!(acked.cseq().seq(), b_cseq, "the re-ACK echoes the INVITE CSeq");
    assert_eq!(acked.cseq().method(), Method::Ack, "and states ACK as its method");
    assert_eq!(
        acked.to().tag(),
        b_tag.as_deref(),
        "the re-ACK is addressed under the callee's own dialog tag",
    );
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_at_reject,
        "the retransmitted final is absorbed by the survivor's transaction layer, never \
         re-served by re-hydrating the released copy",
    );

    // Past where a further Timer G rung would land: the callee's transaction is
    // complete, so he sends no third final and the survivor owes no third ACK.
    fh.advance(Duration::from_millis(1_500)).await;
    assert_eq!(
        acks_at_bob.load(Ordering::SeqCst),
        2,
        "exactly two ACKs ever leave: the swallowed one and the re-ACK",
    );
    assert!(
        bob.try_receive_tolerating("ACK", &[]).await.is_none(),
        "no further ACK once the callee's server transaction has completed",
    );

    fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(
        total_cdrs_for(&[&w_b1, &w_b2], &call_ref),
        1,
        "the rejected call is billed exactly once, by the reclaiming primary",
    );
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
