//! The CANCELs a UAS matched on its INVITE transactions (RFC 3261 §9.2): a
//! racing re-INVITE whose CANCEL the UAS took and answered 2xx before it
//! answered the re-INVITE owes that re-INVITE a 487, not §14.2's 491 or 500.

use std::collections::{BTreeMap, BTreeSet};

use crate::rules::branch::BranchName;
use crate::rules::correlation::Parties;
use crate::rules::sides::Sides;
use crate::wire::Msg;

use super::{Answer, TxnKey};

/// Per INVITE transaction at its UAS's user agent: where the first CANCEL it
/// took on the branch sits in the view, and whether it answered a CANCEL there
/// with a 2xx.
#[derive(Debug, Default)]
pub(super) struct Cancels<'a> {
    taken: BTreeMap<TxnKey<'a>, usize>,
    accepted: BTreeSet<TxnKey<'a>>,
}

impl<'a> Cancels<'a> {
    /// File `msg`, the view's message `mi` on top-Via `branch`, where it is a
    /// CANCEL the strict reading admits or a 2xx to one.
    pub(super) fn absorb(
        &mut self,
        sides: &Sides<'a>,
        parties: &Parties<'a>,
        mi: usize,
        msg: &'a Msg,
        branch: &'a str,
    ) {
        let call_id = msg.call_id.as_str();
        let branch = BranchName(branch);
        if msg.is_request("CANCEL") && !msg.refused {
            let txn = TxnKey { endpoint: parties.took(msg), call_id, branch };
            self.taken.entry(txn).or_insert(mi);
        } else if msg.cseq_method.eq_ignore_ascii_case("CANCEL")
            && msg.status().is_some_and(|s| (200..300).contains(&s))
        {
            let endpoint = parties.agent(call_id, sides.uas(msg));
            self.accepted.insert(TxnKey { endpoint, call_id, branch });
        }
    }

    /// Whether `answer`, the first final of `txn`, is the 487 of a CANCEL the
    /// UAS took on `txn` before that final and answered 2xx.
    pub(super) fn terminated(&self, txn: &TxnKey<'a>, answer: &Answer) -> bool {
        answer.status == 487
            && self.accepted.contains(txn)
            && self.taken.get(txn).is_some_and(|&at| at < answer.msg)
    }
}
