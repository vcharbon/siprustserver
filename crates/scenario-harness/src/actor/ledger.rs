//! The **acknowledgement ledger** — the spine of the settle barrier
//! ([`super::settle`]). A call is reported OK only once every in-dialog request
//! is acknowledged; the ledger is the record of which acknowledgements are
//! still outstanding.
//!
//! # Two kinds of obligation
//!
//! - **A sent request awaiting its final** (our NOTIFY awaiting its 200, our
//!   BYE awaiting its 200, our re-INVITE awaiting its 2xx, our REFER awaiting
//!   its 202) — an [`ObligationKey`] opens when we send and closes when the
//!   matching final is observed.
//! - **An offer we answered awaiting its ACK** (a re-INVITE/UPDATE we 200'd) —
//!   opens on the answer, closes on the ACK.
//!
//! Plus a per-**dialog** CSeq **gap detector** ([`InDialogCseq`]): a dropped
//! request the peer never saw leaves a hole in the dialog's received-CSeq
//! stream (this is endurance failure #2 — a dropped REFER-progress NOTIFY). The
//! stream is tracked against **all in-dialog CSeqs on the dialog**, never a
//! NOTIFY-only stream: BYE and keepalive-OPTIONS share the dialog CSeq space
//! (RFC 3261 §12.2.1.1), so a NOTIFY-only `seen == 1..=high_water` would report
//! phantom holes for every non-NOTIFY request.
//!
//! # Why it is a grow-only (monotone, commutative) fold
//!
//! Reconciliation of observations from N concurrent reactors must be
//! **order-independent** — the same set of facts folded in any order must yield
//! the same verdict (the `(p,b)`-causal, never time-based, discipline the HA
//! work pins). A naive `open`-then-`remove` map is NOT commutative: a close
//! that arrives before its open would find nothing to remove, then the open
//! would insert and never clear. So obligations are two **grow-only** sets —
//! `opened` and `closed` — and an obligation is satisfied iff its key is in
//! both. Applying `(open, close)` in either order ends in the same state. The
//! CSeq streams are grow-only `BTreeSet`s for the same reason (set insertion
//! commutes). Fold-order determinism then holds by construction.

use std::collections::{BTreeSet, HashMap, HashSet};

use tokio::time::Instant;

/// What class of in-dialog acknowledgement an obligation waits on — bounded so
/// the describe/settle strings stay low-cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObligationKind {
    /// A re-INVITE we sent, awaiting its 2xx (OR one we answered, awaiting the ACK).
    ReInvite,
    /// A NOTIFY we sent, awaiting its 200.
    Notify,
    /// A BYE we sent, awaiting its 200.
    Bye,
    /// A REFER we sent, awaiting its 202.
    Refer,
    /// A PRACK we sent, awaiting its 200.
    Prack,
    /// An UPDATE we sent, awaiting its 200.
    Update,
    /// A plain in-dialog request we sent (INFO/MESSAGE) carrying an optional
    /// body, awaiting its 2xx. Unlike re-INVITE/UPDATE it opens NO sub-flow and
    /// takes NO ACK — its 2xx **alone** closes it — but it rides the same ledger
    /// so a dropped request (or its 2xx) holds the settle barrier open until
    /// re-emitted: the loss-soak contract every in-dialog request gets (a lost
    /// NOTIFY, a lost realign ACK). That is the whole reason to route it through
    /// the ledger rather than fire-and-forget. `INFO` today (an MRF media leg's
    /// MSCML `INFO(EOF)`), `MESSAGE` next — see [`from_cseq_method`](Self::from_cseq_method).
    InDialog,
    /// A non-2xx final we sent to an initial INVITE (a reject, or the 487 to a
    /// CANCELled ring), awaiting its hop-ACK (§17.2.1). Closed by the
    /// transaction layer's ACK sighting, not by a response — so it is NOT in
    /// [`from_cseq_method`](Self::from_cseq_method). Keeps the settle barrier
    /// (and thus the per-call recording window) open until a lost hop-ACK is
    /// recovered by the Timer-G final retransmit + the SUT's §17.1.1.2 re-ACK
    /// — the UA-outlives-the-call contract for an abandoned reject leg.
    RejectFinal,
}

impl ObligationKind {
    /// The stable label used in the open-obligation description.
    pub fn label(&self) -> &'static str {
        match self {
            ObligationKind::ReInvite => "re-INVITE",
            ObligationKind::Notify => "NOTIFY",
            ObligationKind::Bye => "BYE",
            ObligationKind::Refer => "REFER",
            ObligationKind::Prack => "PRACK",
            ObligationKind::Update => "UPDATE",
            ObligationKind::InDialog => "in-dialog",
            ObligationKind::RejectFinal => "reject-final",
        }
    }

    /// Map a CSeq method name (as it appears on the wire) to the obligation kind
    /// its final response closes — the key by which a response is matched back
    /// to the request that opened the obligation. `None` for a method that
    /// opens no ledger obligation (e.g. the dialog-creating INVITE, ACK).
    pub fn from_cseq_method(method: &str) -> Option<Self> {
        match method {
            "NOTIFY" => Some(ObligationKind::Notify),
            "BYE" => Some(ObligationKind::Bye),
            "REFER" => Some(ObligationKind::Refer),
            "PRACK" => Some(ObligationKind::Prack),
            "UPDATE" => Some(ObligationKind::Update),
            // A plain body-carrying in-dialog request whose 2xx alone completes
            // it (no ACK, no sub-flow) — the generic origination behind
            // [`super::goals::GoalStep::InDialog`].
            "INFO" | "MESSAGE" => Some(ObligationKind::InDialog),
            // A re-INVITE's 2xx is matched by the ReInvite kind; the initial
            // INVITE creates the dialog and opens no obligation (its CSeq seeds
            // the gap detector instead).
            "INVITE" => Some(ObligationKind::ReInvite),
            _ => None,
        }
    }
}

