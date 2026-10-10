//! The RFC 3262 reliable-provisional family: fourteen obligations that share one
//! reading of the wire — who opened the INVITE, which provisionals were sent
//! reliably, and which PRACK named which `RSeq`.
//!
//! **Two halves of one reading.** The OBLIGATION half walks the reliable
//! provisionals and the PRACKs that discharge them, keyed by early dialog and
//! `RSeq`. The SERVER-TRANSACTION half walks what each endpoint TOOK on a
//! top-Via branch and what it ANSWERED there, which is what the §3 negotiation
//! rules weigh: a `Require: 100rel` INVITE against the responses it drew, a
//! PRACK against the status it drew back. Both are collected in one pass so a
//! rule never re-derives a fact a sibling already read.
//!
//! **A hop's exemption is the CONSUMER's, not the rule's, for the §3
//! negotiation rules.** Whether a lane merely relays a stream is a property of
//! that lane across the whole recording, not of the message in hand, so those
//! rules leave `relayed` false and the consumer skips the lanes it knows to be
//! relays — the [`crate::rules::final_response`] precedent.
//!
//! **What makes a provisional reliable** (RFC 3262 §3, and nothing less): the
//! UAC offered `100rel` on the INVITE — in `Require` or in `Supported` — and
//! the UAS answered a 101-199 carrying BOTH `Require: 100rel` and an `RSeq`.
//! A 100 Trying is never sent reliably, an `RSeq` on its own is not the
//! negotiation, and a provisional whose INVITE this vantage never saw is a
//! provisional whose negotiation nothing witnessed.
//!
//! **An obligation belongs to a dialog, not to a socket.** RFC 3262 §3 has the
//! UAS retransmit until a PRACK names its `RSeq`, so an `RSeq` some endpoint
//! on the view acknowledged is an obligation MET, whichever socket spoke: a
//! proxy that does not record-route never sees the PRACK the UAC sent straight
//! past it, and charging that proxy would charge the one box on the wire that
//! owes nothing. The early dialog's To tag is the partition — two forks
//! answering one INVITE number their `RSeq`s independently.
//!
//! **A hop is not a UAC, and a hop is not a UAS.** An endpoint that passed a
//! provisional or a PRACK on relayed the behaviour rather than originating it,
//! and every rule here marks such a finding `relayed` for the consumer to
//! weigh. A B2BUA is untouched: it re-originates under a new Call-ID, so its
//! b-leg INVITE is that leg's first and its obligations there are its own.

mod acknowledgement;
mod answering;
mod negotiation;
mod offer;
mod reading;
mod records;
mod sequence;

pub use acknowledgement::{
    NoPrackOf100Trying, NoPrackOfOutOfOrderRseq, RackWithoutKnownInvite,
    UnackedReliableProvisional, PRACK_WINDOW_US,
};
pub use answering::{Prack2xxOr481, UnmatchedPrackProxied};
pub use negotiation::{
    NoReliable1xxOnInDialog, ReliableNeedsClientOptIn, RequireReliable1xxOnRequire,
};
pub use offer::{Delay2xxOnUnackedReliable1xxWithSdp, PrackAnswers1xxOffer};
pub use sequence::{
    NoNewReliable1xxAfterFinal, NoOverlappingReliableProvisionals, NonContiguousRseq,
};

crate::registry::rules! {
    /// RFC 3262 §4: a UAC that took a reliable provisional answers it with a PRACK whose RAck names
    /// that provisional.
    UnackedReliableProvisional = "unacked-reliable-provisional";
    /// RFC 3262 §7.2: a PRACK's `RAck` CSeq-num names the INVITE whose RSeq space the acknowledged
    /// provisional belongs to. Charges the PRACK's sender.
    RackWithoutKnownInvite = "rack-without-known-invite";
    /// RFC 3262 §3: a UAS holds one reliable provisional outstanding at a time on a dialog — the
    /// next waits for the PRACK of the last. Charges the UAS; a retransmission of an RSeq already
    /// sent is not a second provisional.
    NoOverlappingReliableProvisionals = "no-overlapping-reliable-provisionals";
    /// RFC 3262 §3: each subsequent reliable provisional on a dialog carries `RSeq = prior + 1`.
    /// Charges the UAS.
    NonContiguousRseq = "non-contiguous-rseq";
    /// RFC 3262 §4: a UAC PRACKs reliable provisionals in RSeq order — one that arrived out of
    /// order waits for the gap to fill. Charges the UAC.
    NoPrackOfOutOfOrderRseq = "no-prack-of-out-of-order-rseq";
    /// RFC 3262 §3: a UAS handed an INVITE requiring `100rel` answers every non-100 provisional
    /// reliably, or rejects the INVITE 420 naming `100rel` Unsupported. Charges that UAS.
    RequireReliable1xxOnRequire = "require-reliable-1xx-on-require";
    /// RFC 3262 §3: a UAS sends a reliable provisional only where the INVITE opted into `100rel`,
    /// in `Require` or in `Supported`. Charges that UAS.
    ReliableNeedsClientOptIn = "reliable-needs-client-opt-in";
    /// RFC 3262 §3: the PRACK machinery is scoped to the INVITE METHOD — no reliable provisional
    /// answers a non-INVITE request; a re-INVITE is an INVITE. Charges the UAS that sent it.
    NoReliable1xxOnInDialog = "no-reliable-1xx-on-in-dialog";
    /// RFC 3262 §3: a proxy forwards a PRACK matching no reliable provisional of its own rather
    /// than absorbing it. Charges the endpoint that took the PRACK.
    UnmatchedPrackProxied = "unmatched-prack-proxied";
    /// RFC 3262 §3: a PRACK naming an `RSeq` its taker sent reliably draws a 2xx; one naming no
    /// such `RSeq` draws a 481. Charges the endpoint that answered the PRACK.
    Prack2xxOr481 = "prack-2xx-or-481";
    /// RFC 3262 §3: a UAS holds the INVITE 2xx while a reliable provisional of its own that carried
    /// a body is still unPRACKed. Charges that UAS.
    Delay2xxOnUnackedReliable1xxWithSdp = "delay-2xx-on-unacked-reliable-1xx-with-sdp";
    /// RFC 3262 §3: a UAS emits no NEW reliable provisional on an INVITE transaction it has already
    /// answered. Charges that UAS; the reliable-only sibling of [`RuleId::No1xxAfterFinal`](crate::RuleId::No1xxAfterFinal), keyed
    /// on `RSeq`.
    NoNewReliable1xxAfterFinal = "no-new-reliable-1xx-after-final";
    /// RFC 3262 §4: a 100 Trying is never reliable, so a UAC never PRACKs one whatever `Require:
    /// 100rel` it carries. Charges the PRACK's sender.
    NoPrackOf100Trying = "no-prack-of-100-trying";
    /// RFC 3262 §5: the PRACK of a reliable provisional that carried an OFFER carries the answer.
    /// Charges the PRACK's sender.
    PrackAnswers1xxOffer = "prack-answers-1xx-offer";
}

#[cfg(test)]
mod tests;
