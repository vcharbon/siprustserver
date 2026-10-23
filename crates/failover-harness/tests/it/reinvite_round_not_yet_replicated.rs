//! **A re-INVITE round answered after the serving node died, before the round
//! reached the backup.** The caller heard the serving node's relayed 100, so it
//! is `Proceeding` and sends nothing more (RFC 3261 §17.1.1.2); the departure,
//! announced, takes nothing over, as the replica holds no INVITE in flight. The
//! callee's final fails over to the survivor on the cookie of the proxy's Via
//! and takes the call over, but the replica predates the round: no transaction
//! there takes the final. Its CSeq, above the last the dialog's record sent and
//! with no relay pending for it, names a round the record lost. The survivor
//! acknowledges it — a 2xx end to end (§13.2.2.4), a non-2xx hop by hop
//! (§17.1.1.3) — raises the dialog's sequence past it, so its BYE never reuses
//! a number the callee saw (§12.2.1.1), and ends the call with a BYE on both
//! legs. The caller's pending re-INVITE cannot get a final: its transaction
//! existed only on the dead node (§17.1.3); the BYE ends its dialog. Every
//! retransmission of the callee's 2xx (§13.3.1.4) finds the call ended and
//! takes nothing over again: one takeover per lost round.

use std::time::Duration;

use failover_harness::{FailoverHarness, PartitionRole, ProxySut, ReplicatedB2buaSut};
use scenario_harness::agent::{Agent, Dialog, ServerTxn};

use crate::departure_support::kill_announced;
use crate::inflight_resend::{
    bring_up, establish, reboot_and_reclaim, settle_to_one_cdr, split_mut, survivor_of,
    synchronized_call_ref, REANSWER, REOFFER, REPLICATE, T1,
};

/// RFC 3261 §17.1.2.2's T2: where the §13.3.1.4 2xx ladder stops doubling.
const T2: Duration = Duration::from_secs(4);

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// An established call whose re-INVITE round reached the callee, its 100 heard
/// by the caller, and whose node then died with the round in no replica.
struct LostRound {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
    dialog: Dialog,
    at_bob: ServerTxn,
    pri_ord: String,
    call_ref: String,
    hydrated_before: u64,
}

async fn lose_a_round(name: &str) -> LostRound {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;
    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;
    let before_round = survivor_of(&pri_ord, &w_b1, &w_b2)
        .get(PartitionRole::Backup, &pri_ord, &call_ref)
        .await
        .expect("the established call's replica");

    let mut reinvite = dialog.reinvite(Some(REOFFER)).await;
    reinvite.expect(100).await;
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(100, "Trying").await;
    fh.advance(REPLICATE).await;
    // The round's replica never landed: the backup holds the call as it was
    // before the round when the node dies.
    survivor_of(&pri_ord, &w_b1, &w_b2)
        .rewind_replica(PartitionRole::Backup, &pri_ord, &call_ref, before_round)
        .await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before,
        "a call with nothing in flight in its replica is not taken over on the departure",
    );
    drop(reinvite);
    LostRound {
        fh,
        alice,
        bob,
        proxy,
        w_b1,
        w_b2,
        dialog,
        at_bob,
        pri_ord,
        call_ref,
        hydrated_before,
    }
}

impl LostRound {
    fn survivor(&self) -> &ReplicatedB2buaSut {
        survivor_of(&self.pri_ord, &self.w_b1, &self.w_b2)
    }

    /// The callee's final, `what`, took the call over, once.
    fn taken_over_once(&self, what: &str) {
        let hydrated = self.survivor().metrics().repl_takeover_hydrated_total();
        assert_eq!(hydrated, self.hydrated_before + 1, "{what} took the call over once");
    }

    /// The CSeq number of the round the callee answers.
    fn round_cseq(&self) -> u32 {
        self.at_bob.request().cseq().seq()
    }

    /// The survivor ends the call: the callee's BYE, numbered past the lost
    /// round, and the caller's, both answered 200.
    async fn ended_on_both_legs(&mut self) {
        let mut bye = self.bob.receive("BYE").await;
        assert!(
            bye.request().cseq().seq() > self.round_cseq(),
            "the BYE is numbered past the round the callee saw (§12.2.1.1)",
        );
        bye.respond(200, "OK").await;
        self.alice.receive("BYE").await.respond(200, "OK").await;
        self.fh.advance(Duration::from_millis(500)).await;
    }

    /// The node returns; the ended call has one CDR and is fully released.
    async fn settle(mut self) {
        self.fh.advance(Duration::from_secs(60)).await;
        let (primary, survivor) = split_mut(&self.pri_ord, &mut self.w_b1, &mut self.w_b2);
        reboot_and_reclaim(&mut self.fh, primary, survivor, &self.pri_ord, &self.proxy).await;
        let nodes = [&self.w_b1, &self.w_b2];
        settle_to_one_cdr(&self.fh, &[&self.alice, &self.bob], &nodes, &self.call_ref).await;
        drop(self.proxy);
    }
}

#[tokio::test(start_paused = true)]
async fn a_round_answered_2xx_before_it_replicated_is_acknowledged_and_the_call_ended() {
    let mut lost = lose_a_round("reinvite-round-not-yet-replicated").await;

    lost.at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    let (answer, toward) = lost
        .fh
        .sip_entries()
        .into_iter()
        .rfind(|e| e.from == lost.bob.addr() && e.raw.starts_with(b"SIP/2.0 200"))
        .map(|e| (e.raw, e.to))
        .expect("the callee's 200 is on the wire");
    let ack = lost.bob.receive("ACK").await;
    assert_eq!(
        ack.request().cseq().seq(),
        lost.round_cseq(),
        "the ACK names the round (§13.2.2.4)"
    );
    lost.taken_over_once("the callee's 200");
    lost.ended_on_both_legs().await;

    // The callee's §13.3.1.4 ladder, as if the ACK were lost: the call is ended.
    let mut gap = T1;
    let mut elapsed = Duration::ZERO;
    while elapsed + gap < 64 * T1 {
        lost.fh.advance(gap).await;
        elapsed += gap;
        lost.bob.try_send_datagram(&answer, toward).await.expect("the retransmitted 200 leaves");
        gap = (gap * 2).min(T2);
    }
    lost.fh.advance(Duration::from_millis(200)).await;
    lost.taken_over_once("the callee's 200, its ladder and the answers to the BYEs");
    assert!(!lost.survivor().serves(&lost.call_ref), "the copy released itself");
    let _ = &lost.dialog;
    lost.settle().await;
}

#[tokio::test(start_paused = true)]
async fn a_round_refused_before_it_replicated_is_acknowledged_and_the_call_ended() {
    let mut lost = lose_a_round("reinvite-round-refused-not-yet-replicated").await;

    lost.at_bob.respond(488, "Not Acceptable Here").await;
    let ack = lost.bob.receive("ACK").await;
    assert_eq!(ack.request().cseq().seq(), lost.round_cseq(), "the ACK names the round");
    assert_eq!(
        ack.request().top_via().branch(),
        lost.at_bob.request().top_via().branch(),
        "a non-2xx is acknowledged on the INVITE's own transaction (§17.1.1.3)",
    );
    lost.taken_over_once("the callee's 488");
    lost.ended_on_both_legs().await;
    lost.taken_over_once("the callee's 488 and the answers to the BYEs");
    let _ = &lost.dialog;
    lost.settle().await;
}
