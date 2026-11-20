//! The tracker both 2xx-ACK rules read: who sent and took each dialog-creating
//! INVITE, each 2xx taken and emitted, and every ACK and BYE.

use std::collections::{BTreeMap, BTreeSet};

use crate::rules::correlation::Parties;
use crate::rules::sides::Sides;
use crate::wire::Msg;

/// One ACK obligation: the party it is read against and the dialog it belongs
/// to. Spelled out as a struct so the three parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ObligationKey<'a> {
    /// The UAC that took the 2xx, by the socket its INVITE left from (owed
    /// map), or the UAS that emitted it (emitted map).
    pub(super) party: &'a str,
    /// The Call-ID: a view may carry several calls through one endpoint, and
    /// a CSeq number is unique only within its call.
    pub(super) call: &'a str,
    pub(super) cseq: u32,
    /// The confirmed dialog's To tag — a fork's 2xx confirms its own dialog
    /// and carries its own ACK, so the tag keeps two obligations apart.
    pub(super) dialog: &'a str,
}

/// What one view's messages say about the ACK obligations on it — the shared
/// tracker both rules read.
#[derive(Debug, Default)]
pub(super) struct Reading<'a> {
    /// (endpoint, Call-ID, INVITE CSeq number) → when that endpoint SENT the
    /// dialog-creating INVITE.
    pub(super) invite: BTreeMap<(&'a str, &'a str, u32), u64>,
    /// (endpoint, Call-ID) → when that endpoint first TOOK a dialog-creating
    /// INVITE on that call: the dial a B2BUA relays onto another call.
    pub(super) invite_taken: BTreeMap<(&'a str, &'a str), u64>,
    /// (Call-ID, INVITE CSeq number) → the first time a dialog-creating INVITE
    /// with that number crossed any vantage of the view. An endpoint whose own
    /// emission is not that first one relayed a dial somebody else opened.
    pub(super) invite_first_us: BTreeMap<(&'a str, u32), u64>,
    /// The dialog-creating 2xx responses TAKEN, one entry per UAC obligation.
    pub(super) owed: BTreeMap<ObligationKey<'a>, Final<'a>>,
    /// EVERY 2xx to an INVITE EMITTED (dialog-creating or not), one entry per
    /// UAS obligation.
    pub(super) emitted: BTreeMap<ObligationKey<'a>, Final<'a>>,
    /// (Call-ID, INVITE CSeq number, To tag) some endpoint on this view
    /// ACKed. Keyed on the OBLIGATION rather than on who met it: the UAC's ACK
    /// travels end to end and may have gone straight past the hop this vantage
    /// watches.
    pub(super) acked: BTreeSet<(&'a str, u32, &'a str)>,
    /// (Call-ID, INVITE CSeq number, To tag) → the LAST time that 2xx crossed
    /// any vantage of the view. A taker that saw it before then passed it on.
    pub(super) propagated: BTreeMap<(&'a str, u32, &'a str), u64>,
    /// (Call-ID, INVITE CSeq number, To tag) → the FIRST time that 2xx
    /// crossed. An emitter whose own emission is later forwarded a 2xx
    /// somebody else originated.
    pub(super) two_xx_first_us: BTreeMap<(&'a str, u32, &'a str), u64>,
    /// (user agent, Call-ID) that sent an ACK with no To tag: it names no
    /// dialog, so every obligation of that agent's on the call becomes
    /// undecidable. The agent is every socket its transactions tie on the call
    /// ([`Parties`]): the ACK may leave from another socket than the INVITE.
    pub(super) untagged_ack: BTreeSet<(&'a str, &'a str)>,
    /// Who acts as one user agent on each call: what ties a B2BUA's sockets
    /// on each of its legs.
    pub(super) parties: Parties<'a>,
    /// Every BYE the view carried, in observation order — discharge for the
    /// UAS rule, corroboration for the UAC rule.
    pub(super) byes: Vec<Bye<'a>>,
}

/// One 2xx as an obligation head: where it sat in the view and who the other
/// side of the obligation is.
#[derive(Debug)]
pub(super) struct Final<'a> {
    pub(super) msg: usize,
    pub(super) hop: usize,
    pub(super) ts_us: u64,
    pub(super) status: u16,
    /// Deliveries of this same 2xx to the same endpoint after the first.
    pub(super) retransmits: u32,
    /// The endpoint on the other side of the obligation.
    pub(super) peer: &'a str,
    /// The socket the 2xx reached.
    pub(super) took_at: &'a str,
    /// The user agent of the 2xx's UAC side ([`Parties`]).
    pub(super) uac_agent: &'a str,
}

/// A BYE as this view carried it. Which dialog it belongs to is read off BOTH
/// tags, because either party may send it (RFC 3261 §15.1).
#[derive(Debug)]
pub(super) struct Bye<'a> {
    pub(super) ts_us: u64,
    pub(super) src: &'a str,
    pub(super) call: &'a str,
    pub(super) from_tag: Option<&'a str>,
    pub(super) to_tag: Option<&'a str>,
}

impl Bye<'_> {
    pub(super) fn on_dialog(&self, call: &str, tag: &str) -> bool {
        self.call == call && (self.from_tag == Some(tag) || self.to_tag == Some(tag))
    }
}

