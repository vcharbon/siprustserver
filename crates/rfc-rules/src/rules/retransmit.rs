//! RFC 3261 §17 / §13.3.1.4, RFC 3262 §3 — a retransmission is the SAME
//! message, byte for byte.
//!
//! Every retransmitting class the RFCs name re-sends what it sent: the client
//! transaction "retransmits the request" (§17.1.1.2 Timer A, §17.1.2.2 Timer
//! E), the server transaction re-passes "the response" (§17.2.1 Timer G,
//! §17.2.2 on a repeated request), the UAS re-sends its 2xx (§13.3.1.4), the
//! UAC re-passes its ACK (§17.1.1.2) and a reliable provisional is
//! "retransmitted" (RFC 3262 §3). None of them says "a response like it": the
//! peer's transaction layer matched the first copy, and every byte a second
//! copy changes is a byte that peer may read differently — a moved header, a
//! normalised value, a re-rendered body. ADR-0032 X3 makes the divergence
//! unexpressable in the emitter's types; this rule is the same invariant read
//! off the wire, so a refactor that re-renders a rung is charged however it is
//! paced.
//!
//! **The occasion is one message an endpoint emitted MORE THAN ONCE**, and what
//! makes two datagrams one message is `Identity`: the emitter and its
//! destination, the hop, the call, the §17 transaction (top-Via branch, CSeq
//! number and method) and, for a response, the status, the To tag and the
//! `RSeq`. The tag is what keeps a fork's two 2xx apart — each fork answers the
//! one INVITE under the one branch, and its tag is the only thing that says so
//! — and the `RSeq` is what keeps two reliable 18x apart, which RFC 3262 §3
//! sequences through one transaction. A message that shares none of those with
//! an earlier one is a fresh message, whatever its bytes; a different status on
//! one transaction is [`super::final_response`]'s business, never a rung.
//!
//! **A class that rides no ladder is still one message.** An ACK and a
//! non-INVITE final retransmit on no timer of their own — the ACK on each
//! repeated final (§17.1.1.2), the final on each repeated request (§17.2.2) —
//! so neither has a pacing obligation; but both are re-PASSED, not re-composed,
//! and a differing copy under one transaction is charged all the same. An ACK
//! under a NEW branch is a new transaction (§17.1.1.3 makes the ACK of a 2xx
//! its own) and is compared with nothing. The UNRELIABLE provisional is the
//! exception and is never an occasion: it rides no timer and §13.3.1.1 lets the
//! UAS send as many distinct ones as it likes, so two 183 on one transaction
//! are two messages and nothing on the wire says otherwise — the 100 Trying
//! included.
//!
//! **Bounded by the transaction envelope (64·T1).** Past it the transaction is
//! gone, and a message under the old identity is whatever the emitter now
//! means by it — a ringing refresh, a re-offer — so it opens a new first copy
//! rather than repeating the old one.
//!
//! **One row is masked: the emitter's own `Record-Route`.** A proxy computes
//! its `Record-Route` per forwarded request (§16.6 step 4) and a stateless one
//! has no memory of what it stamped last time, while §16.11 fixes its Via
//! branch so the downstream server transaction absorbs the copy as a
//! retransmission — and that taker never reads a retransmission's route rows,
//! its route set having been fixed by the first copy (§12.1.1). So the two
//! copies are compared with every `Record-Route` row whose EVERY hop names the
//! emitter itself (host and port of the emitting vantage) left out of each;
//! every other byte stays in — the top Via, a `Record-Route` naming anyone
//! else, `CSeq`, the body. A row that folds a foreign hop beside the emitter's
//! own, or that no reader accepts, is compared whole, and the masked rows pair
//! in wire order — a copy that drops, adds or moves the row is charged. Where
//! the copies differ past the mask, the reported offset and lines are into the
//! ORIGINAL bytes.
//!
//! **So are two parameters of a relayed request: the `received` and `rport`
//! the emitter stamps on the Via below its own** (§18.2.1, RFC 3581 §4) — the
//! source address and port of the copy it relays, which a sender that moved
//! between its copies changes while the message stays one (the downstream
//! server transaction matches it on its branch, §17.2.3). They are left out of
//! each copy on its own, unpaired.
//!
//! **The repeat mark does not decide it.** Both adapters mark a repeat only
//! where the bytes ALREADY match, so a re-composed rung arrives unmarked and
//! looks, to every other rule, like a second emission of the same status. This
//! rule keys on the identity alone and compares the bytes itself, which is what
//! lets it see the divergence the mark cannot carry.
//!
//! Charges the emitter. One occasion per identity, decided at its first
//! divergent copy, the remaining copies counted on the same finding; a copy
//! whose bytes the vantage did not carry, or a divergence under no branch, is
//! undecidable rather than charged.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::Obligation;

mod divergence;
use divergence::Divergence;

crate::registry::rules! {
    /// RFC 3261 §17 / §13.3.1.4, RFC 3262 §3: a retransmission is the same message, byte for byte.
    RungByteIdentical = "rung-byte-identical";
}

