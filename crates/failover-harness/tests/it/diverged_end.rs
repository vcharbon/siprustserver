//! How the parties live through the end of a call whose two copies diverged
//! (ADR-0055 §7): each node that meets the other's copy ends the call cleanly
//! from the union of both, so a party may draw a BYE from each node. The
//! first BYE on a dialog ends it and takes `200`; a later one finds no dialog
//! and takes `481` (RFC 3261 §12.2.2, §15.1.2), the declared window of
//! ADR-0055 §11 for a split that heals. Every other request reaching a party
//! once its dialog ended takes `481`; a request the cluster sends in a dialog
//! still up takes `200`. A copy of a re-INVITE's 2xx reaching the caller is
//! ACKed again (RFC 3261 §13.2.2.4).

use std::collections::HashSet;
use std::time::Duration;

use failover_harness::{assert_call_fully_over, RULE_CSEQ_IN_DIALOG_ORDER};
use scenario_harness::{Agent, Inbound};

use crate::call_terminate_on_backup::Established;

/// How long the cluster has to end the call and forget it.
const END_WITHIN: Duration = Duration::from_secs(40);

/// The clock step between two looks at the parties.
const STEP: Duration = Duration::from_millis(100);

/// Accept, from now, what the clean end of two diverged copies may send the
/// parties: a BYE into a dialog the other node's BYE already ended, and both
/// nodes' BYEs at one CSeq (ADR-0055 §8, §11).
pub(crate) fn accept_the_clean_end(est: &mut Established) {
    let names = [est.alice.name().to_string(), est.bob.name().to_string()];
    est.fh.accept_clean_end_byes(
        &names,
        "ADR-0055 §11: the clean end's BYE into a dialog the other owner ended, closed by \
         the 481 it draws",
    );
    est.fh.accept_rfc_deviations_from_now(
        RULE_CSEQ_IN_DIALOG_ORDER,
        "ADR-0055 §8: two copies of a split call mint one in-dialog CSeq, each node's clean \
         end BYEing at the next CSeq of its own union",
    );
}

/// What the clean end sent a party: the BYEs it took, and the CSeq of the
/// first, which ended its dialog.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Ended {
    pub(crate) byes: usize,
    pub(crate) first_bye_cseq: Option<u32>,
}

/// The parties answer what the clean end sends them until no node holds the
/// call; the call is then fully over, with one CDR. Returns what each party
/// took here, `(caller, callee)`.
pub(crate) async fn answer_the_clean_end(est: &mut Established) -> (Ended, Ended) {
    let mut ended = (HashSet::new(), HashSet::new());
    let mut took = (Ended::default(), Ended::default());
    let mut elapsed = Duration::ZERO;
    loop {
        let repeats = answer(&est.alice, &mut ended.0, &mut took.0).await;
        for _ in 0..repeats {
            est.dialog.ack(None).await;
        }
        answer(&est.bob, &mut ended.1, &mut took.1).await;
        if !held_anywhere(est).await || elapsed >= END_WITHIN {
            break;
        }
        est.fh.advance(STEP).await;
        elapsed += STEP;
    }
    est.fh.linger_peers(&[&est.alice, &est.bob], Duration::from_secs(3)).await;
    answer(&est.alice, &mut ended.0, &mut took.0).await;
    answer(&est.bob, &mut ended.1, &mut took.1).await;
    let nodes = [&est.w_b1, &est.w_b2];
    assert_call_fully_over(&nodes, &est.call_ref, &est.store).await;
    took
}

/// Answer every request queued at `party`, counting its BYEs into `took`;
/// `ended` holds the Call-IDs whose dialog a BYE already ended there.
/// Returns the copies of an INVITE's 2xx it read in a dialog still up.
async fn answer(party: &Agent, ended: &mut HashSet<String>, took: &mut Ended) -> usize {
    let mut repeats = 0;
    while let Some(inbound) = party.take_queued_inbound().await {
        let mut txn = match inbound {
            Inbound::Request(txn) => txn,
            Inbound::Response(r) => {
                let repeat = r.status() / 100 == 2
                    && r.cseq().method().as_str() == "INVITE"
                    && !ended.contains(&r.call_id().to_string());
                repeats += usize::from(repeat);
                continue;
            }
        };
        let request = txn.request();
        let call_id = request.call_id().to_string();
        let cseq = request.cseq().seq();
        match request.method().as_str() {
            "ACK" => {}
            "BYE" if ended.insert(call_id.clone()) => {
                took.byes += 1;
                took.first_bye_cseq.get_or_insert(cseq);
                txn.respond(200, "OK").await;
            }
            "BYE" => {
                took.byes += 1;
                txn.respond(481, "Call/Transaction Does Not Exist").await;
            }
            _ if ended.contains(&call_id) => {
                txn.respond(481, "Call/Transaction Does Not Exist").await;
            }
            _ => txn.respond(200, "OK").await,
        }
    }
    repeats
}

/// Whether a node still holds anything of the call.
async fn held_anywhere(est: &Established) -> bool {
    let call_ref = &est.call_ref;
    est.store.stats().current_total != 0
        || !est.w_b1.memory_clean()
        || !est.w_b2.memory_clean()
        || est.w_b1.holds_any_trace(call_ref).await
        || est.w_b2.holds_any_trace(call_ref).await
}
