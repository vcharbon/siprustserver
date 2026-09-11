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
//! normalised value, a re-rendered body. ADR-0029 X3 makes the divergence
//! unexpressable in the emitter's types; this rule is the same invariant read
//! off the wire, so a refactor that re-renders a rung is charged however it is
//! paced.
//!
//! **The occasion is one message an endpoint emitted MORE THAN ONCE**, and what
//! makes two datagrams one message is [`Identity`]: the emitter and its
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
use std::ops::Range;

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{endpoint_addr, Kind, Msg, WireView};

use super::Obligation;

/// 64·T1, RFC 3261 §17: the envelope inside which a copy is a rung of the same
/// transaction rather than a fresh emission under a reused key.
const ENVELOPE_US: u64 = 32_000_000;

/// **RFC 3261 §17 / §13.3.1.4, RFC 3262 §3 — a rung is byte-identical to the
/// emission it repeats.** See the module doc.
///
/// Charges the endpoint that sent the divergent copy. Repeating the same bytes
/// — or emitting once — discharges it.
pub struct RungByteIdentical;

impl Obligation for RungByteIdentical {
    fn id(&self) -> RuleId {
        RuleId::RungByteIdentical
    }

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

/// Where two copies' bytes first disagree, the emitter's own `Record-Route`
/// rows left out of the comparison.
struct Divergence {
    /// `"head"` or `"body"`.
    region: &'static str,
    /// Byte offset into that region of the FIRST copy.
    offset: usize,
    /// The line spanning the disagreement in each copy; `None` where that copy
    /// ends before it.
    first_line: Option<String>,
    rung_line: Option<String>,
    first_len: usize,
    rung_len: usize,
}

impl Divergence {
    /// The first disagreement between two copies, or `None` where they are one
    /// datagram byte for byte outside the rows `emitter` owns per forward.
    ///
    /// A vantage may store the whole datagram as the head (the live recording
    /// does) or split the body off it (a capture with a binary part does); a
    /// disagreement past a blank line both copies carry is located in the body
    /// either way, so the two vantages report one region.
    fn between(
        first_head: &[u8],
        first_body: Option<&[u8]>,
        copy_head: &[u8],
        copy_body: Option<&[u8]>,
        emitter: &str,
    ) -> Option<Divergence> {
        let first_mask = own_record_route_spans(first_head, emitter);
        let copy_mask = own_record_route_spans(copy_head, emitter);
        if let Some((fi, ci)) = first_difference(first_head, &first_mask, copy_head, &copy_mask) {
            let body_start = |raw: &[u8]| sniff::body(raw).map(|b| raw.len() - b.len());
            return Some(match (body_start(first_head), body_start(copy_head)) {
                (Some(fs), Some(cs)) if fi >= fs && ci >= cs => Divergence::in_region(
                    "body",
                    fi - fs,
                    ci - cs,
                    &first_head[fs..],
                    &copy_head[cs..],
                ),
                _ => Divergence::in_region("head", fi, ci, first_head, copy_head),
            });
        }
        let (first_body, copy_body) = (first_body?, copy_body?);
        let (fi, ci) = first_difference(first_body, &[], copy_body, &[])?;
        Some(Divergence::in_region("body", fi, ci, first_body, copy_body))
    }

    fn in_region(
        region: &'static str,
        first_offset: usize,
        rung_offset: usize,
        first: &[u8],
        copy: &[u8],
    ) -> Divergence {
        Divergence {
            region,
            offset: first_offset,
            first_line: sniff::line_at(first, first_offset),
            rung_line: sniff::line_at(copy, rung_offset),
            first_len: first.len(),
            rung_len: copy.len(),
        }
    }
}

/// The spans of `raw`'s `Record-Route` rows every hop of which names `emitter`
/// — the emitter's own §16.6 stamp, the one row class a rung may re-render. A
/// row no reader accepts, or one that folds a foreign hop beside the emitter's
/// own, keeps its bytes in the comparison. Empty where `emitter` is no socket
/// address: nothing then names it.
fn own_record_route_spans(raw: &[u8], emitter: &str) -> Vec<Range<usize>> {
    let Some(addr) = endpoint_addr(emitter) else { return Vec::new() };
    let names_emitter = |hop: &sniff::UriFacts| {
        hop.port == addr.port()
            && hop.host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip == addr.ip())
    };
    sniff::route_rows(raw, "Record-Route")
        .into_iter()
        .filter(|row| {
            row.hops.as_ref().is_some_and(|hops| !hops.is_empty() && hops.iter().all(names_emitter))
        })
        .map(|row| row.span)
        .collect()
}

