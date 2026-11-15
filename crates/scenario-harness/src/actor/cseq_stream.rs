//! The **received-CSeq stream** of one dialog end — the settle ledger's gap
//! detector ([`super::ledger`]). A request the peer sent and the leg never
//! observed leaves a hole in the stream until the peer's retransmission fills
//! it.

use std::collections::BTreeSet;

/// The most missing CSeq numbers one stream lists in the settle diagnostic;
/// the rest are counted on one line.
pub(super) const MAX_LISTED_HOLES: usize = 16;

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
/// A peer starts its space anywhere below 2^31 (§8.1.1.5), so no operation
/// enumerates `[min..=max]`: each costs at most `seen` plus
/// [`MAX_LISTED_HOLES`].
#[derive(Debug, Clone, Default)]
pub struct InDialogCseq {
    seen: BTreeSet<u32>,
}

/// The holes of one stream: the lowest missing numbers, at most
/// [`MAX_LISTED_HOLES`], and how many more are missing above them.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Holes {
    pub listed: Vec<u32>,
    pub unlisted: u64,
}

impl InDialogCseq {
    pub(super) fn record(&mut self, cseq: u32) {
        self.seen.insert(cseq);
    }

    /// No gap between the lowest and highest CSeq observed (an empty stream is
    /// vacuously contiguous). A hole means a request the peer sent was never
    /// observed here — a dropped datagram the SUT's retransmit has not (yet)
    /// re-emitted.
    pub fn is_contiguous(&self) -> bool {
        self.missing() == 0
    }

    /// How many numbers of `[min..=max]` were never observed.
    fn missing(&self) -> u64 {
        match (self.seen.first(), self.seen.last()) {
            (Some(&lo), Some(&hi)) => u64::from(hi) - u64::from(lo) + 1 - self.seen.len() as u64,
            _ => 0,
        }
    }

    /// The holes of `[min..=max]`, lowest first (none when contiguous).
    pub(super) fn holes(&self) -> Holes {
        let mut listed = Vec::new();
        for (&below, &above) in self.seen.iter().zip(self.seen.iter().skip(1)) {
            if listed.len() == MAX_LISTED_HOLES {
                break;
            }
            listed.extend((below + 1..above).take(MAX_LISTED_HOLES - listed.len()));
        }
        let unlisted = self.missing() - listed.len() as u64;
        Holes { listed, unlisted }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(seen: &[u32]) -> InDialogCseq {
        let mut s = InDialogCseq::default();
        for &cseq in seen {
            s.record(cseq);
        }
        s
    }

    /// Holes between several runs are listed lowest first, across runs.
    #[test]
    fn holes_span_every_run() {
        let s = stream(&[1, 3, 4, 7]);
        assert!(!s.is_contiguous());
        assert_eq!(s.holes(), Holes { listed: vec![2, 5, 6], unlisted: 0 });
    }

    /// A stream spanning a small and a large space (2 and a 10-digit base,
    /// §8.1.1.5) reports its gap in bounded lines and bounded work.
    #[test]
    fn a_ten_digit_span_is_counted_not_enumerated() {
        let s = stream(&[2, 1_684_995_485]);
        assert!(!s.is_contiguous());
        let holes = s.holes();
        assert_eq!(holes.listed, (3..19).collect::<Vec<u32>>());
        assert_eq!(holes.unlisted, 1_684_995_482 - 16);
    }

    /// The extremes of the CSeq space (§8.1.1.5 bounds it below 2^31, a
    /// receiver still sees any u32) neither overflow nor read as contiguous.
    #[test]
    fn the_full_u32_span_does_not_overflow() {
        let s = stream(&[0, u32::MAX]);
        assert!(!s.is_contiguous());
        assert_eq!(s.holes().unlisted, u64::from(u32::MAX) - 1 - 16);
        assert!(stream(&[u32::MAX - 1, u32::MAX]).is_contiguous());
    }
}