/// Identifies one outstanding acknowledgement: the leg that owns it, its kind,
/// and the CSeq number of the request. Bounded-cardinality (`leg` is a
/// `&'static str` role, `kind` an enum, `cseq` a number).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObligationKey {
    pub leg: &'static str,
    pub kind: ObligationKind,
    pub cseq: u32,
}

impl ObligationKey {
    pub fn new(leg: &'static str, kind: ObligationKind, cseq: u32) -> Self {
        Self { leg, kind, cseq }
    }
}

/// Bookkeeping for one opened obligation (the metadata used only for the
/// `describe_open` diagnostic; the key alone drives the verdict).
#[derive(Debug, Clone)]
struct Obligation {
    /// When the obligation opened — a `tokio::time::Instant` so it rides the
    /// paused clock (a `std::time::Instant` would freeze under `start_paused`).
    #[allow(dead_code)]
    opened_at: Instant,
    detail: String,
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
    fn new(leg: &'static str) -> Self {
        Self { leg, seen: BTreeSet::new() }
    }

    fn record(&mut self, cseq: u32) {
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
    fn holes(&self) -> Vec<u32> {
        match (self.seen.iter().next(), self.seen.iter().next_back()) {
            (Some(&lo), Some(&hi)) => (lo..=hi).filter(|c| !self.seen.contains(c)).collect(),
            _ => Vec::new(),
        }
    }
}

/// The acknowledgement ledger — grow-only, so the fold is order-independent
/// (see the module doc). Every in-dialog acknowledgement the settle barrier
/// waits on lives here.
#[derive(Default)]
pub struct ObligationLedger {
    /// Every obligation ever opened (grow-only).
    opened: HashMap<ObligationKey, Obligation>,
    /// Every obligation ever closed (grow-only). An obligation is satisfied iff
    /// its key is in BOTH `opened` and `closed`.
    closed: HashSet<ObligationKey>,
    /// Per-dialog CSeq streams, keyed by dialog identity (the Call-ID). Tracks
    /// ALL in-dialog methods, not a NOTIFY-only stream (§12.2.1.1).
    dialogs: HashMap<String, InDialogCseq>,
}

impl ObligationLedger {
    /// Open an obligation (idempotent: re-opening the same key keeps the first
    /// `opened_at`). `detail` is a human string for `describe_open`.
    pub fn open(&mut self, key: ObligationKey, opened_at: Instant, detail: impl Into<String>) {
        self.opened.entry(key).or_insert_with(|| Obligation { opened_at, detail: detail.into() });
    }

    /// Close an obligation (idempotent, and commutes with `open`: a close whose
    /// open has not been folded yet still lands — `is_closed` reconciles the two
    /// grow-only sets).
    pub fn close(&mut self, key: ObligationKey) {
        self.closed.insert(key);
    }

    /// Seed a dialog's CSeq baseline with its dialog-creating request's CSeq, so
    /// the first in-dialog request is not mistaken for a hole above an empty
    /// stream. Idempotent (the stream is a grow-only set).
    pub fn seed_dialog(&mut self, call_id: impl Into<String>, leg: &'static str, cseq: u32) {
        self.dialogs
            .entry(call_id.into())
            .or_insert_with(|| InDialogCseq::new(leg))
            .record(cseq);
    }

    /// Record an in-dialog request's CSeq on its dialog (any method — BYE,
    /// NOTIFY, OPTIONS, re-INVITE, …). Creates the stream if unseen.
    pub fn record_in_dialog(&mut self, call_id: impl Into<String>, leg: &'static str, cseq: u32) {
        self.dialogs
            .entry(call_id.into())
            .or_insert_with(|| InDialogCseq::new(leg))
            .record(cseq);
    }

    /// The verdict predicate: every opened obligation is closed AND every dialog
    /// CSeq stream is gap-free. Order-independent by construction.
    pub fn is_closed(&self) -> bool {
        self.opened.keys().all(|k| self.closed.contains(k))
            && self.dialogs.values().all(InDialogCseq::is_contiguous)
    }

