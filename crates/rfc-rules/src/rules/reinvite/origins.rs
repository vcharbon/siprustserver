//! Where each INVITE transaction came from: what tells a spiral's copy from a
//! second INVITE.

use std::collections::{BTreeMap, BTreeSet};

use sip_message::header::SentBy;
use sip_message::sniff;

use crate::rules::branch::BranchName;
use crate::wire::Msg;

use super::TxnKey;

/// The bottom Via (sent-by and branch) and CSeq of each INVITE transaction an
/// endpoint drove, where the vantage carried them: what every forwarder keeps
/// (§16.6), so the INVITE by another path (§16.3) is told by them.
#[derive(Default)]
pub(super) struct Origins<'a> {
    of: BTreeMap<TxnKey<'a>, (SentBy, String, u32)>,
}

impl<'a> Origins<'a> {
    pub(super) fn record(&mut self, msg: &Msg, txn: TxnKey<'a>) {
        if let Some(origin) = origin(msg) {
            self.of.entry(txn).or_insert(origin);
        }
    }

    /// Whether `msg` is, by its bottom Via and CSeq, one of the INVITEs
    /// `endpoint` has `in_flight` on its dialog under another top-Via branch.
    pub(super) fn copies_one_of(
        &self,
        msg: &Msg,
        endpoint: &str,
        in_flight: &BTreeSet<&str>,
    ) -> bool {
        let Some(origin) = origin(msg) else { return false };
        in_flight.iter().any(|branch| {
            let txn =
                TxnKey { endpoint, call_id: msg.call_id.as_str(), branch: BranchName(branch) };
            self.of.get(&txn) == Some(&origin)
        })
    }
}

/// `msg`'s bottom Via sent-by and branch, and its CSeq number, or `None` where
/// the vantage carried no header block or the bottom Via names no branch.
fn origin(msg: &Msg) -> Option<(SentBy, String, u32)> {
    let bottom = msg.head.as_deref().and_then(sniff::bottom_via)?;
    Some((bottom.sent_by, bottom.branch?, msg.cseq))
}