/// The first byte at which `a` and `b` disagree once the spans `a_mask` /
/// `b_mask` (ascending, disjoint) are left out of each — as an offset into
/// each ORIGINAL, the shorter's end where what remains of one is a strict
/// prefix of the other — or `None` where the remainders are equal. The spans
/// pair in lockstep: a masked row one copy carries where the other carries
/// none is a disagreement at that row, so a rung that drops, adds or moves
/// the emitter's own row is charged like any other re-render.
fn first_difference(
    a: &[u8],
    a_mask: &[Range<usize>],
    b: &[u8],
    b_mask: &[Range<usize>],
) -> Option<(usize, usize)> {
    let (mut i, mut j) = (0, 0);
    let (mut am, mut bm) = (a_mask.iter().peekable(), b_mask.iter().peekable());
    loop {
        let a_masked = am.peek().is_some_and(|r| r.start <= i);
        let b_masked = bm.peek().is_some_and(|r| r.start <= j);
        match (a_masked, b_masked) {
            (true, true) => {
                i = i.max(am.next().unwrap().end);
                j = j.max(bm.next().unwrap().end);
                continue;
            }
            (false, false) => {}
            _ => return Some((i, j)),
        }
        match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) if x == y => {
                i += 1;
                j += 1;
            }
            (None, None) => return None,
            _ => return Some((i, j)),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The rule's OWN semantics under a CLOSED observation: which copies form
    //! one message, and what the bytes alone settle about them.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding, RuleId};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::RungByteIdentical;

    const UAC: &str = "10.0.0.1:5060";
    const UAS: &str = "10.0.0.2:5060";

    /// A response the UAS sent, its bytes given in full.
    fn rsp(at_us: u64, status: u16, method: &str, to_tag: &str, raw: &str) -> Msg {
        Msg {
            at_us,
            src: UAS.to_string(),
            dst: UAC.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Response { status },
            call_id: "c1".to_string(),
            cseq: 1,
            cseq_method: method.to_string(),
            from_tag: Some("fa".to_string()),
            to_tag: (!to_tag.is_empty()).then(|| to_tag.to_string()),
            via_branch: Some("z9hG4bK-1".to_string()),
            head: Some(raw.as_bytes().to_vec()),
            body: sip_message::sniff::body(raw.as_bytes()).map(<[u8]>::to_vec),
        }
    }

    /// A request the UAC sent, its bytes given in full.
    fn req(at_us: u64, method: &str, branch: &str, raw: &str) -> Msg {
        Msg {
            at_us,
            src: UAC.to_string(),
            dst: UAS.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: method.to_string() },
            call_id: "c1".to_string(),
            cseq: 1,
            cseq_method: method.to_string(),
            from_tag: Some("fa".to_string()),
            to_tag: None,
            via_branch: Some(branch.to_string()),
            head: Some(raw.as_bytes().to_vec()),
            body: sip_message::sniff::body(raw.as_bytes()).map(<[u8]>::to_vec),
        }
    }

    /// A 200 (INVITE), headers in the order given.
    fn ok_200(rows: &[&str]) -> String {
        format!("SIP/2.0 200 OK\r\n{}\r\n\r\n", rows.join("\r\n"))
    }

    const ROWS: [&str; 6] = [
        "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1",
        "From: <sip:a@h>;tag=fa",
        "To: <sip:b@h>;tag=tb",
        "Call-ID: c1",
        "CSeq: 1 INVITE",
        "Content-Length: 0",
    ];

    fn obs(msgs: &[Msg]) -> Observation {
        let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
        let mut last_us = 0;
        for m in msgs {
            last_us = last_us.max(m.at_us);
            for ep in [&m.src, &m.dst] {
                let at = endpoint_last_us.entry(ep.clone()).or_default();
                *at = (*at).max(m.at_us);
            }
        }
        Observation { last_us, endpoint_last_us, closed: true }
    }

    fn eval(msgs: &[Msg]) -> Vec<Finding> {
        RungByteIdentical.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    fn violated(f: &Finding) -> &Evidence {
        match &f.decision {
            Decision::Violated(e) => e,
            other => panic!("expected a violation, got {other:?}"),
        }
    }

    /// The violation the ticket names: a 2xx whose repeat was re-composed — the
    /// same headers, two of them swapped — is not the same response.
    #[test]
    fn a_re_composed_2xx_rung_is_violated() {
        let first = ok_200(&ROWS);
        let mut swapped = ROWS;
        swapped.swap(4, 5);
        let rung = ok_200(&swapped);
        let f = eval(&[
            rsp(1_000, 200, "INVITE", "tb", &first),
            rsp(501_000, 200, "INVITE", "tb", &rung),
        ]);
        assert_eq!(f.len(), 1, "one occasion, the message: {f:?}");
        assert_eq!(f[0].rule, RuleId::RungByteIdentical);
        assert_eq!(f[0].emitter, UAS, "the endpoint that re-composed its rung is charged");
        assert_eq!(f[0].taker, UAC);
        assert_eq!(f[0].anchor, 1, "the occasion rests on the divergent copy");
        let Evidence::RungDiverged {
            rung,
            copies,
            divergent,
            class,
            method,
            status,
            rseq,
            branch,
            region,
            offset,
            first_line,
            rung_line,
            gap_us,
            ..
        } = violated(&f[0])
        else {
            panic!("rung-diverged evidence: {:?}", f[0].decision)
        };
        assert_eq!((*rung, *copies, *divergent), (1, 2, 1));
        assert_eq!(class, "2xx final");
        assert_eq!((method.as_str(), *status, *rseq), ("INVITE", Some(200), None));
        assert_eq!(branch, "z9hG4bK-1");
        assert_eq!(region, "head");
        assert_eq!(
            *offset,
            first.find("CSeq: 1").unwrap() + 1,
            "the first byte the two disagree on"
        );
        assert_eq!(first_line.as_deref(), Some("CSeq: 1 INVITE"));
        assert_eq!(rung_line.as_deref(), Some("Content-Length: 0"));
        assert_eq!(*gap_us, 500_000);
    }

    /// A normalised value is a different byte: `Content-Length:0` is not
    /// `Content-Length: 0`, whatever a parser makes of both.
    #[test]
    fn a_normalised_value_on_a_rung_is_violated() {
        let first = ok_200(&ROWS);
        let rung = first.replace("Content-Length: 0", "Content-Length:0");
        let f = eval(&[
            rsp(1_000, 200, "INVITE", "tb", &first),
            rsp(501_000, 200, "INVITE", "tb", &rung),
        ]);
        assert_eq!(f.len(), 1);
        assert!(f[0].violated(), "{f:?}");
    }

    /// The obliged behaviour: the same bytes again, marked a repeat by the
    /// adapter or not, is one compliant occasion.
    #[test]
    fn a_byte_identical_rung_is_compliant() {
        let raw = ok_200(&ROWS);
        let mut marked = rsp(501_000, 200, "INVITE", "tb", &raw);
        marked.repeat = true;
        let f = eval(&[
            rsp(1_000, 200, "INVITE", "tb", &raw),
            marked,
            rsp(1_501_000, 200, "INVITE", "tb", &raw),
        ]);
        assert_eq!(f.len(), 1, "one occasion however many rungs: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{f:?}");
        assert_eq!(f[0].anchor, 1, "a compliant occasion rests on the second copy");
    }

    #[test]
    fn a_single_emission_is_no_occasion() {
        assert!(eval(&[rsp(1_000, 200, "INVITE", "tb", &ok_200(&ROWS))]).is_empty());
    }

    /// Two forks answer the one INVITE under the one branch and CSeq; their tags
    /// are what says they are two messages, and no bytes are compared.
    #[test]
    fn a_forks_2xx_is_not_a_repeat_of_another_forks() {
        let winner = ok_200(&ROWS);
        let mut rows = ROWS;
        rows[2] = "To: <sip:b@h>;tag=tc";
        let loser = ok_200(&rows);
        let f = eval(&[
            rsp(1_000, 200, "INVITE", "tb", &winner),
            rsp(2_000, 200, "INVITE", "tc", &loser),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    /// A different status on the one transaction is the single-final rule's
    /// occasion, not a rung of anything.
    #[test]
    fn a_second_final_of_another_status_is_not_a_rung() {
        let first = ok_200(&ROWS).replace("200 OK", "487 Terminated");
        let second = ok_200(&ROWS).replace("200 OK", "480 Gone");
        let f = eval(&[
            rsp(1_000, 487, "INVITE", "tb", &first),
            rsp(2_000, 480, "INVITE", "tb", &second),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    fn reliable_183(rseq: u32, extra: &str) -> String {
        format!(
            "SIP/2.0 183 Session Progress\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:a@h>;tag=fa\r\n\
             To: <sip:b@h>;tag=tb\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             Require: 100rel\r\n\
             RSeq: {rseq}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
        )
    }

    /// RFC 3262 §3 sequences distinct provisionals through one transaction:
    /// two 183 that differ in `RSeq` are two messages.
    #[test]
    fn two_reliable_provisionals_with_distinct_rseq_are_two_messages() {
        let f = eval(&[
            rsp(1_000, 183, "INVITE", "tb", &reliable_183(1, "")),
            rsp(2_000, 183, "INVITE", "tb", &reliable_183(2, "P-Early-Media: sendrecv\r\n")),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    /// The same `RSeq` again with other bytes is a re-composed rung of the one
    /// reliable provisional.
    #[test]
    fn a_re_composed_reliable_provisional_rung_is_violated() {
        let f = eval(&[
            rsp(1_000, 183, "INVITE", "tb", &reliable_183(1, "")),
            rsp(501_000, 183, "INVITE", "tb", &reliable_183(1, "P-Early-Media: sendrecv\r\n")),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Evidence::RungDiverged { class, rseq, status, .. } = violated(&f[0]) else { panic!() };
        assert_eq!((class.as_str(), *rseq, *status), ("reliable provisional", Some(1), Some(183)));
    }

    /// An unreliable provisional rides no timer and §13.3.1.1 lets the UAS
    /// send as many distinct ones as it likes — two 183, or two 100, on one
    /// transaction are two messages and never an occasion.
    #[test]
    fn an_unreliable_provisional_is_never_an_occasion() {
        let ringing = |extra: &str| {
            format!(
                "SIP/2.0 183 Session Progress\r\n\
                 Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
                 To: <sip:b@h>;tag=tb\r\n\
                 CSeq: 1 INVITE\r\n{extra}Content-Length: 0\r\n\r\n"
            )
        };
        let f = eval(&[
            rsp(1_000, 183, "INVITE", "tb", &ringing("")),
            rsp(501_000, 183, "INVITE", "tb", &ringing("P-Early-Media: sendrecv\r\n")),
        ]);
        assert!(f.is_empty(), "{f:?}");
        let trying = |extra: &str| format!("SIP/2.0 100 Trying\r\nCSeq: 1 INVITE\r\n{extra}\r\n");
        let f = eval(&[
            rsp(1_000, 100, "INVITE", "", &trying("")),
            rsp(501_000, 100, "INVITE", "", &trying("Timestamp: 2\r\n")),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    fn invite(branch: &str, max_forwards: u8) -> String {
        format!(
            "INVITE sip:b@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             From: <sip:a@h>;tag=fa\r\n\
             To: <sip:b@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: {max_forwards}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// The client transaction retransmits THE request (§17.1.1.2): a copy
    /// under the same branch with another byte is charged to the UAC.
    #[test]
    fn a_re_composed_request_rung_is_violated() {
        let f = eval(&[
            req(1_000, "INVITE", "z9hG4bK-1", &invite("z9hG4bK-1", 70)),
            req(501_000, "INVITE", "z9hG4bK-1", &invite("z9hG4bK-1", 69)),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, UAC);
        let Evidence::RungDiverged { class, method, status, first_line, rung_line, .. } =
            violated(&f[0])
        else {
            panic!()
        };
        assert_eq!((class.as_str(), method.as_str(), *status), ("INVITE request", "INVITE", None));
        assert_eq!(first_line.as_deref(), Some("Max-Forwards: 70"));
        assert_eq!(rung_line.as_deref(), Some("Max-Forwards: 69"));
    }

    /// A new branch is a new transaction (§17.1.3): the same CSeq re-sent under
    /// it is compared with nothing.
    #[test]
    fn a_request_under_a_new_branch_is_a_new_transaction() {
        let f = eval(&[
            req(1_000, "INVITE", "z9hG4bK-1", &invite("z9hG4bK-1", 70)),
            req(2_000, "INVITE", "z9hG4bK-2", &invite("z9hG4bK-2", 69)),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    fn ack(branch: &str, extra: &str) -> String {
        format!(
            "ACK sip:b@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             From: <sip:a@h>;tag=fa\r\n\
             To: <sip:b@h>;tag=tb\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 ACK\r\n{extra}\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// An ACK rides no timer, but the one re-passed on a repeated final is THE
    /// ACK (§17.1.1.2): other bytes under the same branch are charged. A fresh
    /// ACK under a new branch is its own transaction (§17.1.1.3) and no rung.
    #[test]
    fn an_ack_re_passed_with_other_bytes_is_violated_and_a_new_branch_is_not() {
        let f = eval(&[
            req(1_000, "ACK", "z9hG4bK-a", &ack("z9hG4bK-a", "")),
            req(501_000, "ACK", "z9hG4bK-a", &ack("z9hG4bK-a", "Max-Forwards: 70\r\n")),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Evidence::RungDiverged { class, .. } = violated(&f[0]) else { panic!() };
        assert_eq!(class, "ACK");
        let f = eval(&[
            req(1_000, "ACK", "z9hG4bK-a", &ack("z9hG4bK-a", "")),
            req(501_000, "ACK", "z9hG4bK-b", &ack("z9hG4bK-b", "Max-Forwards: 70\r\n")),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    /// A non-INVITE final is re-passed on each repeated request (§17.2.2) —
    /// the same final, so a re-composed one is charged like any other.
    #[test]
    fn a_re_composed_non_invite_final_is_violated() {
        let bye_ok = |extra: &str| {
            format!(
                "SIP/2.0 200 OK\r\n\
                 Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
                 To: <sip:b@h>;tag=tb\r\n\
                 Call-ID: c1\r\n\
                 CSeq: 1 BYE\r\n{extra}Content-Length: 0\r\n\r\n"
            )
        };
        let f = eval(&[
            rsp(1_000, 200, "BYE", "tb", &bye_ok("")),
            rsp(501_000, 200, "BYE", "tb", &bye_ok("Server: x\r\n")),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Evidence::RungDiverged { class, method, .. } = violated(&f[0]) else { panic!() };
        assert_eq!((class.as_str(), method.as_str()), ("non-INVITE final", "BYE"));
    }

    /// Past 64·T1 the transaction is gone: a copy under the old identity opens
    /// a ladder of its own and repeats nothing.
    #[test]
    fn past_the_envelope_a_matching_identity_is_a_fresh_message() {
        let first = ok_200(&ROWS);
        let later = first.replace("Content-Length: 0", "Content-Length:0");
        let f = eval(&[
            rsp(1_000, 200, "INVITE", "tb", &first),
            rsp(33_001_000, 200, "INVITE", "tb", &later),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    /// Without a branch the copies cannot be split into transactions, so a
    /// divergence is undecidable; identical bytes are compliant regardless.
    #[test]
    fn without_a_branch_a_divergent_copy_is_undecidable() {
        let first = ok_200(&ROWS);
        let rung = first.replace("Content-Length: 0", "Content-Length:0");
        let mut a = rsp(1_000, 200, "INVITE", "tb", &first);
        let mut b = rsp(501_000, 200, "INVITE", "tb", &rung);
        a.via_branch = None;
        b.via_branch = None;
        let f = eval(&[a.clone(), b]);
        assert_eq!(f.len(), 1);
        assert!(
            matches!(f[0].decision, Decision::Undecidable("no via branch at this vantage")),
            "{f:?}"
        );
        let mut same = a.clone();
        same.at_us = 501_000;
        let f = eval(&[a, same]);
        assert!(matches!(f[0].decision, Decision::Compliant), "{f:?}");
    }

    /// A copy the vantage carried no bytes for can be compared with nothing.
    #[test]
    fn without_bytes_a_copy_is_undecidable() {
        let first = ok_200(&ROWS);
        let mut opaque = rsp(501_000, 200, "INVITE", "tb", &first);
        opaque.head = None;
        opaque.body = None;
        let f = eval(&[rsp(1_000, 200, "INVITE", "tb", &first), opaque]);
        assert_eq!(f.len(), 1);
        assert!(
            matches!(f[0].decision, Decision::Undecidable("no bytes at this vantage")),
            "{f:?}"
        );
    }

    fn ok_200_sdp(port: u16) -> String {
        let body = format!("v=0\r\no=- 1 1 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio {port} RTP/AVP 0\r\n");
        let mut rows = ROWS.to_vec();
        rows.pop();
        rows.push("Content-Type: application/sdp");
        let len = format!("Content-Length: {}", body.len());
        rows.push(&len);
        format!("SIP/2.0 200 OK\r\n{}\r\n\r\n{body}", rows.join("\r\n"))
    }

    /// A body re-rendered on the rung is located in the body, whether the
    /// vantage stored the datagram whole (the live recording) or split the
    /// body off its head (a capture with a binary part).
    #[test]
    fn a_body_divergence_is_located_in_the_body_at_either_vantage() {
        let first = ok_200_sdp(4000);
        let rung = ok_200_sdp(4002);
        let whole = eval(&[
            rsp(1_000, 200, "INVITE", "tb", &first),
            rsp(501_000, 200, "INVITE", "tb", &rung),
        ]);
        assert_eq!(whole.len(), 1);
        let Evidence::RungDiverged { region, offset, first_line, rung_line, .. } =
            violated(&whole[0])
        else {
            panic!()
        };
        let (whole_region, whole_offset) = (region.clone(), *offset);
        assert_eq!(region, "body");
        assert_eq!(first_line.as_deref(), Some("m=audio 4000 RTP/AVP 0"));
        assert_eq!(rung_line.as_deref(), Some("m=audio 4002 RTP/AVP 0"));

        let split = |mut m: Msg| {
            let head = m.head.take().unwrap();
            let body_len = m.body.as_ref().unwrap().len();
            m.head = Some(head[..head.len() - body_len].to_vec());
            m
        };
        let f = eval(&[
            split(rsp(1_000, 200, "INVITE", "tb", &first)),
            split(rsp(501_000, 200, "INVITE", "tb", &rung)),
        ]);
        let Evidence::RungDiverged { region, offset, .. } = violated(&f[0]) else { panic!() };
        assert_eq!(
            (region.as_str(), *offset),
            (whole_region.as_str(), whole_offset),
            "one region, one offset, either way"
        );
    }

    /// One occasion per message: the finding rests on the FIRST divergent copy
    /// and counts every copy and every divergence on the ladder.
    #[test]
    fn every_divergent_copy_is_counted_on_one_finding() {
        let first = ok_200(&ROWS);
        let other = first.replace("Content-Length: 0", "Content-Length:0");
        let f = eval(&[
            rsp(1_000, 200, "INVITE", "tb", &first),
            rsp(501_000, 200, "INVITE", "tb", &first),
            rsp(1_501_000, 200, "INVITE", "tb", &other),
            rsp(3_501_000, 200, "INVITE", "tb", &other),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].anchor, 2);
        let Evidence::RungDiverged { rung, copies, divergent, .. } = violated(&f[0]) else {
            panic!()
        };
        assert_eq!((*rung, *copies, *divergent), (2, 4, 2));
    }

    /// Another call's, another hop's or another taker's copy is another
    /// message: the identity folds none of them together.
    #[test]
    fn another_call_hop_or_taker_is_another_message() {
        let first = ok_200(&ROWS);
        let other = first.replace("Content-Length: 0", "Content-Length:0");
        let mut on_c2 = rsp(501_000, 200, "INVITE", "tb", &other);
        on_c2.call_id = "c2".to_string();
        let mut on_hop1 = rsp(501_000, 200, "INVITE", "tb", &other);
        on_hop1.hop = 1;
        let mut to_other = rsp(501_000, 200, "INVITE", "tb", &other);
        to_other.dst = "10.0.0.3:5060".to_string();
        let f = eval(&[rsp(1_000, 200, "INVITE", "tb", &first), on_c2, on_hop1, to_other]);
        assert!(f.is_empty(), "{f:?}");
    }

    // ── the one masked row: the emitter's own Record-Route ──────────────────

    const PROXY: &str = "10.0.0.9:5080";
    const WORKER: &str = "10.0.0.2:5091";

    /// The a-leg INVITE a proxy forwards to a worker: its own Via on top, then
    /// `rows` (its Record-Route stamps), then the caller's rows.
    fn forwarded_invite(rows: &[&str]) -> String {
        format!(
            "INVITE sip:b@10.0.0.2:5091 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5080;branch=z9hG4bK-p1;rport\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1;received=10.0.0.1;rport=5060\r\n\
             {}\r\n\
             From: <sip:a@h>;tag=fa\r\n\
             To: <sip:b@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 69\r\n\
             Content-Length: 0\r\n\r\n",
            rows.join("\r\n")
        )
    }

    /// A copy the proxy put on the wire toward the worker.
    fn forwarded(at_us: u64, raw: &str) -> Msg {
        let mut m = req(at_us, "INVITE", "z9hG4bK-p1", raw);
        m.src = PROXY.to_string();
        m.dst = WORKER.to_string();
        m
    }

    const OWN_COOKIE_B2: &str = "Record-Route: <sip:10.0.0.9:5080;w_pri=b1;w_bak=b2;lr>";
    const OWN_COOKIE_NONE: &str = "Record-Route: <sip:10.0.0.9:5080;w_pri=b1;w_bak=\"\";lr>";
    const OWN_OUTBOUND: &str = "Record-Route: <sip:10.0.0.9:5080;outbound;lr>";
    const FOREIGN: &str = "Record-Route: <sip:10.0.0.7:5060;lr>";

    /// The proxy's own §16.6 stamp is the one row a rung may re-render: the
    /// backup named in the cookie changed between two Timer-A copies of one
    /// INVITE, and nothing else did.
    #[test]
    fn a_proxys_own_record_route_row_may_differ_between_rungs() {
        let first = forwarded_invite(&[OWN_OUTBOUND, OWN_COOKIE_B2, FOREIGN]);
        let rung = forwarded_invite(&[OWN_OUTBOUND, OWN_COOKIE_NONE, FOREIGN]);
        assert_ne!(first, rung);
        let f = eval(&[forwarded(1_000, &first), forwarded(501_000, &rung)]);
        assert_eq!(f.len(), 1, "one occasion, the message: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{f:?}");
        assert_eq!(f[0].anchor, 1);
    }

    /// A Record-Route naming anyone else is not the emitter's to re-render:
    /// the copies are charged, and the finding points into the ORIGINAL bytes
    /// past the masked row, whose length differs between the two.
    #[test]
    fn a_foreign_record_route_row_on_a_rung_is_violated() {
        let first = forwarded_invite(&[OWN_COOKIE_B2, FOREIGN]);
        let rung = forwarded_invite(&[OWN_COOKIE_NONE, "Record-Route: <sip:10.0.0.7:5060;x=1;lr>"]);
        let f = eval(&[forwarded(1_000, &first), forwarded(501_000, &rung)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, PROXY);
        let Evidence::RungDiverged { region, offset, first_line, rung_line, .. } = violated(&f[0])
        else {
            panic!()
        };
        assert_eq!(region, "head");
        assert_eq!(
            *offset,
            first.find(FOREIGN).unwrap() + "Record-Route: <sip:10.0.0.7:5060;".len(),
            "the offset is into the first copy, past its own (longer) cookie row"
        );
        assert_eq!(first_line.as_deref(), Some(FOREIGN));
        assert_eq!(rung_line.as_deref(), Some("Record-Route: <sip:10.0.0.7:5060;x=1;lr>"));

        // The same row is nobody's to re-render at another vantage: the worker
        // re-sending the request it took would be charged for the cookie row.
        let mut a = forwarded(1_000, &forwarded_invite(&[OWN_COOKIE_B2]));
        let mut b = forwarded(501_000, &forwarded_invite(&[OWN_COOKIE_NONE]));
        (a.src, b.src) = (WORKER.to_string(), WORKER.to_string());
        let f = eval(&[a, b]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "{f:?}");
    }

    /// Every other byte stays in the comparison: a `CSeq` re-rendered beside a
    /// re-stamped cookie is charged, and located in the `CSeq` row.
    #[test]
    fn a_re_rendered_row_beside_the_masked_one_is_still_violated() {
        let first = forwarded_invite(&[OWN_COOKIE_B2]);
        let rung =
            forwarded_invite(&[OWN_COOKIE_NONE]).replace("CSeq: 1 INVITE", "CSeq:  1 INVITE");
        let f = eval(&[forwarded(1_000, &first), forwarded(501_000, &rung)]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Evidence::RungDiverged { offset, first_line, rung_line, .. } = violated(&f[0]) else {
            panic!()
        };
        assert_eq!(*offset, first.find("CSeq: 1").unwrap() + "CSeq: ".len());
        assert_eq!(first_line.as_deref(), Some("CSeq: 1 INVITE"));
        assert_eq!(rung_line.as_deref(), Some("CSeq:  1 INVITE"));
    }

    /// The mask pairs rows, it does not license their absence: a rung that
    /// drops the emitter's own row, or adds one, is charged at that row — in
    /// the copy that carries it, against whatever the other carries there.
    #[test]
    fn a_dropped_or_added_own_record_route_row_on_a_rung_is_violated() {
        let with = forwarded_invite(&[OWN_OUTBOUND, OWN_COOKIE_B2, FOREIGN]);
        let without = forwarded_invite(&[OWN_OUTBOUND, FOREIGN]);

        let f = eval(&[forwarded(1_000, &with), forwarded(501_000, &without)]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Evidence::RungDiverged { offset, first_line, rung_line, .. } = violated(&f[0]) else {
            panic!()
        };
        assert_eq!(
            *offset,
            with.find(OWN_COOKIE_B2).unwrap(),
            "the dropped row, in the first copy"
        );
        assert_eq!(first_line.as_deref(), Some(OWN_COOKIE_B2));
        assert_eq!(rung_line.as_deref(), Some(FOREIGN));

        let f = eval(&[forwarded(1_000, &without), forwarded(501_000, &with)]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Evidence::RungDiverged { offset, first_line, rung_line, .. } = violated(&f[0]) else {
            panic!()
        };
        assert_eq!(
            *offset,
            without.find(FOREIGN).unwrap(),
            "the added row, where the first copy has none"
        );
        assert_eq!(first_line.as_deref(), Some(FOREIGN));
        assert_eq!(rung_line.as_deref(), Some(OWN_COOKIE_B2));
    }

    /// A row that folds a foreign hop beside the emitter's own is compared
    /// whole — the conservative reading of a row the emitter only half owns.
    #[test]
    fn a_folded_record_route_row_with_a_foreign_hop_is_compared_whole() {
        let folded = |cookie: &str| format!("{cookie}, <sip:10.0.0.7:5060;lr>");
        let first = forwarded_invite(&[&folded(OWN_COOKIE_B2)]);
        let rung = forwarded_invite(&[&folded(OWN_COOKIE_NONE)]);
        let f = eval(&[forwarded(1_000, &first), forwarded(501_000, &rung)]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Evidence::RungDiverged { first_line, .. } = violated(&f[0]) else { panic!() };
        assert_eq!(first_line.as_deref(), Some(folded(OWN_COOKIE_B2).as_str()));
    }
}
