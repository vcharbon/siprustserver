//! The **received-CSeq stream** of one dialog end — the settle ledger's gap
//! detector ([`super::ledger`]). A request the peer sent and the leg never
//! observed leaves a hole in the stream until the peer's retransmission fills
//! it.

use std::collections::BTreeSet;

/// Names one received-CSeq stream: the remote CSeq space of one dialog as the
/// observing leg sees it. A dialog is its Call-ID plus both tags (RFC 3261
/// §12), and the requests a leg receives on it all carry the sender's tag as
/// their From-tag, so `(call_id, leg, sender_tag)` tells apart the two ends of
/// one dialog and the forked dialogs sharing one Call-ID (§12.1.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CseqStreamKey {
    pub call_id: String,
    /// The observing leg.
    pub leg: &'static str,
    /// The From-tag of the requests the leg receives on the dialog: the
    /// dialog's remote tag on the observing side.
    pub sender_tag: String,
}

/// Per-dialog received-CSeq completeness — the NOTIFY-gap detector, generalized
/// to **every** in-dialog method on the dialog (§12.2.1.1). `seen` is a
/// grow-only set of the CSeq numbers of in-dialog requests observed on this
/// dialog (seeded with the dialog-creating INVITE's CSeq so the baseline is not
/// a phantom hole). Contiguous iff `seen` covers `[min..=max]` with no gap.
#[derive(Debug, Clone)]
pub struct InDialogCseq {
    /// The leg this dialog belongs to — for the `describe_open` diagnostic.
    leg: &'static str,
    seen: BTreeSet<u32>,
}

impl InDialogCseq {
    pub(super) fn new(leg: &'static str) -> Self {
        Self { leg, seen: BTreeSet::new() }
    }

    pub(super) fn leg(&self) -> &'static str {
        self.leg
    }

    pub(super) fn record(&mut self, cseq: u32) {
        self.seen.insert(cseq);
    }

    /// No gap between the lowest and highest CSeq observed (an empty stream is
    /// vacuously contiguous). A hole means a request the peer sent was never
    /// observed here — a dropped datagram the SUT's retransmit has not (yet)
    /// re-emitted.
    pub fn is_contiguous(&self) -> bool {
        match (self.seen.iter().next(), self.seen.iter().next_back()) {
            (Some(&lo), Some(&hi)) => (hi - lo + 1) as usize == self.seen.len(),
            _ => true,
        }
    }

    /// The missing CSeq numbers in `[min..=max]` (empty when contiguous).
    pub(super) fn holes(&self) -> Vec<u32> {
        match (self.seen.iter().next(), self.seen.iter().next_back()) {
            (Some(&lo), Some(&hi)) => (lo..=hi).filter(|c| !self.seen.contains(c)).collect(),
            _ => Vec::new(),
        }
    }
}