    /// Human descriptions of everything still open — each names the leg and the
    /// CSeq, never free-form text with a Call-ID (bounded cardinality for the
    /// settle FAIL diagnostic). Empty iff [`is_closed`](Self::is_closed).
    pub fn describe_open(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (key, ob) in &self.opened {
            if !self.closed.contains(key) {
                out.push(format!(
                    "{}:{} cseq={} ({}, unacked)",
                    key.leg,
                    key.kind.label(),
                    key.cseq,
                    ob.detail,
                ));
            }
        }
        for stream in self.dialogs.values() {
            for hole in stream.holes() {
                out.push(format!("{}:in-dialog cseq={hole} (gap — never observed)", stream.leg));
            }
        }
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn open_then_close_is_closed() {
        let mut led = ObligationLedger::default();
        let k = ObligationKey::new("bob", ObligationKind::Notify, 3);
        assert!(led.is_closed(), "empty ledger is vacuously closed");
        led.open(k, t0(), "Trying");
        assert!(!led.is_closed(), "an unacked obligation keeps it open");
        led.close(k);
        assert!(led.is_closed(), "closing the obligation satisfies the ledger");
    }

    #[test]
    fn close_before_open_still_reconciles() {
        // The commutativity guarantee: a close folded before its open must land.
        let mut led = ObligationLedger::default();
        let k = ObligationKey::new("bob", ObligationKind::Notify, 3);
        led.close(k);
        led.open(k, t0(), "Trying");
        assert!(led.is_closed(), "grow-only sets reconcile regardless of order");
    }

    #[test]
    fn real_gap_keeps_ledger_open() {
        // A dropped NOTIFY: cseq 1 and 3 seen, 2 never was.
        let mut led = ObligationLedger::default();
        led.seed_dialog("call-abc", "bob", 1);
        led.record_in_dialog("call-abc", "bob", 3);
        assert!(!led.is_closed(), "a hole at cseq 2 leaves the ledger open");
        let open = led.describe_open();
        assert_eq!(open.len(), 1);
        assert!(open[0].contains("bob"), "describe_open names the leg: {open:?}");
        assert!(open[0].contains("cseq=2"), "describe_open names the missing cseq: {open:?}");
    }

    #[test]
    fn non_notify_request_fills_the_hole() {
        // The §12.2.1.1 fix: a BYE sharing the dialog CSeq space closes a hole a
        // NOTIFY-only detector would report forever.
        let mut led = ObligationLedger::default();
        led.seed_dialog("call-abc", "bob", 1);
        led.record_in_dialog("call-abc", "bob", 3); // hole at 2
        assert!(!led.is_closed());
        led.record_in_dialog("call-abc", "bob", 2); // e.g. a BYE / OPTIONS
        assert!(led.is_closed(), "any in-dialog method filling cseq 2 closes the gap");
    }

    #[test]
    fn baseline_is_not_a_phantom_hole() {
        // Seeding with the INVITE cseq (1) then a BYE at cseq 2 must be
        // contiguous — the toy-call shape (no NOTIFYs).
        let mut led = ObligationLedger::default();
        led.seed_dialog("call-xyz", "bob", 1);
        led.record_in_dialog("call-xyz", "bob", 2);
        assert!(led.is_closed(), "1 then 2 is contiguous — no phantom hole at the baseline");
    }

    #[test]
    fn info_and_message_map_to_in_dialog() {
        // The generic in-dialog origination (INFO/MESSAGE) is closed by its 2xx,
        // matched back through the same CSeq-method mapping every sent request
        // uses — no ACK, no sub-flow.
        assert_eq!(ObligationKind::from_cseq_method("INFO"), Some(ObligationKind::InDialog));
        assert_eq!(ObligationKind::from_cseq_method("MESSAGE"), Some(ObligationKind::InDialog));
        assert_eq!(ObligationKind::InDialog.label(), "in-dialog");
    }

    #[test]
    fn in_dialog_obligation_holds_settle_until_its_2xx() {
        // A sent INFO opens an InDialog obligation; only its 2xx closes it — the
        // loss-soak contract (a dropped INFO/2xx keeps the ledger open).
        let mut led = ObligationLedger::default();
        let k = ObligationKey::new("bob", ObligationKind::InDialog, 2);
        led.open(k, t0(), "INFO awaiting 2xx");
        assert!(!led.is_closed(), "the unacked INFO keeps the ledger open");
        assert!(led.describe_open().iter().any(|s| s.contains("bob:in-dialog cseq=2")));
        led.close(k);
        assert!(led.is_closed(), "the INFO's 2xx closes it");
    }

    #[test]
    fn describe_open_lists_both_obligations_and_gaps() {
        let mut led = ObligationLedger::default();
        led.open(ObligationKey::new("alice", ObligationKind::Bye, 2), t0(), "hangup");
        led.seed_dialog("call-1", "bob", 1);
        led.record_in_dialog("call-1", "bob", 4); // holes 2,3
        let open = led.describe_open();
        assert!(open.iter().any(|s| s.contains("alice:BYE cseq=2")), "{open:?}");
        assert!(open.iter().any(|s| s.contains("bob:in-dialog cseq=2")), "{open:?}");
        assert!(open.iter().any(|s| s.contains("bob:in-dialog cseq=3")), "{open:?}");
    }
}
