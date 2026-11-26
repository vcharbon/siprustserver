//! A 2xx after a taken CANCEL that the endpoint relays (RFC 3261 §9.2,
//! §16.10): it relayed the INVITE onward, as a proxy or a B2BUA does, then
//! the CANCEL too, or had none to send because the far 2xx beat it there, and
//! answers with the 2xx that far UAS gave, its description carried byte for
//! byte. It carries back the answer the canceller would have met end to end,
//! whatever Call-ID its far side wears; whether that 2xx came after the far
//! UAS took a CANCEL is the far UAS's own occasion, judged there. A relay that
//! restates the description is not told apart from one that answers on its
//! own account, and is charged.

use super::super::T1_64_US;

use super::keys::TxnKey;
use super::{Answer, Cancel, Reading, Txn};

/// Whether `answer`, the first 2xx `emitter` gave to `txn` after taking
/// `cancel`, relays a far 2xx: `emitter` sent an INVITE onward after taking
/// `txn`'s and took that INVITE's 2xx no later than `answer` left, carrying
/// `answer`'s description; and it CANCELled that INVITE after taking
/// `cancel`, or sent no CANCEL there and took that 2xx within 64·T1 of
/// `cancel`, the canceller's own horizon (§9.1).
pub(super) fn relays_far_answer(
    seen: &Reading<'_>,
    emitter: &str,
    txn: &Txn<'_>,
    cancel: &Cancel<'_>,
    answer: &Answer<'_>,
) -> bool {
    let (Some(took_us), Some(body)) = (txn.took_invite_us, answer.body.filter(|b| !b.is_empty()))
    else {
        return false;
    };
    seen.txns.iter().any(|(&TxnKey { emitter: far, .. }, onward)| {
        let relayed_cancel = onward
            .cancel
            .as_ref()
            .is_some_and(|relayed| relayed.src == emitter && cancel.ts_us <= relayed.ts_us);
        let beaten = |far_2xx: &Answer<'_>| {
            onward.cancel.is_none() && far_2xx.ts_us <= cancel.ts_us + T1_64_US
        };
        far != emitter
            && onward.invite_src == Some(emitter)
            && onward.took_invite_us.is_some_and(|at| took_us <= at)
            && onward.answers.iter().any(|far_2xx| {
                far_2xx.taker == emitter
                    && far_2xx.ts_us <= answer.ts_us
                    && far_2xx.body == Some(body)
                    && (relayed_cancel || beaten(far_2xx))
            })
    })
}