impl<'a> Reading<'a> {
    /// The 2xx the user agent holding `sockets` passed on for the answer it
    /// took at `taken_us` on `call`, where it is a B2BUA relaying one: the call
    /// whose INVITE it took LAST before dialling `call` is the one it relayed,
    /// and the first 2xx it emitted there from `taken_us` on is the answer
    /// passed on. The B2BUA is every socket its transactions tie to
    /// `sockets` on `call`, and on the relayed call every socket tied there to
    /// one of those ([`Parties`]): a tie on a third call joins nothing.
    pub(super) fn relayed_upstream(
        &self,
        sockets: [&'a str; 2],
        call: &'a str,
        taken_us: u64,
    ) -> Option<ObligationKey<'a>> {
        let held: BTreeSet<&str> =
            sockets.into_iter().flat_map(|ep| self.parties.held_on(call, ep)).collect();
        let dialled_us = self
            .invite
            .iter()
            .filter(|((src, c, _), _)| held.contains(src) && *c == call)
            .map(|(_, at)| *at)
            .min()?;
        let ((b2bua_at, upstream), _) = self
            .invite_taken
            .iter()
            .filter(|((dst, c), at)| {
                *c != call
                    && **at < dialled_us
                    && held.iter().any(|ep| self.parties.same(c, dst, ep))
            })
            .max_by_key(|(_, at)| **at)?;
        self.emitted
            .iter()
            .filter(|(k, sent)| {
                k.call == *upstream
                    && self.parties.same(upstream, k.party, b2bua_at)
                    && sent.ts_us >= taken_us
            })
            .min_by_key(|(_, sent)| sent.ts_us)
            .map(|(k, _)| *k)
    }

    pub(super) fn of(msgs: &'a [Msg]) -> Self {
        let sides = Sides::of(msgs);
        let parties = Parties::of(msgs);
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(&sides, &parties, mi, msg);
        }
        seen.parties = parties;
        seen
    }

    /// Absorb one message.
    ///
    /// A repeat is not a fresh event for what CREATES an obligation, but it is
    /// evidence for what MEETS one: an ACK flagged as a repeat is still an ACK
    /// on the wire, and skipping it would charge a UAC whose first ACK this
    /// vantage simply missed.
    ///
    /// An INVITE 2xx with a To tag (one without confirms no dialog and draws no
    /// ACK) feeds both halves:
    /// - only a FRESH delivery says where it travelled: a retransmission ladder
    ///   to one endpoint would otherwise read as that endpoint having passed
    ///   the 2xx on;
    /// - the UAC half collapses the ladder on ONE obligation (§13.2.2.4: one
    ///   ACK answers every retransmission) and counts its rungs; a vantage
    ///   whose first sight of a 2xx is already a repeat never saw the
    ///   obligation come due and records none;
    /// - the UAS half opens the emitter's clear-it obligation on every fresh
    ///   emission, dialog-creating or not: §13.3.1.4 confirms a re-INVITE's 2xx
    ///   the same way (the UAC half stays dialog-creating-only via its
    ///   witnessed-INVITE gate).
    fn absorb(&mut self, sides: &Sides<'a>, parties: &Parties<'a>, mi: usize, msg: &'a Msg) {
        let fresh = !msg.repeat;
        let call = msg.call_id.as_str();
        if msg.is_request("ACK") {
            match msg.to_tag.as_deref() {
                Some(tag) => {
                    self.acked.insert((call, msg.cseq, tag));
                }
                None => {
                    self.untagged_ack.insert((parties.sent_by(msg), call));
                }
            }
        } else if msg.is_request("BYE") {
            self.byes.push(Bye {
                ts_us: msg.at_us,
                src: msg.src.as_str(),
                call,
                from_tag: msg.from_tag.as_deref(),
                to_tag: msg.to_tag.as_deref(),
            });
        } else if fresh && msg.is_request("INVITE") && msg.to_tag.is_none() {
            let first = self.invite_first_us.entry((call, msg.cseq)).or_insert(msg.at_us);
            *first = (*first).min(msg.at_us);
            self.invite.entry((msg.src.as_str(), call, msg.cseq)).or_insert(msg.at_us);
            self.invite_taken.entry((msg.dst.as_str(), call)).or_insert(msg.at_us);
        } else if let Some(status) = msg.status() {
            if !(200..300).contains(&status) || !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                return;
            }
            let Some(dialog) = msg.to_tag.as_deref() else { return };
            if fresh {
                let last = self.propagated.entry((call, msg.cseq, dialog)).or_insert(msg.at_us);
                *last = (*last).max(msg.at_us);
                let first =
                    self.two_xx_first_us.entry((call, msg.cseq, dialog)).or_insert(msg.at_us);
                *first = (*first).min(msg.at_us);
            }
            let uac = sides.uac(msg);
            let taken = ObligationKey { party: uac, call, cseq: msg.cseq, dialog };
            match self.owed.get_mut(&taken) {
                Some(seen) => seen.retransmits += 1,
                None if fresh => {
                    self.owed.insert(
                        taken,
                        Final {
                            msg: mi,
                            hop: msg.hop,
                            ts_us: msg.at_us,
                            status,
                            retransmits: 0,
                            peer: msg.src.as_str(),
                            took_at: msg.dst.as_str(),
                            uac_agent: parties.took(msg),
                        },
                    );
                }
                None => {}
            }
            let emitted = ObligationKey { party: msg.src.as_str(), call, cseq: msg.cseq, dialog };
            if fresh {
                self.emitted.entry(emitted).or_insert(Final {
                    msg: mi,
                    hop: msg.hop,
                    ts_us: msg.at_us,
                    status,
                    retransmits: 0,
                    peer: uac,
                    took_at: msg.dst.as_str(),
                    uac_agent: parties.took(msg),
                });
            }
        }
    }
}