/// 64·T1, RFC 3261 §17: the envelope inside which a copy is a rung of the same
/// transaction rather than a fresh emission under a reused key.
const ENVELOPE_US: u64 = 32_000_000;

/// **RFC 3261 §17 / §13.3.1.4, RFC 3262 §3 — a rung is byte-identical to the
/// emission it repeats** (see the module doc). Charges the sender of the
/// divergent copy; repeating the same bytes, or emitting once, discharges it.
pub struct RungByteIdentical;

impl Obligation for RungByteIdentical {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut ladders: BTreeMap<Identity<'_>, Ladder<'_>> = BTreeMap::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            let Some((identity, class)) = Identity::of(msg) else { continue };
            match ladders.entry(identity) {
                Entry::Vacant(v) => {
                    v.insert(Ladder::opened_by(mi, msg, class));
                }
                Entry::Occupied(mut o) => {
                    // Past the envelope the transaction is gone: whatever this is,
                    // it repeats nothing and opens a ladder of its own.
                    if msg.at_us.saturating_sub(o.get().first.at_us) > ENVELOPE_US {
                        o.insert(Ladder::opened_by(mi, msg, class));
                        continue;
                    }
                    o.get_mut().absorb(mi, msg);
                }
            }
        }
        let mut out: Vec<Finding> = ladders
            .into_iter()
            .filter_map(|(identity, ladder)| ladder.finding(&identity))
            .collect();
        // Observation order, so a ladder and the report read the same way.
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// The retransmitting classes, each the RFC sentence that makes its repeat the
/// same message. `label` is the vocabulary [`Evidence::RungDiverged::class`]
/// carries and `clause` the citation a report prints for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A 2xx answering an INVITE, re-sent by the UAS until the ACK.
    Final2xx,
    /// A non-2xx final on an INVITE server transaction, Timer G.
    InviteFinal,
    /// A final on a non-INVITE server transaction, re-passed on each repeated
    /// request.
    NonInviteFinal,
    /// A provisional carrying `RSeq`, re-sent by the UAS until the PRACK.
    ReliableProvisional,
    /// An INVITE, Timer A.
    InviteRequest,
    /// Any other request but ACK — CANCEL, PRACK, BYE, … — Timer E.
    NonInviteRequest,
    /// An ACK, re-passed on each repeated final.
    Ack,
}

impl Class {
    /// Every class, for a consumer folding the vocabulary.
    pub const ALL: &'static [Class] = &[
        Class::Final2xx,
        Class::InviteFinal,
        Class::NonInviteFinal,
        Class::ReliableProvisional,
        Class::InviteRequest,
        Class::NonInviteRequest,
        Class::Ack,
    ];

    /// The class `msg` belongs to, or `None` where the class is never an
    /// occasion: an unreliable provisional (the 100 Trying included), or a
    /// provisional whose bytes the vantage did not carry, so nothing says
    /// whether it was reliable.
    fn of(msg: &Msg) -> Option<Class> {
        match &msg.kind {
            Kind::Request { method } => Some(match method.to_ascii_uppercase().as_str() {
                "ACK" => Class::Ack,
                "INVITE" => Class::InviteRequest,
                _ => Class::NonInviteRequest,
            }),
            Kind::Response { status } if *status >= 200 => {
                Some(if msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                    if (200..300).contains(status) {
                        Class::Final2xx
                    } else {
                        Class::InviteFinal
                    }
                } else {
                    Class::NonInviteFinal
                })
            }
            Kind::Response { status } if *status > 100 => {
                sniff::rseq_of(msg.head.as_deref()?).map(|_| Class::ReliableProvisional)
            }
            Kind::Response { .. } => None,
        }
    }

    /// The vocabulary token a finding carries.
    pub fn label(self) -> &'static str {
        match self {
            Class::Final2xx => "2xx final",
            Class::InviteFinal => "non-2xx INVITE final",
            Class::NonInviteFinal => "non-INVITE final",
            Class::ReliableProvisional => "reliable provisional",
            Class::InviteRequest => "INVITE request",
            Class::NonInviteRequest => "non-INVITE request",
            Class::Ack => "ACK",
        }
    }

    /// The class a finding's `class` token names.
    pub fn from_label(label: &str) -> Option<Class> {
        Class::ALL.iter().copied().find(|c| c.label() == label)
    }

    /// The RFC sentence that makes this class's repeat the same message.
    pub fn clause(self) -> &'static str {
        match self {
            Class::Final2xx => "RFC 3261 §13.3.1.4",
            Class::InviteFinal => "RFC 3261 §17.2.1",
            Class::NonInviteFinal => "RFC 3261 §17.2.2",
            Class::ReliableProvisional => "RFC 3262 §3",
            Class::InviteRequest => "RFC 3261 §17.1.1.2",
            Class::NonInviteRequest => "RFC 3261 §17.1.2.2",
            Class::Ack => "RFC 3261 §17.1.1.2",
        }
    }
}

