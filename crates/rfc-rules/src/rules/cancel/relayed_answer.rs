//! A 2xx after a taken CANCEL that the endpoint relays (RFC 3261 §9.2,
//! §16.10): it relayed the INVITE onward, as a proxy or a B2BUA does, then
//! the CANCEL too, or would have but for §9.1 (its far UAS answered before
//! any provisional), and answers with the 2xx that far UAS gave. It carries
//! back the answer the canceller would have met end to end, whatever Call-ID
//! its far side wears; whether that 2xx came after the far UAS took a CANCEL
//! is the far UAS's own occasion, judged there.

use super::keys::TxnKey;
use super::{Answer, Cancel, Reading, Txn};

/// Whether `answer`, the first 2xx `emitter` gave to `txn` after taking
/// `cancel`, relays a far 2xx: `emitter` sent an INVITE onward after taking
/// `txn`'s, and took that INVITE's 2xx no later than `answer` left, having
/// CANCELled it after taking `cancel`, or with no provisional before that 2xx
/// to let a CANCEL leave (§9.1).
pub(super) fn relays_far_answer(
    seen: &Reading<'_>,
    emitter: &str,
    txn: &Txn<'_>,
    cancel: &Cancel<'_>,
    answer: &Answer<'_>,
) -> bool {
    let Some(took_us) = txn.took_invite_us else { return false };
    seen.txns.iter().any(|(&TxnKey { emitter: far, .. }, onward)| {
        let relayed_cancel = || {
            onward
                .cancel
                .as_ref()
                .is_some_and(|relayed| relayed.src == emitter && cancel.ts_us <= relayed.ts_us)
        };
        let cancel_forbidden =
            |at: u64| onward.cancel.is_none() && onward.first_1xx_us.is_none_or(|p| at < p);
        far != emitter
            && onward.invite_src == Some(emitter)
            && onward.took_invite_us.is_some_and(|at| took_us <= at)
            && onward.answers.iter().any(|far_2xx| {
                far_2xx.taker == emitter
                    && far_2xx.ts_us <= answer.ts_us
                    && (relayed_cancel() || cancel_forbidden(far_2xx.ts_us))
            })
    })
}