/// What makes two datagrams one message: who sent it where, over which hop, on
/// which call and transaction, and — for a response — which answer. Spelled
/// out as a struct so no two parts can swap places.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Identity<'a> {
    emitter: &'a str,
    taker: &'a str,
    hop: usize,
    call_id: &'a str,
    /// The top-Via branch, or `None` where this vantage carried none — the
    /// identity still folds, but a divergence under it is undecidable.
    branch: Option<&'a str>,
    cseq: u32,
    /// The request method for a request, the CSeq method for a response,
    /// ASCII-uppercased.
    method: String,
    /// A response's status; a request has none.
    status: Option<u16>,
    /// A response's To tag — the fork it answers from. A request's is not part
    /// of its §17 key.
    to_tag: Option<&'a str>,
    /// A reliable provisional's `RSeq`.
    rseq: Option<u64>,
}

impl<'a> Identity<'a> {
    /// The identity `msg` repeats or opens, with its class — or `None` where
    /// the class is never an occasion.
    fn of(msg: &'a Msg) -> Option<(Self, Class)> {
        let class = Class::of(msg)?;
        let (method, status, to_tag, rseq) = match &msg.kind {
            Kind::Request { method } => (method.to_ascii_uppercase(), None, None, None),
            Kind::Response { status } => (
                msg.cseq_method.to_ascii_uppercase(),
                Some(*status),
                msg.to_tag.as_deref(),
                (class == Class::ReliableProvisional)
                    .then(|| msg.head.as_deref().and_then(sniff::rseq_of))
                    .flatten(),
            ),
        };
        Some((
            Identity {
                emitter: msg.src.as_str(),
                taker: msg.dst.as_str(),
                hop: msg.hop,
                call_id: msg.call_id.as_str(),
                branch: msg.via_branch.as_deref().filter(|b| !b.is_empty()),
                cseq: msg.cseq,
                method,
                status,
                to_tag,
                rseq,
            },
            class,
        ))
    }
}

/// One message's copies inside the envelope: the first, and what each later
/// one settled against it.
struct Ladder<'a> {
    class: Class,
    first_msg: usize,
    first: &'a Msg,
    /// Copies carried so far, the first included.
    copies: u32,
    /// The second copy, where one exists: the anchor of a compliant occasion.
    second_msg: Option<usize>,
    /// The first divergent copy and where it diverged.
    offence: Option<(usize, u32, &'a Msg, Divergence)>,
    divergent: u32,
    /// A copy the vantage carried no bytes for, so nothing could be compared.
    incomparable: bool,
}

impl<'a> Ladder<'a> {
    fn opened_by(mi: usize, msg: &'a Msg, class: Class) -> Self {
        Ladder {
            class,
            first_msg: mi,
            first: msg,
            copies: 1,
            second_msg: None,
            offence: None,
            divergent: 0,
            incomparable: false,
        }
    }

    /// Absorb one later copy of the message.
    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
        self.copies += 1;
        self.second_msg.get_or_insert(mi);
        let rung = self.copies - 1;
        match (self.first.head.as_deref(), msg.head.as_deref()) {
            (Some(first), Some(copy)) => {
                let d = Divergence::between(
                    first,
                    self.first.body.as_deref(),
                    copy,
                    msg.body.as_deref(),
                    &self.first.src,
                );
                if let Some(d) = d {
                    self.divergent += 1;
                    self.offence.get_or_insert((mi, rung, msg, d));
                }
            }
            _ => self.incomparable = true,
        }
    }

    /// The occasion this ladder is, or `None` where the message went out once.
    fn finding(self, identity: &Identity<'a>) -> Option<Finding> {
        let second = self.second_msg?;
        let head = |anchor: usize, decision| Finding {
            rule: RuleId::RungByteIdentical,
            emitter: identity.emitter.to_string(),
            taker: identity.taker.to_string(),
            cseq: identity.cseq,
            relayed: false,
            anchor,
            decision,
        };
        let Some((mi, rung, msg, d)) = self.offence else {
            return Some(if self.incomparable {
                head(second, Decision::Undecidable("no bytes at this vantage"))
            } else {
                head(second, Decision::Compliant)
            });
        };
        // Without a branch the copies cannot be split into transactions, so the
        // divergent one may be another transaction's answer.
        let Some(branch) = identity.branch else {
            return Some(head(mi, Decision::Undecidable("no via branch at this vantage")));
        };
        Some(head(
            mi,
            Decision::Violated(Evidence::RungDiverged {
                rung_msg: mi,
                rung_hop: msg.hop,
                rung_ts_us: msg.at_us,
                first_msg: self.first_msg,
                first_ts_us: self.first.at_us,
                rung,
                copies: self.copies,
                divergent: self.divergent,
                class: self.class.label().to_string(),
                method: identity.method.clone(),
                status: identity.status,
                rseq: identity.rseq,
                branch: branch.to_string(),
                region: d.region.to_string(),
                offset: d.offset,
                first_line: d.first_line,
                rung_line: d.rung_line,
                first_len: d.first_len,
                rung_len: d.rung_len,
                gap_us: msg.at_us.saturating_sub(self.first.at_us),
            }),
        ))
    }
}

#[cfg(test)]
mod tests;
