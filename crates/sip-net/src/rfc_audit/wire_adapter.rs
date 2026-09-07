//! The live adapter (issue 29): the recorded event channel projected into the
//! `rfc-rules` wire model, per bind, and the merged rules surfaced as
//! cross-message audit findings.
//!
//! Observation policy lives here, not in the rules: the harness controls
//! shutdown and drains transit before close, so every view is CLOSED — an
//! absence at end-of-stream decides immediately, and the rules' capture-side
//! observability windows collapse. Consumer policy per issue 29: a finding is
//! surfaced only when it is `Violated`, charged to the VANTAGE bind itself,
//! and not relay-attributed — a hop-vantage or undecidable occasion is the
//! old engine's silence, kept deliberately (R3; revisit per rule as later
//! rungs port).
//!
//! Projection reads each arrival's production-time [`WireStamp`]
//! (parse-once + the §17.2 repeat mark) and dedups SENDS by transaction key
//! here, at projection — the send-side twin of that stamp.

use std::collections::HashMap;

use layer_harness::{LaneKey, Stamped};
use sip_message::{SipMessage, SipParser};

use rfc_rules::rules::Obligation;
use rfc_rules::{Decision, Kind, Msg, Observation, WireView};

use crate::contracts::{CrossMessageAuditRule, SignalingNetworkEvent};
use crate::report::wire_positions_by_stamp;
use crate::rfc_audit::msg_reads::{
    call_id, cseq_method, cseq_seq, from_tag, status, to_tag, top_via_branch,
};
use crate::rfc_audit::relay_lanes::relay_lanes;
use crate::types::UaRole;

/// One bind's stream in the wire model, with the per-message bookkeeping the
/// adapter needs to format and anchor a finding.
struct BindView {
    bind: LaneKey,
    msgs: Vec<Msg>,
    /// Per msg: the originating event's stamp `seq` (the waiver anchor) and
    /// the Call-ID (finding text; the wire model itself never keys on it).
    meta: Vec<(u64, String)>,
}

/// Project the (audit-visible) events into per-bind views, in event order.
fn bind_views(events: &[Stamped<SignalingNetworkEvent>]) -> Vec<BindView> {
    let mut order: Vec<LaneKey> = Vec::new();
    let mut views: HashMap<LaneKey, BindView> = HashMap::new();
    let mut send_tables: HashMap<LaneKey, crate::repeat::RepeatTables> = HashMap::new();
    let parser = crate::rfc_audit::lenient_parser();

    for s in events {
        let (bind, parsed, repeat, src, dst, head): (
            &LaneKey,
            SipMessage,
            bool,
            String,
            String,
            Vec<u8>,
        ) = match &s.event {
            SignalingNetworkEvent::RecvItem { bind_key, packet, wire, .. } => {
                // Parsed once at event production; a datagram even the lenient
                // parser rejects can key no obligation.
                let Some(parsed) = wire.parsed.as_deref() else { continue };
                (
                    bind_key,
                    parsed.clone(),
                    wire.repeat_of.is_some(),
                    packet.src.to_string(),
                    bind_key.clone(),
                    packet.raw.clone(),
                )
            }
            SignalingNetworkEvent::SendCalled { bind_key, to, msg } => {
                let Ok(parsed) = parser.parse(msg) else { continue };
                let repeat = send_tables
                    .entry(bind_key.clone())
                    .or_default()
                    .note(msg, &parsed, s.seq)
                    .is_some();
                (bind_key, parsed, repeat, bind_key.clone(), to.to_string(), msg.clone())
            }
            _ => continue,
        };
        let kind = match &parsed {
            SipMessage::Request(r) => Kind::Request { method: r.method().to_string() },
            SipMessage::Response(_) => Kind::Response { status: status(&parsed) },
        };
        let view = views.entry(bind.clone()).or_insert_with(|| {
            order.push(bind.clone());
            BindView { bind: bind.clone(), msgs: Vec::new(), meta: Vec::new() }
        });
        // Strictly monotonic per-view stamps: the channel records at ms
        // resolution, so a relay's receive and forward can TIE — which would
        // defeat the rules' order-based hop gates (who emitted first, who
        // passed a message on). Event `seq` is the total order (the wire
        // model's tiebreak); a µs bump inside the tied ms preserves it.
        let at_us =
            (s.at_ms * 1_000).max(view.msgs.last().map_or(0, |m| m.at_us + 1));
        view.msgs.push(Msg {
            at_us,
            src,
            dst,
            hop: 0,
            repeat,
            kind,
            call_id: call_id(&parsed).to_string(),
            cseq: cseq_seq(&parsed),
            cseq_method: cseq_method(&parsed).to_string(),
            via_branch: top_via_branch(&parsed),
            from_tag: from_tag(&parsed).map(str::to_string),
            to_tag: to_tag(&parsed).map(str::to_string),
            head: Some(head),
            // The vantage IS the datagram, so the body is always carried: a
            // message with none states an EMPTY one, never an unknown one.
            body: Some(body_of(&parsed).to_vec()),
        });
        view.meta.push((s.seq, call_id(&parsed).to_string()));
    }
    order.into_iter().filter_map(|b| views.remove(&b)).collect()
}

/// The body bytes a parsed message carried — empty where it carried none.
fn body_of(m: &SipMessage) -> &[u8] {
    match m {
        SipMessage::Request(r) => r.body(),
        SipMessage::Response(r) => r.body(),
    }
}

/// The CLOSED observation over one bind's stream.
fn observation(msgs: &[Msg]) -> Observation {
    let mut obs = Observation { closed: true, ..Observation::default() };
    for m in msgs {
        obs.last_us = obs.last_us.max(m.at_us);
        for ep in [&m.src, &m.dst] {
            let at = obs.endpoint_last_us.entry(ep.clone()).or_default();
            *at = (*at).max(m.at_us);
        }
    }
    obs
}

/// Run `rule` over every bind view and keep the violated, non-relayed findings
/// the `vantage` predicate accepts for the bind the view belongs to.
fn at_vantage(
    events: &[Stamped<SignalingNetworkEvent>],
    rule: &dyn Obligation,
    vantage: impl Fn(&rfc_rules::Finding, &LaneKey) -> bool,
    detail: impl Fn(&rfc_rules::Finding, &str) -> String,
) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
    let positions = wire_positions_by_stamp(events);
    let mut out = Vec::new();
    for view in bind_views(events) {
        let obs = observation(&view.msgs);
        for f in rule.eval(&WireView { msgs: &view.msgs, obs: &obs }) {
            if !f.violated() || f.relayed || !vantage(&f, &view.bind) {
                continue;
            }
            let (seq, cid) = &view.meta[f.anchor];
            // The charged party is the rule's own `emitter` — the bind that
            // emitted the offending message or owed the missing one — whatever
            // vantage this finding is reported at.
            out.push((
                view.bind.clone(),
                detail(&f, cid),
                positions.get(seq).copied(),
                Some(f.emitter.clone()),
            ));
        }
    }
    out
}

/// Run `rule` over every bind view and keep the findings the live policy
/// surfaces: violated, charged to the vantage bind, not relay-attributed.
fn surfaced(
    events: &[Stamped<SignalingNetworkEvent>],
    rule: &dyn Obligation,
    detail: impl Fn(&rfc_rules::Finding, &str) -> String,
) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
    at_vantage(events, rule, |f, bind| f.emitter == *bind, detail)
}

/// The same, surfaced at the TAKER instead — for a rule that audits what this
/// bind's PEER sent IT. The bind is the vantage, not the charged party: the
/// finding's `emitter` names the far UAC whose stream is judged, and the
/// recording is the only place that stream is checked at all, so it is reported
/// on the lane that took it (which is also where a scoped waiver's structural
/// party attribution still resolves to the true offender, off the offending
/// wire entry's sender).
fn surfaced_at_taker(
    events: &[Stamped<SignalingNetworkEvent>],
    rule: &dyn Obligation,
    detail: impl Fn(&rfc_rules::Finding, &str) -> String,
) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
    at_vantage(events, rule, |f, bind| f.taker == *bind, detail)
}

/// The same, surfaced at BOTH ends of the offending message — for a rule whose
/// occasion is a two-party negotiation, where either party's lane is a place
/// the recording checks it. The charged party is still the finding's `emitter`;
/// what this widens is which lanes REPORT it, so an offence whose sender is not
/// a recorded bind is still named on the lane that took it.
fn surfaced_at_either_end(
    events: &[Stamped<SignalingNetworkEvent>],
    rule: &dyn Obligation,
    detail: impl Fn(&rfc_rules::Finding, &str) -> String,
) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
    at_vantage(events, rule, |f, bind| f.emitter == *bind || f.taker == *bind, detail)
}

/// The To tag a violated ACK-family finding names, off its evidence.
fn evidence_to_tag(f: &rfc_rules::Finding) -> &str {
    match &f.decision {
        Decision::Violated(
            rfc_rules::Evidence::NoAck { to_tag, .. }
            | rfc_rules::Evidence::Uncleared { to_tag, .. },
        ) => to_tag,
        _ => "",
    }
}

/// RFC 3261 §9.2 live: the merged `no-200-after-cancel` rule
/// (`rfc_rules::rules::cancel`) at this bind's vantage — the UAS answers a
/// cancelled INVITE 487, never 2xx.
///
/// Live order is exact (the stamps above are strictly monotonic per view), so
/// the rule's crossing gate reads a genuine race: a 2xx this bind emitted
/// BEFORE the CANCEL reached it is compliant and surfaces nothing.
pub struct No200AfterCancelRule;

impl CrossMessageAuditRule for No200AfterCancelRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::No200AfterCancel.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::cancel::No200AfterCancel, |f, cid| {
            let (status, gap_us) = match &f.decision {
                Decision::Violated(rfc_rules::Evidence::Cancelled { status, gap_us, .. }) => {
                    (*status, *gap_us)
                }
                _ => (0, 0),
            };
            format!(
                "Answered an INVITE it had already taken a CANCEL for with a {status} (callId \
                 {cid}, CSeq {cseq}), {gap_ms} ms after that CANCEL — RFC 3261 §9.2 has the \
                 cancelled UAS answer 487 Request Terminated",
                cseq = f.cseq,
                gap_ms = gap_us / 1_000,
            )
        })
    }
}

/// RFC 3261 §13.2.2.4 live: the merged `no-ack-to-dialog-creating-2xx` rule
/// (`rfc_rules::rules::ack`) at this bind's vantage — the UAC's half.
pub struct NoAckToDialogCreating2xxRule;

impl CrossMessageAuditRule for NoAckToDialogCreating2xxRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoAckToDialogCreating2xx.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::ack::NoAckToDialogCreating2xx, |f, cid| {
            format!(
                "Took a 2xx confirming the dialog (callId {cid}, To-tag {tt}) to its own \
                 INVITE (CSeq {cseq}) and never ACKed it; RFC 3261 §13.2.2.4 has the UAC \
                 core ACK every 2xx received",
                tt = evidence_to_tag(f),
                cseq = f.cseq,
            )
        })
    }
}

/// RFC 3261 §13.3.1.4 live: the merged `unacked-2xx-not-cleared` rule
/// (`rfc_rules::rules::ack`) at this bind's vantage — the UAS's half, the
/// silent answered-call leak.
pub struct Unacked2xxNotClearedRule;

impl CrossMessageAuditRule for Unacked2xxNotClearedRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::Unacked2xxNotCleared.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::ack::Unacked2xxNotCleared, |f, cid| {
            format!(
                "Sent a 2xx to an INVITE (callId {cid}, To-tag {tt}) that was never ACKed \
                 and the dialog was never BYE'd — the answered call leaks; RFC 3261 \
                 §13.3.1.4 requires retransmitting the 2xx and, on no ACK, BYE-ing the \
                 dialog",
                tt = evidence_to_tag(f),
            )
        })
    }
}

/// RFC 3262 §4 live: the merged `unacked-reliable-provisional` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — the UAC's PRACK duty.
pub struct UnackedReliableProvisionalRule;

impl CrossMessageAuditRule for UnackedReliableProvisionalRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::UnackedReliableProvisional.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::prack::UnackedReliableProvisional, |f, cid| {
            let (rseq, status) = match &f.decision {
                Decision::Violated(rfc_rules::Evidence::Unacked { rseq, status, .. }) => {
                    (*rseq, *status)
                }
                _ => (0, 0),
            };
            format!(
                "Received reliable 1xx (status {status}, RSeq={rseq}, callId {cid}, CSeq \
                 {cseq}) — UAC did not send a matching PRACK (RFC 3262 §4 / RFC3262-MUST-021)",
                cseq = f.cseq,
            )
        })
    }
}

/// RFC 3262 §7.2 live: the merged `rack-without-known-invite` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — the PRACK sender's
/// duty to copy the acknowledged provisional's CSeq.
pub struct RackWithoutKnownInviteRule;

impl CrossMessageAuditRule for RackWithoutKnownInviteRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::RackWithoutKnownInvite.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::prack::RackWithoutKnownInvite, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::UnknownRack {
                rack_rseq,
                rack_cseq,
                known_cseqs,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            // One INVITE opened ⇒ the correct header is unambiguous: name it,
            // so the reader repairs the sender instead of re-deriving the rule.
            let expected = match known_cseqs.as_slice() {
                [only] => format!(", expected \"RAck: {rack_rseq} {only} INVITE\""),
                _ => String::new(),
            };
            let seen =
                known_cseqs.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
            format!(
                "Sent \"RAck: {rack_rseq} {rack_cseq} INVITE\" (callId {cid}) — its CSeq-num \
                 field ({rack_cseq}) names no INVITE this bind opened (opened INVITE CSeq: \
                 {seen}){expected} — RFC 3262 §7.2 (the RAck CSeq-num+method are copied from \
                 the acknowledged 1xx's CSeq, NOT from the PRACK's own CSeq; this PRACK \
                 matches no reliable provisional and settles nothing, so the 1xx keeps \
                 retransmitting)"
            )
        })
    }
}

/// RFC 3262 §3 live: the merged `no-overlapping-reliable-provisionals` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — the UAS serialises its
/// reliable provisionals on the PRACK.
pub struct NoOverlappingReliableProvisionalsRule;

impl CrossMessageAuditRule for NoOverlappingReliableProvisionalsRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoOverlappingReliableProvisionals.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::prack::NoOverlappingReliableProvisionals, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::Overlapping {
                rseq,
                status,
                unacked_rseq,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent second reliable 1xx (status {status}, RSeq={rseq}, callId {cid}) before \
                 prior RSeq {unacked_rseq} PRACKed — RFC 3262 §3 / RFC3262-MUST-012"
            )
        })
    }
}

/// RFC 3262 §3 live: the merged `non-contiguous-rseq` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — the UAS allocates the
/// dialog's RSeq space contiguously.
pub struct NonContiguousRseqRule;

impl CrossMessageAuditRule for NonContiguousRseqRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NonContiguousRseq.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::prack::NonContiguousRseq, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::RseqGap { rseq, prior_rseq, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent reliable 1xx RSeq={rseq} not contiguous with prior RSeq={prior_rseq} \
                 (callId {cid}) — RFC 3262 §3 / RFC3262-MUST-013"
            )
        })
    }
}

/// RFC 3262 §4 live: the merged `no-prack-of-out-of-order-rseq` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — the UAC PRACKs in RSeq
/// order.
pub struct NoPrackOfOutOfOrderRseqRule;

impl CrossMessageAuditRule for NoPrackOfOutOfOrderRseqRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoPrackOfOutOfOrderRseq.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::prack::NoPrackOfOutOfOrderRseq, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::OutOfOrderRack {
                rack_rseq,
                expected_rseq,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent PRACK for out-of-order RSeq={rack_rseq} (expected {expected_rseq}, callId \
                 {cid}) — UAC must PRACK in order (RFC 3262 §4 / RFC3262-MUST-024)"
            )
        })
    }
}

/// RFC 3261 §17.2.1 live: the merged `single-final-per-server-txn` rule
/// (`rfc_rules::rules::final_response`) at this bind's vantage — a server
/// transaction answers once, and only a same-status retransmission follows.
///
/// The live policy this bind adds to the rule body: subject `{Uas}`, plus a
/// relay-lane skip, because the rule judges what a lane AUTHORED. A lane that
/// forwards both directions of one Call-ID relays whatever the upstream
/// produced — the divergent pair is the upstream's defect and is flagged on the
/// upstream lane. The skip has RECORDING granularity, not per-call: a lane
/// classified relay in any dialog slice is unjudged for the whole recording.
pub struct SingleFinalPerServerTxnRule;

impl CrossMessageAuditRule for SingleFinalPerServerTxnRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::SingleFinalPerServerTxn.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Uas])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::final_response::SingleFinalPerServerTxn, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::MultipleFinals {
                first_status,
                second_status,
                divergent,
                method,
                branch,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            // Every divergent status is named once, on this one finding.
            let also = match divergent.as_slice() {
                [_] => String::new(),
                rest => format!(
                    " (and later {})",
                    rest[1..].iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
                ),
            };
            format!(
                "Sent a second final {second_status} on the {method} server transaction \
                 (callId {cid}, branch {branch}) that already answered {first_status}{also} — a \
                 server transaction emits exactly one final response; only a same-status \
                 retransmission is legal (RFC 3261 §17.2.1 / §13.3.1.4)"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §9.1 live: the merged `cancel-route-echoes-invite` rule
/// (`rfc_rules::rules::cancel`) at this bind's vantage — a CANCEL states the
/// path its INVITE stated.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, because
/// the rule judges what a lane AUTHORED. A lane that forwards both directions
/// of one Call-ID passes the upstream UAC's CANCEL through, Route set included,
/// so a divergence there is the upstream's and is flagged on the upstream lane.
/// The skip has RECORDING granularity, not per-call.
pub struct CancelRouteEchoesInviteRule;

impl CrossMessageAuditRule for CancelRouteEchoesInviteRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::CancelRouteEchoesInvite.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::cancel::CancelRouteEchoesInvite, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::CancelRouteDiverged {
                cancel_routes,
                invite_routes,
                branch,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent CANCEL Route values [{}] differ from INVITE Route values [{}] (callId \
                 {cid}, branch {branch}) — RFC 3261 §9.1 / RFC3261-MUST-046",
                cancel_routes.join(", "),
                invite_routes.join(", "),
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §9.1 live: the merged `cancel-after-1xx` rule
/// (`rfc_rules::rules::cancel`) at this bind's vantage — **informational, never
/// gating** (ADR-0028).
///
/// The live policy this bind adds to the rule body: the finding is advisory,
/// because the SUT's bounded hold deliberately sends a pre-1xx CANCEL once the
/// branch stays response-less past the grace window, so a literal §9.1 breach
/// is sanctioned policy here rather than a defect class; and a `{Proxy}`-only
/// declared lane is skipped, because a transparent relay forwards the upstream
/// UAC's CANCEL at the upstream's timing. No relay-lane skip beyond that: a
/// B2BUA/AS authors its own b-leg CANCEL and is judged on it.
pub struct CancelAfter1xxRule;

impl CrossMessageAuditRule for CancelAfter1xxRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::CancelAfter1xx.token()
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let proxy_only: std::collections::HashSet<LaneKey> =
            crate::rfc_audit::bind_roles_of(events)
                .into_iter()
                .filter(|(_, roles)| roles.len() == 1 && roles.contains(&UaRole::Proxy))
                .map(|(lane, _)| lane)
                .collect();
        surfaced(events, &rfc_rules::rules::cancel::CancelAfter1xx, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::EagerCancel { branch, .. }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent CANCEL (callId {cid}, branch {branch}) before any received 1xx for the \
                 INVITE — {{Uac}} RFC 3261 §9.1 / RFC3261-MUST-048"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !proxy_only.contains(lane))
        .collect()
    }
}

/// RFC 3261 §9.1 live: the merged `no-cancel-after-final` rule
/// (`rfc_rules::rules::cancel`) at this bind's vantage — this bind does not
/// CANCEL an INVITE transaction its OWN ACK already completed.
///
/// The live policy this bind adds to the rule body: none. The rule already
/// charges only a CANCEL the emitter sent after ACKing the final (§17.1.1.3),
/// and a live view's stamps are strictly monotonic, so no crossing reaches it;
/// and a lane that merely forwards an upstream CANCEL never ACKed the final
/// itself, so the relay skip the CANCEL siblings carry has nothing to skip.
pub struct NoCancelAfterFinalRule;

impl CrossMessageAuditRule for NoCancelAfterFinalRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoCancelAfterFinal.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::cancel::NoCancelAfterFinal, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::LateCancel {
                final_status,
                since_final_us,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent CANCEL (callId {cid}, CSeq {cseq}) {gap_ms} ms after ACKing the \
                 {final_status} that completed its own INVITE transaction — {{Uac}} RFC 3261 \
                 §9.1; §17.1.1.2 left no transaction to cancel and §9.2 answers the CANCEL 481",
                cseq = f.cseq,
                gap_ms = since_final_us / 1_000,
            )
        })
    }
}

/// The `Call-ID=… from-tag=… to-tag=…` descriptor the CSeq findings name the
/// dialog by.
fn dialog_desc(call_id: &str, from_tag: &str, to_tag: &str) -> String {
    format!(
        "Call-ID={call_id} from-tag={from_tag} to-tag={}",
        if to_tag.is_empty() { "<none>" } else { to_tag },
    )
}

/// RFC 3261 §12.2.1.1 live: the merged `cseq-in-dialog-order` rule
/// (`rfc_rules::rules::cseq`) at this bind's vantage — the far UAC increments
/// the dialog CSeq by exactly one per new request.
///
/// The live policy this bind adds to the rule body: the finding is surfaced at
/// the TAKER. The rule judges the stream a UAC GENERATED, and the recording is
/// where that stream is checked at all (the test UAs answer whatever CSeq they
/// are handed), so the lane that took the stream is the lane that reports it.
pub struct CseqInDialogOrderRule;

impl CrossMessageAuditRule for CseqInDialogOrderRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::CseqInDialogOrder.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced_at_taker(events, &rfc_rules::rules::cseq::CseqInDialogOrder, |f, cid| {
            match &f.decision {
                Decision::Violated(rfc_rules::Evidence::CseqReused {
                    cseq,
                    method,
                    from_tag,
                    to_tag,
                    ..
                }) => format!(
                    "in-dialog CSeq reused (RFC 3261 §12.2.1.1): {method} CSeq {cseq} reuses a \
                     prior request's CSeq (a new in-dialog transaction must increment the dialog \
                     CSeq by exactly one) on {dialog} — it rides a FRESH Via branch, so a real \
                     UAS does not fold it away as a retransmission (§17.2.3): it is a new server \
                     transaction whose sequence number did not advance and is rejected out of \
                     order (§12.2.2 — 500, or an implementation-defined reject) (the test UA \
                     answers it, hiding the bug)",
                    dialog = dialog_desc(cid, from_tag, to_tag),
                ),
                Decision::Violated(rfc_rules::Evidence::CseqNotContiguous {
                    cseq,
                    prior_cseq,
                    method,
                    from_tag,
                    to_tag,
                    ..
                }) if cseq <= prior_cseq => format!(
                    "in-dialog CSeq did not advance (RFC 3261 §12.2.1.1): {method} CSeq {cseq} \
                     does not exceed the dialog-creating CSeq {prior_cseq} on {dialog}; the first \
                     in-dialog request MUST increment the dialog CSeq by exactly one (expected \
                     {expected}) — a value at or below the INVITE's own CSeq reuses or regresses \
                     it, which a real UAS rejects (the test UA answers it, hiding the bug)",
                    dialog = dialog_desc(cid, from_tag, to_tag),
                    expected = prior_cseq + 1,
                ),
                Decision::Violated(rfc_rules::Evidence::CseqNotContiguous {
                    cseq,
                    prior_cseq,
                    method,
                    from_tag,
                    to_tag,
                    ..
                }) => format!(
                    "in-dialog CSeq not contiguous (RFC 3261 §12.2.1.1): {method} CSeq {cseq} \
                     skips ahead of CSeq {prior_cseq}; the UAC MUST increment the dialog CSeq by \
                     exactly one (expected {expected}) on {dialog}",
                    dialog = dialog_desc(cid, from_tag, to_tag),
                    expected = prior_cseq + 1,
                ),
                _ => String::new(),
            }
        })
    }
}

/// RFC 3261 §8.1.3.5 live: the merged `response-cseq-matches-transaction` rule
/// (`rfc_rules::rules::cseq`) at this bind's vantage — a response the bind took
/// carries the CSeq of the request its transaction opened.
///
/// The live policy this bind adds to the rule body: surfaced at the TAKER, the
/// UAC whose client transaction the response claims to answer — the party a
/// real stack would drop it at.
pub struct ResponseCseqMatchesTransactionRule;

impl CrossMessageAuditRule for ResponseCseqMatchesTransactionRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ResponseCseqMatchesTransaction.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced_at_taker(
            events,
            &rfc_rules::rules::cseq::ResponseCseqMatchesTransaction,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::ResponseCseqUnmatched {
                    status,
                    response_cseq,
                    response_method,
                    txn_cseqs,
                    branch,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                format!(
                    "response CSeq does not match its transaction (RFC 3261 §8.1.3.5): {status} \
                     response carries CSeq {response_cseq} {response_method} on Via branch \
                     {branch} (callId {cid}), but no request on that transaction had that CSeq \
                     (it carried: {carried}) — a real UAC drops a response whose CSeq/method does \
                     not match the request it sent (the test UA accepts it, hiding the bug)",
                    carried = txn_cseqs.join(", "),
                )
            },
        )
    }
}

/// RFC 3261 §13.2.2.4 live: the merged `ack-cseq-matches-invite` rule
/// (`rfc_rules::rules::cseq`) at this bind's vantage — an ACK the bind took
/// reuses the CSeq of an INVITE the stream sent it.
///
/// The live policy this bind adds to the rule body: surfaced at the TAKER, the
/// UAS whose INVITE server transaction the ACK fails to match.
pub struct AckCseqMatchesInviteRule;

impl CrossMessageAuditRule for AckCseqMatchesInviteRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::AckCseqMatchesInvite.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced_at_taker(events, &rfc_rules::rules::cseq::AckCseqMatchesInvite, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::AckCseqUnmatched {
                ack_cseq,
                invite_cseqs,
                from_tag,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "ACK CSeq does not match any INVITE (RFC 3261 §13.2.2.4): ACK CSeq {ack_cseq} on \
                 Call-ID={cid} from-tag={from_tag} acknowledges no INVITE the stream sent (it \
                 sent CSeq: {sent}) — an INVITE 2xx ACK reuses the INVITE's CSeq; a running \
                 dialog CSeq advanced by an intervening PRACK/UPDATE is wrong — a real UAS cannot \
                 match it to the INVITE server transaction",
                sent = invite_cseqs.iter().map(u32::to_string).collect::<Vec<_>>().join(", "),
            )
        })
    }
}

/// RFC 3261 §12.2.1.1 live: the merged `mid-dialog-uri` rule
/// (`rfc_rules::rules::dialog`) at this bind's vantage — the in-dialog requests
/// this bind SENT repeat the URIs its dialog was created with.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, because
/// a transparent proxy carries the originator's From/To through unchanged and
/// per-UA URI stability is not its invariant. The skip has RECORDING
/// granularity, not per-call.
pub struct MidDialogUriRule;

impl CrossMessageAuditRule for MidDialogUriRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::MidDialogUri.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::dialog::MidDialogUri, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::MidDialogUriChanged {
                method,
                sent_from_uri,
                dialog_local_uri,
                sent_to_uri,
                dialog_remote_uri,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let mut parts = Vec::new();
            if sent_from_uri != dialog_local_uri {
                parts.push(format!(
                    "in-dialog {method} From URI \"{sent_from_uri}\" differs from dialog local \
                     URI \"{dialog_local_uri}\""
                ));
            }
            if sent_to_uri != dialog_remote_uri {
                parts.push(format!(
                    "in-dialog {method} To URI \"{sent_to_uri}\" differs from dialog remote URI \
                     \"{dialog_remote_uri}\""
                ));
            }
            format!("{} — RFC 3261 §12.2.1.1", parts.join("; "))
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §12.2.1.1 / §16.12 live: the merged `mid-dialog-route` rule
/// (`rfc_rules::rules::dialog`) at this bind's vantage — the in-dialog requests
/// this bind SENT reproduce the dialog route set.
///
/// The live policy this bind adds to the rule body: the same relay-lane skip —
/// a proxy forwards the originator's Route set rather than authoring one.
pub struct MidDialogRouteRule;

impl CrossMessageAuditRule for MidDialogRouteRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::MidDialogRoute.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::dialog::MidDialogRoute, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::MidDialogRouteDiverged {
                method,
                dialog_route_set,
                sent_routes,
                loose_first_route,
                request_uri,
                first_bad_hop,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            if *loose_first_route {
                let first = dialog_route_set.first().map(String::as_str).unwrap_or_default();
                if sent_routes.is_empty() {
                    return format!(
                        "in-dialog {method} omits Route header although the dialog route set is \
                         non-empty (loose, first entry \"{first}\") — RFC 3261 §12.2.1.1"
                    );
                }
                if sent_routes.len() != dialog_route_set.len() {
                    return format!(
                        "in-dialog {method} Route header count {} differs from dialog route set \
                         length {} — RFC 3261 §12.2.1.1",
                        sent_routes.len(),
                        dialog_route_set.len(),
                    );
                }
                let i = first_bad_hop.unwrap_or(0);
                return format!(
                    "in-dialog {method} Route[{i}] \"{}\" routes to a different host:port than \
                     dialog route set entry \"{}\" — RFC 3261 §12.2.1.1",
                    sent_routes.get(i).map(String::as_str).unwrap_or_default(),
                    dialog_route_set.get(i).map(String::as_str).unwrap_or_default(),
                );
            }
            let expected = dialog_route_set.first().map(String::as_str).unwrap_or_default();
            let mut parts = Vec::new();
            if request_uri != expected {
                parts.push(format!(
                    "in-dialog {method} Request-URI \"{request_uri}\" should be first strict \
                     route URI \"{expected}\""
                ));
            }
            let tail = &dialog_route_set[1.min(dialog_route_set.len())..];
            if !(sent_routes.as_slice() == tail
                || (sent_routes.len() == tail.len() + 1 && sent_routes[..tail.len()] == *tail))
            {
                parts.push(format!(
                    "in-dialog {method} strict-route Route tail does not match dialog route set \
                     (expected {tail:?}, got {sent_routes:?})"
                ));
            }
            format!("{} — RFC 3261 §16.12", parts.join("; "))
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §8.1.2 + RFC 3263 §4 live: the merged `mid-dialog-wire-destination`
/// rule (`rfc_rules::rules::dialog`) at this bind's vantage — the datagrams this
/// bind SENT went where their own routing pointed.
///
/// The live policy this bind adds to the rule body: the same relay-lane skip.
pub struct MidDialogWireDestinationRule;

impl CrossMessageAuditRule for MidDialogWireDestinationRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::MidDialogWireDestination.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::dialog::MidDialogWireDestination, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::MidDialogWireTargetDiverged {
                method,
                sent_to,
                target_uri,
                target_host,
                target_port,
                from_route,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let lead = if *from_route {
                "topmost Route URI resolves to "
            } else {
                "Request-URI resolves to "
            };
            format!(
                "in-dialog {method} wire-sent to {sent_to} but {lead}{target_host}:{target_port} \
                 (\"{target_uri}\") — RFC 3261 §8.1.2 + RFC 3263 §4"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §12.1.1 / §12.2.2 live: the merged `record-route-placement` rule
/// (`rfc_rules::rules::dialog`) at this bind's vantage — the responses this bind
/// TOOK carry no Record-Route the route set can no longer absorb.
///
/// The live policy this bind adds to the rule body: surfaced at the TAKER, since
/// the offending response is the PEER's emission and the recording is where a
/// strict UAC's route-set bookkeeping is checked at all; plus the relay-lane
/// skip, so a proxy's mixed stream is not judged as one endpoint's dialog.
pub struct RecordRoutePlacementRule;

impl CrossMessageAuditRule for RecordRoutePlacementRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::RecordRoutePlacement.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_taker(events, &rfc_rules::rules::dialog::RecordRoutePlacement, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::RecordRouteMisplaced {
                status,
                record_route,
                request_method,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            if *status == 100 {
                return format!(
                    "100 Trying carries Record-Route header(s) — 100 is not dialog-creating, \
                     Record-Route is vestigial here per RFC 3261 §12.1.1. Found: {record_route}"
                );
            }
            format!(
                "{status} response to in-dialog {request_method} carries Record-Route — route \
                 set is fixed at dialog establishment per RFC 3261 §12.2.2. Found: {record_route}"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3581 §4 live: the merged `rport-echo` rule (`rfc_rules::rules::via`) at
/// this bind's vantage — **informational, never gating**.
///
/// The live policy this bind adds to the rule body: surfaced at the TAKER (the
/// bind that asked is where the missing echo is observed, and the far server is
/// charged); the finding is advisory, because the source-port lookup only fires
/// under NAT and there is none on 127.0.0.1, so a loopback fake stack that omits
/// the echo is not defective; and the relay-lane skip.
pub struct RportEchoRule;

impl CrossMessageAuditRule for RportEchoRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::RportEcho.token()
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_taker(events, &rfc_rules::rules::via::RportEcho, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::RportNotEchoed {
                status,
                request_method,
                branch,
                echoed_empty,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            if *echoed_empty {
                return format!(
                    "response {status} to {request_method} (branch {branch}) keeps an empty rport \
                     parameter — RFC 3581 §4 requires the server to set it to the source port"
                );
            }
            format!(
                "response {status} to {request_method} (branch {branch}) dropped the rport \
                 parameter the request advertised — RFC 3581 §4 requires the server to echo \
                 rport=<source-port>"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §13.2.1 / §20.37 live: the merged `allow-supported-on-invite` rule
/// (`rfc_rules::rules::capability`) at this bind's vantage — the re-INVITEs and
/// INVITE 2xx this bind TOOK advertised what their sender accepts.
///
/// The live policy this bind adds to the rule body: surfaced at the TAKER (the
/// bind that needed the advertisement reports it, the far sender is charged),
/// plus the relay-lane skip.
///
/// ADVISORY, at the strength the text has: both clauses are SHOULDs. A
/// back-to-back UA states on a relayed re-INVITE exactly what its originator
/// stated and claims nothing of its own on one it originates, so an absence
/// here is the originator's choice or the stack's deliberate silence, never
/// a stripped set — the stripped case is the delta `capability-set-not-relayed`
/// names on the replay lane, message for message.
pub struct AllowSupportedOnInviteRule;

impl CrossMessageAuditRule for AllowSupportedOnInviteRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::AllowSupportedOnInvite.token()
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_taker(events, &rfc_rules::rules::capability::AllowSupportedOnInvite, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::CapabilitiesNotAdvertised {
                missing,
                status,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let label =
                if *status == 0 { "re-INVITE".to_string() } else { format!("{status} OK INVITE") };
            missing
                .iter()
                .map(|h| match h.as_str() {
                    "Allow" => format!(
                        "{label} missing Allow: header — RFC 3261 §13.2.1 (SHOULD list accepted \
                         methods)"
                    ),
                    _ => format!(
                        "{label} missing Supported: header — RFC 3261 §20.37 (SHOULD list \
                         extensions for Require negotiation)"
                    ),
                })
                .collect::<Vec<_>>()
                .join("; ")
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §16.7 step 5 live: the merged `proxy-100-trying-not-forwarded` rule
/// (`rfc_rules::rules::proxy`) at this bind's vantage — this bind took at most
/// one 100 per INVITE it sent on the transaction.
///
/// The live policy this bind adds to the rule body: surfaced at the TAKER (the
/// UAC that observed the extra copy; the hop that forwarded it is charged), plus
/// the relay-lane skip.
pub struct Proxy100TryingNotForwardedRule;

impl CrossMessageAuditRule for Proxy100TryingNotForwardedRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::Proxy100TryingNotForwarded.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_taker(events, &rfc_rules::rules::proxy::Proxy100TryingNotForwarded, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::ExtraTryingForwarded {
                trying_taken,
                invites_sent,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "received {trying_taken} 100 Trying for INVITE CSeq={cseq} Call-ID={cid} against \
                 {invites_sent} INVITE(s) sent — the stateful proxy on the path forwarded a \
                 downstream 100 it should have absorbed per RFC 3261 §16.7 step 5",
                cseq = f.cseq,
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §12.2.2 live: the merged `unknown-dialog-481` rule
/// (`rfc_rules::rules::dialog`) at this bind's vantage — an in-dialog request
/// this bind TOOK for a dialog it never confirmed is answered 481.
///
/// The live policy this bind adds to the rule body: none. The bind is the
/// charged party — it took the request and owed the answer — so the finding
/// surfaces at its emitter, and a relay face that carried both directions of
/// the call knows both of its peers and is never charged (the rule's peer-tag
/// key), so no relay skip is needed.
pub struct UnknownDialog481Rule;

impl CrossMessageAuditRule for UnknownDialog481Rule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::UnknownDialog481.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::dialog::UnknownDialog481, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::UnknownDialogRequest {
                method,
                from_tag,
                to_tag,
                answered_status,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let answered = match answered_status {
                0 => "nothing at all".to_string(),
                st => st.to_string(),
            };
            format!(
                "Received in-dialog request {method} for unknown dialog \
                 {cid}/{from_tag}/{to_tag} — {{Uas}} must respond 481 (RFC 3261 §12.2.2 / \
                 RFC3261-MUST-071); answered {answered}"
            )
        })
    }
}

/// RFC 3261 §8.2.1 live: the merged `unsupported-method-405-allow` rule
/// (`rfc_rules::rules::capability`) at this bind's vantage — an unrecognised
/// verb this bind TOOK is answered 405 with `Allow`.
///
/// The live policy this bind adds to the rule body: none. The bind took the
/// request and owed the rejection, so it is the charged party.
pub struct UnsupportedMethod405AllowRule;

impl CrossMessageAuditRule for UnsupportedMethod405AllowRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::UnsupportedMethod405Allow.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::capability::UnsupportedMethod405Allow, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::RejectionNotIssued {
                method,
                branch,
                answered_status,
                listed_rows,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Received unrecognised method {method} (Call-ID {cid}, branch {branch}) — \
                 {{Uas}} must respond 405 with Allow header (RFC 3261 §8.2.1 / \
                 RFC3261-MUST-030); got {answered_status} / Allow={listed_rows}"
            )
        })
    }
}

/// RFC 3261 §8.2.2 live: the merged `unsupported-extension-420` rule
/// (`rfc_rules::rules::capability`) at this bind's vantage — a `Require` this
/// bind cannot honour is answered 420 with `Unsupported`.
///
/// The live policy this bind adds to the rule body: none.
pub struct UnsupportedExtension420Rule;

impl CrossMessageAuditRule for UnsupportedExtension420Rule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::UnsupportedExtension420.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::capability::UnsupportedExtension420, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::RejectionNotIssued {
                method,
                branch,
                unsupported_tags,
                answered_status,
                listed_rows,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Received request {method} requires unsupported option tag(s) [{tags}] (callId \
                 {cid}, branch {branch}) — {{Uas}} must respond 420 with Unsupported header; got \
                 {answered_status} / Unsupported={listed_rows}",
                tags = unsupported_tags.join(", "),
            )
        })
    }
}

/// RFC 3261 §8.2.3 live: the merged `unsupported-415-accepts` rule
/// (`rfc_rules::rules::capability`) at this bind's vantage — a 415 this bind
/// SENT names the formats it does accept.
///
/// The live policy this bind adds to the rule body: none.
pub struct Unsupported415AcceptsRule;

impl CrossMessageAuditRule for Unsupported415AcceptsRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::Unsupported415Accepts.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::capability::Unsupported415Accepts, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::ResponseHeadersMissing { branch, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent 415 response (callId {cid}, branch {branch}) carries no Accept / \
                 Accept-Encoding / Accept-Language header — {{Uas}} RFC 3261 §8.2.3 / \
                 RFC3261-MUST-036"
            )
        })
    }
}

/// RFC 3261 §21.4.15 live: the merged `unsupported-extension-421` rule
/// (`rfc_rules::rules::capability`) at this bind's vantage — a 421 this bind
/// SENT lists what it demands.
///
/// The live policy this bind adds to the rule body: none.
pub struct UnsupportedExtension421Rule;

impl CrossMessageAuditRule for UnsupportedExtension421Rule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::UnsupportedExtension421.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::capability::UnsupportedExtension421, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::ResponseHeadersMissing { branch, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent 421 response (callId {cid}, branch {branch}) lacks Require header listing \
                 required extensions — {{Uas}} RFC 3261 §21.4.15 / RFC3261-MUST-182"
            )
        })
    }
}

/// RFC 3261 §16.3 live: the merged `no-target-404` rule
/// (`rfc_rules::rules::proxy`) at this bind's vantage — a request this bind
/// resolved no target for is answered 404.
///
/// The live policy this bind adds to the rule body: subject `{Proxy}`, so the
/// rule does not run against UA binds at all — a UAS rejecting a call it took
/// is not a resolution failure; and the finding is advisory, because the B2BUA
/// worker legitimately answers 403/481/491 without forwarding when the backend
/// refuses the call.
pub struct NoTarget404Rule;

impl CrossMessageAuditRule for NoTarget404Rule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoTarget404.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Proxy])
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::proxy::NoTarget404, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::NoTargetFinal {
                method,
                branch,
                status,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "{{Proxy}} received request {method} (callId {cid}, branch {branch}) and emitted \
                 final response {status} without forwarding — expected 404 (RFC 3261 §16.3 / \
                 RFC3261-MUST-105)"
            )
        })
    }
}

/// RFC 3261 §11.2 live: the merged `options-response-echoes` rule
/// (`rfc_rules::rules::capability`) at this bind's vantage — **informational,
/// never gating**.
///
/// The live policy this bind adds to the rule body: the finding is advisory,
/// because the B2BUA answers OPTIONS-keepalive probes (ADR-0008 two-tier
/// OPTIONS) with a deliberately bare 200 — those are transport health checks,
/// not §11.2 capability discovery.
pub struct OptionsResponseEchoesRule;

impl CrossMessageAuditRule for OptionsResponseEchoesRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::OptionsResponseEchoes.token()
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::capability::OptionsResponseEchoes, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::ResponseHeadersMissing { branch, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent 2xx OPTIONS response (callId {cid}, branch {branch}) lacks \
                 Allow/Supported/Accept headers — {{Uas}} RFC 3261 §11.2 / RFC3261-MUST-059"
            )
        })
    }
}

/// RFC 3261 §13.2.2.4 live: the merged `ack-require-subset-of-invite` rule
/// (`rfc_rules::rules::ack`) at this bind's vantage — an ACK requires no more
/// than the INVITE it acknowledges did.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, because
/// the rule judges what a lane AUTHORED. A lane that forwards both directions
/// of one Call-ID passes the upstream UAC's ACK through, Require set included,
/// so an escalation there is the upstream's and is flagged on the upstream lane.
pub struct AckRequireSubsetOfInviteRule;

impl CrossMessageAuditRule for AckRequireSubsetOfInviteRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::AckRequireSubsetOfInvite.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::ack::AckRequireSubsetOfInvite, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::AckRequireNotSubset {
                ack_tags,
                invite_tags,
                branch,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent ACK Require=[{}] not a subset of INVITE Require=[{}] (callId {cid}, branch \
                 {branch}) — RFC 3261 §13.2.2.4 / RFC3261-MUST-035",
                ack_tags.join(", "),
                invite_tags.join(", "),
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §17.1.1.3 live: the merged `ack-preserves-invite-route` rule
/// (`rfc_rules::rules::ack`) at this bind's vantage — the ACK of a non-2xx
/// final states the path its INVITE stated.
///
/// The live policy this bind adds to the rule body: the same relay-lane skip —
/// a forwarding face passes the upstream's ACK through, Route set included.
pub struct AckPreservesInviteRouteRule;

impl CrossMessageAuditRule for AckPreservesInviteRouteRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::AckPreservesInviteRoute.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::ack::AckPreservesInviteRoute, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::AckRouteDiverged {
                ack_routes,
                invite_routes,
                branch,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent ACK Route values [{}] differ from INVITE Route values [{}] (callId {cid}, \
                 branch {branch}) — RFC 3261 §17.1.1.3 / RFC3261-MUST-145",
                ack_routes.join(", "),
                invite_routes.join(", "),
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §16.4 live: the merged `strict-route-rewrite-handled` rule
/// (`rfc_rules::rules::proxy`) at this bind's vantage — a strict-routed request
/// is rewritten before it is forwarded.
///
/// The live policy this bind adds to the rule body: the `{Proxy}` subject, since
/// §16.4 is proxy behaviour and a UA that takes a strict-routed request forwards
/// nothing at all.
pub struct StrictRouteRewriteHandledRule;

impl CrossMessageAuditRule for StrictRouteRewriteHandledRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::StrictRouteRewriteHandled.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Proxy])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::proxy::StrictRouteRewriteHandled, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::StrictRouteNotRewritten {
                branch,
                first_route,
                forwarded_request_uri,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let forwarded =
                if forwarded_request_uri.is_empty() { "<none>" } else { forwarded_request_uri };
            format!(
                "{{Proxy}} received strict-route request (callId {cid}, branch {branch}; first \
                 Route={first_route}) but outgoing Request-URI={forwarded} — RFC 3261 §16.4 / \
                 RFC3261-MUST-100"
            )
        })
    }
}

/// RFC 3261 §10.2 live: the merged `serial-register` rule
/// (`rfc_rules::rules::register`) at this bind's vantage — one binding change
/// for an address-of-record at a time.
pub struct SerialRegisterRule;

impl CrossMessageAuditRule for SerialRegisterRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::SerialRegister.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::register::SerialRegister, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::ConcurrentRegister {
                aor,
                branch,
                pending_branch,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent REGISTER (callId {cid}, branch {branch}) with new Contact while prior \
                 REGISTER (branch {pending_branch}) for AOR {aor} still pending — {{Uac}} RFC 3261 \
                 §10.2 / RFC3261-MUST-054"
            )
        })
    }
}

/// RFC 3261 §10.2 live: the merged `register-no-route-set` rule
/// (`rfc_rules::rules::register`) at this bind's vantage — a REGISTER states no
/// route set.
pub struct RegisterNoRouteSetRule;

impl CrossMessageAuditRule for RegisterNoRouteSetRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::RegisterNoRouteSet.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::register::RegisterNoRouteSet, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::RegisterCarriesRoute { .. }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent REGISTER (callId {cid}) carries Route header — {{Uac}} RFC 3261 §10.2 / \
                 RFC3261-MUST-051"
            )
        })
    }
}

/// RFC 3261 §14.2 live: the merged `concurrent-re-invite-500-or-491` rule
/// (`rfc_rules::rules::reinvite`) at this bind's vantage — the UAS serialises
/// the INVITE transactions of one dialog.
///
/// The live policy this bind adds to the rule body: none. A relay face that
/// takes two racing re-INVITEs and passes both on is itself the UAS of two
/// server transactions it never serialised, so it is judged like any other.
pub struct ConcurrentReInvite500Or491Rule;

impl CrossMessageAuditRule for ConcurrentReInvite500Or491Rule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ConcurrentReInvite500Or491.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::reinvite::ConcurrentReInvite500Or491, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::ConcurrentReInvite {
                branch,
                pending_invite_branch,
                answered_status,
                retry_after,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Took a concurrent re-INVITE (callId {cid}, branch {branch}) while a prior \
                 INVITE (branch {pending_invite_branch}) was still in progress — {{Uas}} must \
                 respond 491 or 500+Retry-After; got {answered_status}/RetryAfter={retry_after} \
                 (RFC 3261 §14.2)"
            )
        })
    }
}

/// RFC 3261 §15 live: the merged `no-bye-outside-or-early-dialog` rule
/// (`rfc_rules::rules::dialog`) at this bind's vantage — a BYE names a dialog,
/// and never an early one its sender is the callee of.
///
/// The live policy this bind adds to the rule body: none. A lane that relays a
/// BYE authored upstream carries the same defect on its own wire, and the
/// merged rule's peer-tag dialog key already keeps a face that saw the dialog
/// created from being charged for one it never did.
pub struct NoByeOutsideOrEarlyDialogRule;

impl CrossMessageAuditRule for NoByeOutsideOrEarlyDialogRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoByeOutsideOrEarlyDialog.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::dialog::NoByeOutsideOrEarlyDialog, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::ByeOffDialog { early_dialog, .. }) =
                &f.decision
            else {
                return String::new();
            };
            if *early_dialog {
                format!(
                    "Callee sent BYE on early dialog (callId {cid}) — should use CANCEL or \
                     4xx/5xx/6xx (RFC 3261 §15)"
                )
            } else {
                format!(
                    "Sent BYE (callId {cid}) outside any dialog — RFC 3261 §15 / \
                     RFC3261-MUST-089"
                )
            }
        })
    }
}

/// RFC 3261 §14.1 live: the merged `no-re-invite-while-invite-in-progress` rule
/// (`rfc_rules::rules::reinvite`) at this bind's vantage — one INVITE
/// outstanding per dialog direction.
///
/// The live policy this bind adds to the rule body: none. The merged rule's
/// ORDERED dialog key already spares a forwarding lane that relays both
/// parties' crossing re-INVITEs — that is legal glare, one requester per
/// direction, not one UAC overlapping itself.
pub struct NoReInviteWhileInviteInProgressRule;

impl CrossMessageAuditRule for NoReInviteWhileInviteInProgressRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoReInviteWhileInviteInProgress.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(
            events,
            &rfc_rules::rules::reinvite::NoReInviteWhileInviteInProgress,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::OverlappingReInvite {
                    branch,
                    prior_branch,
                    prior_accepted,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                let phase = if *prior_accepted {
                    "still Accepted — its 2xx not yet ACKed (RFC 6026)"
                } else {
                    "still in progress"
                };
                format!(
                    "Sent re-INVITE (callId {cid}, branch {branch}) while prior INVITE (branch \
                     {prior_branch}) {phase} — {{Uac}} RFC 3261 §14.1 / RFC3261-MUST-083"
                )
            },
        )
    }
}

/// RFC 3261 §16.7 live: the merged `proxy-100-within-grace` rule
/// (`rfc_rules::rules::proxy`) at this bind's vantage — a hop that cannot
/// answer promptly says it is trying.
///
/// The live policy this bind adds to the rule body: subject `{Proxy}`, because
/// §16.7 binds proxies and a UAS answering an INVITE is governed by §8.2.6; and
/// the finding is **advisory**, because a paused test clock lets a fixture
/// advance far past the grace in VIRTUAL time before it answers, which is no
/// real-world latency at all.
pub struct Proxy100WithinGraceRule;

impl CrossMessageAuditRule for Proxy100WithinGraceRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::Proxy100WithinGrace.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Proxy])
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::proxy::Proxy100WithinGrace, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::TryingNotSentInGrace {
                branch,
                first_final_after_us,
                grace_us,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let observed = match first_final_after_us {
                Some(us) => format!("first final after Δ={}ms", us / 1_000),
                None => "no response sent".to_string(),
            };
            format!(
                "{{Proxy}} did not emit 100 Trying within {}ms of INVITE receipt (callId {cid}, \
                 branch {branch}; {observed}) — RFC 3261 §16.7 / RFC3261-MUST-095",
                grace_us / 1_000,
            )
        })
    }
}

/// RFC 3261 §17.1.1.3 live: the merged `unacked-invite-non-2xx-final` rule
/// (`rfc_rules::rules::ack`) at this bind's vantage — a reject this bind sent
/// was never ACKed back to it.
///
/// The live policy this bind adds to the rule body: none, and it GATES. The
/// harness UAC carries the §17.1.1.3 client-transaction behaviour (its INVITE
/// agents auto-ACK any non-2xx final they surface), so an undischarged
/// obligation on the functional surface is a genuine defect: an unread reject,
/// a peer that never ACKs, or a final emitted and never delivered. A test that
/// deliberately models a peer which never ACKs waives it.
pub struct UnackedInviteNon2xxFinalRule;

impl CrossMessageAuditRule for UnackedInviteNon2xxFinalRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::UnackedInviteNon2xxFinal.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::ack::UnackedInviteNon2xxFinal, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::UnackedReject { status, branch, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent a non-2xx final {status} to an INVITE (callId {cid}, branch {branch}) that \
                 was never ACKed — the INVITE transaction never completes and the reject \
                 retransmits to Timer H; RFC 3261 §17.1.1.3 makes the ACK mandatory \
                 (hop-by-hop, so a proxy in the path owes this UAS its own synthesized ACK)"
            )
        })
    }
}

/// RFC 3261 §14.1 live: the merged `failed-reinvite-tears-down-dialog` rule
/// (`rfc_rules::rules::reinvite`) at this bind's vantage — a re-INVITE that
/// drew a provisional drew a final too.
///
/// The live policy this bind adds to the rule body: none. The rule judges the
/// transaction a lane OPENED, and a lane that relays a re-INVITE opens its own
/// client transaction on its own branch — the abandoned one on its wire is its
/// own to answer for.
pub struct FailedReinviteTearsDownDialogRule;

impl CrossMessageAuditRule for FailedReinviteTearsDownDialogRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::FailedReinviteTearsDownDialog.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::reinvite::FailedReinviteTearsDownDialog, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::AbandonedReInvite {
                branch,
                provisional_status,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "re-INVITE (in-dialog INVITE, callId {cid}, branch {branch}) received a \
                 {provisional_status} provisional but never a final response, and the dialog was \
                 never BYE'd — the prior dialog state was silently torn down on a failed \
                 re-INVITE; RFC 3261 §14.1 requires a failed re-INVITE to leave the dialog in \
                 its prior state (and §17.1.1.2 a final response)"
            )
        })
    }
}

/// RFC 3261 §13.3.1.1 / §17.2.1 live: the merged `no-1xx-after-final` rule
/// (`rfc_rules::rules::final_response`) at this bind's vantage — a completed
/// server transaction emits no further provisionals.
///
/// The live policy this bind adds to the rule body: subject `{Uas}`, plus a
/// relay-lane skip — the same policy its one-final sibling carries, for the same
/// reason. A B2BUA face forwards its upstream's 1xx rather than originating it,
/// so a late provisional on a relay lane is the upstream's emission and is
/// flagged on the upstream lane.
pub struct No1xxAfterFinalRule;

impl CrossMessageAuditRule for No1xxAfterFinalRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::No1xxAfterFinal.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Uas])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::final_response::No1xxAfterFinal, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::LateProvisional {
                status,
                completed_by_status,
                branch,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent a new {status} provisional on an INVITE server transaction (callId {cid}, \
                 branch {branch}) after its {completed_by_status} final — a completed \
                 transaction emits no further provisionals (RFC 3261 §13.3.1.1 / §17.2.1)"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3262 §3 live: the merged `require-reliable-1xx-on-require` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — a UAS handed
/// `Require: 100rel` answers reliably or rejects the extension.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, because
/// the rule judges what a lane AUTHORED. A lane that forwards both directions
/// of one Call-ID passes the upstream UAS's provisional through, so a plain 18x
/// there is the upstream's and is flagged on the upstream lane. The skip has
/// RECORDING granularity, not per-call.
pub struct RequireReliable1xxOnRequireRule;

impl CrossMessageAuditRule for RequireReliable1xxOnRequireRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::RequireReliable1xxOnRequire.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::prack::RequireReliable1xxOnRequire, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::Unreliable1xx { status, branch, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "INVITE required 100rel (callId {cid}, branch {branch}) but sent 1xx response \
                 {status} lacks Require:100rel/RSeq and no 420 Unsupported:100rel was sent — RFC \
                 3262 §3 / RFC3262-MUST-001/-002"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3262 §3 live: the merged `reliable-needs-client-opt-in` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — the PRACK machinery is
/// negotiated, not imposed.
///
/// The live policy this bind adds to the rule body: subject `{Uas}`; a
/// relay-lane skip; and the finding is **advisory**, because a B2BUA worker may
/// terminate PRACK on one leg under a `100rel` the OTHER leg's INVITE offered —
/// the downstream INVITE this rule sees need not carry it.
pub struct ReliableNeedsClientOptInRule;

impl CrossMessageAuditRule for ReliableNeedsClientOptInRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ReliableNeedsClientOptIn.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Uas])
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::prack::ReliableNeedsClientOptIn, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::UnsolicitedReliable1xx {
                status,
                branch,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent reliable 1xx (status {status}, callId {cid}, branch {branch}) — matching \
                 INVITE neither Supported:100rel nor Require:100rel (RFC 3262 §3 / \
                 RFC3262-MUST-004)"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3262 §3 live: the merged `no-reliable-1xx-on-in-dialog` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — the PRACK machinery is
/// scoped to the INVITE method, a re-INVITE included.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, for the
/// reason its `Require`-honouring sibling above carries.
pub struct NoReliable1xxOnInDialogRule;

impl CrossMessageAuditRule for NoReliable1xxOnInDialogRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoReliable1xxOnInDialog.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::prack::NoReliable1xxOnInDialog, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::InDialogReliable1xx {
                status,
                method,
                branch,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent reliable 1xx on a non-INVITE request (status {status}, callId {cid}, \
                 branch {branch}, method {method}) — forbidden per RFC 3262 §3 / \
                 RFC3262-MUST-005"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3262 §3 live: the merged `unmatched-prack-proxied` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — a PRACK matching
/// nothing local is forwarded, not absorbed.
///
/// The live policy this bind adds to the rule body: subject `{Proxy}`; a
/// relay-lane skip; and the finding is **advisory**, because the B2BUA worker
/// terminates PRACK per leg. The provisional that drew the PRACK went out on
/// the leg-mate's Call-ID, so the PRACK reads unmatched from this leg's view.
pub struct UnmatchedPrackProxiedRule;

impl CrossMessageAuditRule for UnmatchedPrackProxiedRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::UnmatchedPrackProxied.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Proxy])
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::prack::UnmatchedPrackProxied, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::PrackAbsorbed {
                rack_rseq,
                rack_cseq,
                rack_method,
                known_rseqs,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let known = match known_rseqs.as_slice() {
                [] => "none".to_string(),
                seqs => seqs.iter().map(u64::to_string).collect::<Vec<_>>().join(", "),
            };
            format!(
                "Received PRACK with RAck={rack_rseq} {rack_cseq} {rack_method} on proxy bind \
                 (callId {cid}) — no matching reliable 1xx taken on this bind (RSeq taken: \
                 {known}) AND no outgoing PRACK observed (proxy must forward unmatched PRACKs, \
                 not absorb) — RFC 3262 §3 / RFC3262-MUST-006"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3262 §3 live: the merged `prack-2xx-or-481` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — the PRACK's answer is
/// the UAS's own `RSeq` state read back.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, because
/// a lane that forwards both directions of one Call-ID relays the upstream
/// UAS's answer to the PRACK rather than authoring it.
pub struct Prack2xxOr481Rule;

impl CrossMessageAuditRule for Prack2xxOr481Rule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::Prack2xxOr481.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::prack::Prack2xxOr481, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::PrackAnsweredWrongly {
                status,
                rack_rseq,
                rack_matched,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            match rack_matched {
                true => format!(
                    "Received PRACK with RAck.response-num {rack_rseq} names a reliable 1xx this \
                     agent sent (RSeq, INVITE CSeq and method all match), but agent responded \
                     {status} instead of 2xx (callId {cid}) — RFC 3262 §3 / RFC3262-MUST-009"
                ),
                false => format!(
                    "Received PRACK with RAck.response-num {rack_rseq} names NO reliable 1xx this \
                     agent sent (RSeq, INVITE CSeq or method differ), but agent responded \
                     {status} instead of 481 (callId {cid}) — RFC 3262 §3 / RFC3262-MUST-010"
                ),
            }
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3262 §3 live: the merged `delay-2xx-on-unacked-reliable-1xx-with-sdp`
/// rule (`rfc_rules::rules::prack`) at this bind's vantage — the 2xx waits for
/// the PRACK of an offer sent reliably.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, because
/// a lane that forwards both directions of one Call-ID relays the upstream
/// UAS's 2xx at the upstream's timing.
pub struct Delay2xxOnUnackedReliable1xxWithSdpRule;

impl CrossMessageAuditRule for Delay2xxOnUnackedReliable1xxWithSdpRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::Delay2xxOnUnackedReliable1xxWithSdp.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(
            events,
            &rfc_rules::rules::prack::Delay2xxOnUnackedReliable1xxWithSdp,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::AnsweredOverUnackedOffer {
                    unacked_rseqs,
                    branch,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                let rseqs =
                    unacked_rseqs.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
                format!(
                    "Sent 2xx INVITE response while reliable 1xx (RSeq={rseqs}) with SDP still \
                     unacked (callId {cid}, branch {branch}) — RFC 3262 §3 / RFC3262-MUST-014"
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3262 §3 live: the merged `prack-accepted-after-final` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — the PRACK's own server
/// transaction outlives the INVITE's.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, for the
/// reason its `prack-2xx-or-481` sibling carries.
pub struct PrackAcceptedAfterFinalRule;

impl CrossMessageAuditRule for PrackAcceptedAfterFinalRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::PrackAcceptedAfterFinal.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::prack::PrackAcceptedAfterFinal, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::LatePrackRejected {
                status,
                branch,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Received PRACK after final INVITE response was sent (callId {cid}, PRACK branch \
                 {branch}) but PRACK got {status} instead of 2xx — RFC 3262 §3 / RFC3262-MUST-015"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3262 §3 live: the merged `no-new-reliable-1xx-after-final` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — no fresh `RSeq` on a
/// transaction that has answered.
///
/// The live policy this bind adds to the rule body: a relay-lane skip — the
/// same policy its general §17.2.1 sibling [`No1xxAfterFinalRule`] carries, for
/// the same reason.
pub struct NoNewReliable1xxAfterFinalRule;

impl CrossMessageAuditRule for NoNewReliable1xxAfterFinalRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoNewReliable1xxAfterFinal.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::prack::NoNewReliable1xxAfterFinal, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::Reliable1xxAfterFinal { rseq, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent new reliable 1xx (RSeq={rseq}, callId {cid}) after final INVITE response — \
                 forbidden per RFC 3262 §3 / RFC3262-MUST-016"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3262 §4 live: the merged `no-prack-of-100-trying` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — a 100 Trying is never
/// PRACKed, whatever markers it carries.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, because
/// a lane that forwards both directions of one Call-ID passes the upstream
/// UAC's PRACK through rather than authoring it.
pub struct NoPrackOf100TryingRule;

impl CrossMessageAuditRule for NoPrackOf100TryingRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoPrackOf100Trying.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::prack::NoPrackOf100Trying, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::PrackedTrying { rseq, .. }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent PRACK references RSeq {rseq} from a received 100 Trying carrying \
                 Require:100rel (callId {cid}) — UAC MUST ignore 100rel on 100 Trying (RFC 3262 \
                 §4 / RFC3262-MUST-019)"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3262 §5 live: the merged `prack-answers-1xx-offer` rule
/// (`rfc_rules::rules::prack`) at this bind's vantage — the PRACK of an offer
/// carries the answer.
///
/// The live policy this bind adds to the rule body: subject `{Uac, Uas}`; a
/// relay-lane skip; the finding is surfaced at BOTH ends, because the occasion
/// is one negotiation and the party that sent the PRACK need not be a recorded
/// bind at all; and it is **advisory**, because a genuine reliable-1xx offer and
/// its PRACK answer can straddle two legs of a B2BUA, which rewrites the
/// Call-ID between them.
pub struct PrackAnswers1xxOfferRule;

impl CrossMessageAuditRule for PrackAnswers1xxOfferRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::PrackAnswers1xxOffer.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Uac, UaRole::Uas])
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(events, &rfc_rules::rules::prack::PrackAnswers1xxOffer, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::PrackWithoutAnswer { rack_rseq, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "PRACK for reliable-1xx-with-offer (RSeq={rack_rseq}, callId {cid}) carries no \
                 body — RFC 3262 §5 / RFC3262-MUST-025"
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §13.2.2.4 live: the merged `ack-body-after-complete-offer-answer`
/// rule (`rfc_rules::rules::offer_answer`) at this bind's vantage — an ACK
/// closing a finished round carries no body.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, because
/// the rule judges what a lane AUTHORED. A lane that forwards both directions
/// of one Call-ID carries both agents' descriptions and has no offer/answer
/// state of its own, so a stray ACK body there is the upstream's and is flagged
/// on the upstream lane. The skip has RECORDING granularity, not per-call.
pub struct AckBodyAfterCompleteOfferAnswerRule;

impl CrossMessageAuditRule for AckBodyAfterCompleteOfferAnswerRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::AckBodyAfterCompleteOfferAnswer.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(
            events,
            &rfc_rules::rules::offer_answer::AckBodyAfterCompleteOfferAnswer,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::AckBodyOnClosedRound {
                    streams,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                let table =
                    if streams.is_empty() { "no m= line".to_string() } else { streams.join(", ") };
                format!(
                    "Sent an ACK (callId {cid}, CSeq {cseq}) carrying a session description \
                     ({table}) although the offer/answer exchange it acknowledges was already \
                     complete — expected no body on this ACK (Content-Length: 0), RFC 3261 \
                     §13.2.2.4 / §13.2.1",
                    cseq = f.cseq,
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §13.2.1 live: the merged `final-2xx-answers-the-offer` rule
/// (`rfc_rules::rules::offer_answer`) — the 2xx to a request that carried an
/// offer carries the answer, unless a reliable provisional already stated one.
///
/// The live policy this bind adds to the rule body: surfaced at either end,
/// because the negotiation is two-party and an answerer that is not a recorded
/// bind is still named on the lane that took its silence; plus the relay-lane
/// skip its ACK-body sibling above carries.
pub struct Final2xxAnswersTheOfferRule;

impl CrossMessageAuditRule for Final2xxAnswersTheOfferRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::Final2xxAnswersTheOffer.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(
            events,
            &rfc_rules::rules::offer_answer::Final2xxAnswersTheOffer,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::OfferLeftUnanswered {
                    status,
                    offered_streams,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                let table = if offered_streams.is_empty() {
                    "no m= line".to_string()
                } else {
                    offered_streams.join(", ")
                };
                format!(
                    "Sent a {status} (callId {cid}, CSeq {cseq}) carrying no session \
                     description although the request it answers offered one ({table}) and no \
                     reliable message had answered it — expected the answer in this final, RFC \
                     3261 §13.2.1 / RFC 3264 §6",
                    cseq = f.cseq,
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3261 §13.2.1 live: the merged `second-answer-repeats-the-first` rule
/// (`rfc_rules::rules::offer_answer`) — one dialog carries one answer, and a
/// later description on it re-states that answer's transport plan.
///
/// The live policy this bind adds to the rule body: surfaced at either end,
/// because the negotiation is two-party and an answerer that is not a recorded
/// bind is still named on the lane that took it; plus the relay-lane skip its
/// ACK-body sibling above carries.
pub struct SecondAnswerRepeatsTheFirstRule;

impl CrossMessageAuditRule for SecondAnswerRepeatsTheFirstRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::SecondAnswerRepeatsTheFirst.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(
            events,
            &rfc_rules::rules::offer_answer::SecondAnswerRepeatsTheFirst,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::SecondAnswerDiverged {
                    first_plan,
                    second_plan,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                format!(
                    "Sent a second session description on one dialog (callId {cid}, CSeq \
                     {cseq}) stating a transport plan its first answer did not — first \
                     [{first}], second [{second}]; the peer takes the first answer and ignores \
                     every later description on that dialog (RFC 3261 §13.2.1)",
                    cseq = f.cseq,
                    first = first_plan.join(", "),
                    second = second_plan.join(", "),
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3264 §6 live: the merged `answer-stream-matches-offer` rule
/// (`rfc_rules::rules::offer_answer`) at this bind's vantage — an accepted
/// stream keeps the offer's media type and transport.
///
/// The live policy this bind adds to the rule body: a relay-lane skip, for the
/// reason its ACK-body sibling above carries.
pub struct AnswerStreamMatchesOfferRule;

impl CrossMessageAuditRule for AnswerStreamMatchesOfferRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::AnswerStreamMatchesOffer.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::offer_answer::AnswerStreamMatchesOffer, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::AnswerStreamRetyped {
                stream_indexes,
                offered,
                answered,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            // Every re-typed stream is named once, on this one finding.
            let rows: Vec<String> = stream_indexes
                .iter()
                .zip(offered.iter().zip(answered.iter()))
                .map(|(i, (o, a))| format!("m=[{i}] offered \"{o}\", answered \"{a}\""))
                .collect();
            format!(
                "Answered the offer on CSeq {cseq} re-typing {n} stream(s) — {rows} (callId \
                 {cid}); an answer keeps each stream's media type and proto (port 0 to reject \
                 it) — RFC 3264 §6",
                cseq = f.cseq,
                n = stream_indexes.len(),
                rows = rows.join("; "),
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 4566 §5.2 / RFC 3264 §8 live: the merged `sdp-origin-continuity` rule
/// (`rfc_rules::rules::offer_answer`) at this bind's vantage — every
/// description a lane sends on one call describes the same session.
///
/// The live policy this bind adds to the rule body: subject `{Uac, Uas}` plus a
/// relay-lane skip, because origin continuity is a per-ORIGINATOR invariant — a
/// transparent proxy's sent stream legitimately interleaves both agents'
/// origins on one Call-ID. And the finding is **advisory**: a B2BUA re-offers
/// under an origin of its own, and the shared SDP fixtures are constants
/// several agents reuse, so the rule fires on fixture data as much as on stack
/// behaviour. Gating waits on one session per agent in the fixtures.
pub struct SdpOriginContinuityRule;

impl CrossMessageAuditRule for SdpOriginContinuityRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::SdpOriginContinuity.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Uac, UaRole::Uas])
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(events, &rfc_rules::rules::offer_answer::SdpOriginContinuity, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::SdpOriginDiverged {
                origin_line,
                prior_origin_line,
                same_session,
                body_changed,
                session_version,
                prior_session_version,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let offence = if !same_session {
                "it describes a different session — a later description keeps the o= identity \
                 (username, sess-id, nettype, addrtype, address) and moves only sess-version"
                    .to_string()
            } else if *body_changed {
                format!(
                    "the description changed but sess-version went {prior_session_version} → \
                     {session_version} (expected exactly {expected})",
                    expected = prior_session_version.saturating_add(1),
                )
            } else {
                format!(
                    "the description is byte-identical but sess-version went \
                     {prior_session_version} → {session_version} (expected it unchanged)"
                )
            };
            format!(
                "Sent \"{origin_line}\" after \"{prior_origin_line}\" on the same call (callId \
                 {cid}, CSeq {cseq}) — {offence} — RFC 4566 §5.2 / RFC 3264 §8",
                cseq = f.cseq,
            )
        })
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// The `m=[i] …` rows a collapsed per-stream finding names, one per offending
/// stream position, rendered by `row(i, a, b)` over three parallel vectors.
fn stream_rows(
    indexes: &[usize],
    left: &[String],
    right: &[String],
    row: impl Fn(usize, &str, &str) -> String,
) -> String {
    indexes
        .iter()
        .zip(left.iter().zip(right.iter()))
        .map(|(i, (l, r))| row(*i, l, r))
        .collect::<Vec<_>>()
        .join("; ")
}

/// RFC 3264 §5 live: the merged `no-new-offer-while-offer-pending` rule
/// (`rfc_rules::rules::offer_answer`) — an agent holds one offer of its own
/// outstanding at a time.
///
/// The live policy this bind adds to the rule body: surfaced at EITHER END,
/// since the glare is one negotiation and both lanes are places the recording
/// checks it (the taker-side reading of the same act is the old MUST-001, the
/// sender-side one MUST-002 — one obligation, charged to the sender); the
/// finding is **advisory**, because a B2BUA legitimately re-offers on one leg
/// before the prior answer arrives on the other; plus the relay-lane skip.
pub struct NoNewOfferWhileOfferPendingRule;

impl CrossMessageAuditRule for NoNewOfferWhileOfferPendingRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoNewOfferWhileOfferPending.token()
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(
            events,
            &rfc_rules::rules::offer_answer::NoNewOfferWhileOfferPending,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::OfferWhilePending {
                    pending_offer_cseq,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                format!(
                    "Sent an SDP offer (callId {cid}, CSeq {cseq}) while its own offer on CSeq \
                     {pending_offer_cseq} was still unanswered — an offer is answered before the \
                     next one goes out (RFC 3264 §5 / RFC3264-MUST-002)",
                    cseq = f.cseq,
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3264 §6 live: the merged `answer-m-line-count-matches-offer` rule
/// (`rfc_rules::rules::offer_answer`) — the answer holds one `m=` line per
/// offered stream.
///
/// The live policy this bind adds to the rule body: surfaced at EITHER END,
/// since the offer/answer pair is one negotiation and the answerer is not
/// always a recorded bind; plus the relay-lane skip.
pub struct AnswerMLineCountMatchesOfferRule;

impl CrossMessageAuditRule for AnswerMLineCountMatchesOfferRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::AnswerMLineCountMatchesOffer.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(
            events,
            &rfc_rules::rules::offer_answer::AnswerMLineCountMatchesOffer,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::AnswerMLineCountDiffers {
                    offer_m_lines,
                    answer_m_lines,
                    offered_streams,
                    answered_streams,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                format!(
                    "Answer SDP m= count ({answer_m_lines}) differs from offer m= count \
                     ({offer_m_lines}) (callId {cid}, CSeq {cseq}) — offered [{offered}], answered \
                     [{answered}]; a rejected stream keeps its slot at port 0 (RFC 3264 §6 / \
                     RFC3264-MUST-018)",
                    cseq = f.cseq,
                    offered = offered_streams.join(", "),
                    answered = answered_streams.join(", "),
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3264 §6 live: the merged `answer-t-line-equals-offer` rule
/// (`rfc_rules::rules::offer_answer`) — the answer repeats the offer's `t=`.
///
/// The live policy this bind adds to the rule body: surfaced at either end,
/// plus the relay-lane skip — its m-line-count sibling's reasons.
pub struct AnswerTLineEqualsOfferRule;

impl CrossMessageAuditRule for AnswerTLineEqualsOfferRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::AnswerTLineEqualsOffer.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(
            events,
            &rfc_rules::rules::offer_answer::AnswerTLineEqualsOffer,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::AnswerTLineDiffers {
                    offer_t_line,
                    answer_t_line,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                format!(
                    "Answer SDP t= line ('{answer_t_line}') differs from offer t= line \
                     ('{offer_t_line}') (callId {cid}, CSeq {cseq}) — RFC 3264 §6 / \
                     RFC3264-MUST-019",
                    cseq = f.cseq,
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3264 §6.1 live: the merged `answer-media-type-matches-offer` rule
/// (`rfc_rules::rules::offer_answer`) — streams pair by position.
///
/// The live policy this bind adds to the rule body: surfaced at either end,
/// plus the relay-lane skip.
pub struct AnswerMediaTypeMatchesOfferRule;

impl CrossMessageAuditRule for AnswerMediaTypeMatchesOfferRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::AnswerMediaTypeMatchesOffer.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(
            events,
            &rfc_rules::rules::offer_answer::AnswerMediaTypeMatchesOffer,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::AnswerMediaTypeMismatched {
                    stream_indexes,
                    offered_types,
                    answered_types,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                // Every mis-typed position is named once, on this one finding.
                let rows = stream_rows(stream_indexes, offered_types, answered_types, |i, o, a| {
                    format!("m=[{i}] offered '{o}', answered '{a}'")
                });
                format!(
                    "Answer media type does not match the offer's at {n} stream position(s) — \
                     {rows} (callId {cid}, CSeq {cseq}) — RFC 3264 §6.1 / RFC3264-MUST-022",
                    n = stream_indexes.len(),
                    cseq = f.cseq,
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3264 §6.1 live: the merged `direction-pair-valid` rule
/// (`rfc_rules::rules::offer_answer`) — the answer's direction is one the
/// offer's admits.
///
/// The live policy this bind adds to the rule body: surfaced at either end; the
/// finding is **advisory**, because a B2BUA translates direction across legs as
/// policy (forcing `sendrecv` on one leg while the other holds); plus the
/// relay-lane skip.
pub struct DirectionPairValidRule;

impl CrossMessageAuditRule for DirectionPairValidRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::DirectionPairValid.token()
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(
            events,
            &rfc_rules::rules::offer_answer::DirectionPairValid,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::DirectionPairInvalid {
                    stream_indexes,
                    offered_directions,
                    answered_directions,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                let rows = stream_rows(
                    stream_indexes,
                    offered_directions,
                    answered_directions,
                    |i, o, a| format!("m=[{i}] answer '{a}' invalid for offer '{o}'"),
                );
                format!(
                    "Answer direction invalid for the offer's at {n} stream position(s) — {rows} \
                     (callId {cid}, CSeq {cseq}) — RFC 3264 §6.1 / RFC3264-MUST-023",
                    n = stream_indexes.len(),
                    cseq = f.cseq,
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3264 §6 live: the merged `rejected-stream-minimal-answer` rule
/// (`rfc_rules::rules::offer_answer`) — a port-0 rejection still lists a
/// format.
///
/// The live policy this bind adds to the rule body: surfaced at either end,
/// plus the relay-lane skip.
pub struct RejectedStreamMinimalAnswerRule;

impl CrossMessageAuditRule for RejectedStreamMinimalAnswerRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::RejectedStreamMinimalAnswer.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(
            events,
            &rfc_rules::rules::offer_answer::RejectedStreamMinimalAnswer,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::RejectedStreamWithoutFormat {
                    stream_indexes,
                    rejected_rows,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                let rows = stream_indexes
                    .iter()
                    .zip(rejected_rows.iter())
                    .map(|(i, r)| format!("m=[{i}] \"{r}\""))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!(
                    "Rejected stream(s) (port=0) carry no media format token — {rows} (callId \
                     {cid}, CSeq {cseq}) — RFC 3264 §6 / RFC3264-MUST-021",
                    cseq = f.cseq,
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3264 §8 live: the merged `re-offer-m-line-count-monotonic` rule
/// (`rfc_rules::rules::offer_answer`) at this bind's vantage — the stream table
/// a lane states only ever grows.
///
/// The live policy this bind adds to the rule body: the rule judges what a lane
/// SENT and nothing else, so the vantage charge is exact; plus the relay-lane
/// skip.
pub struct ReOfferMLineCountMonotonicRule;

impl CrossMessageAuditRule for ReOfferMLineCountMonotonicRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ReOfferMLineCountMonotonic.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced(
            events,
            &rfc_rules::rules::offer_answer::ReOfferMLineCountMonotonic,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::ReOfferStreamsDropped {
                    m_lines,
                    prior_m_lines,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                format!(
                    "Re-offer m= count {m_lines} decreased from prior offer {prior_m_lines} — \
                     streams must keep their slot (port=0) (callId {cid}, CSeq {cseq}) — RFC 3264 \
                     §8 / RFC3264-MUST-042",
                    cseq = f.cseq,
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3264 §8 live: the merged `zero-port-propagation` rule
/// (`rfc_rules::rules::offer_answer`) — a stream offered at port 0 is answered
/// at port 0.
///
/// The live policy this bind adds to the rule body: surfaced at either end; the
/// finding is **advisory**, because a B2BUA anchors media per leg and assigns
/// its own ports, so a peer-side disabled stream can legitimately reappear with
/// the B2BUA's port on the other leg; plus the relay-lane skip.
pub struct ZeroPortPropagationRule;

impl CrossMessageAuditRule for ZeroPortPropagationRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ZeroPortPropagation.token()
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(
            events,
            &rfc_rules::rules::offer_answer::ZeroPortPropagation,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::ZeroPortResurrected {
                    stream_indexes,
                    answered_ports,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                let rows = stream_indexes
                    .iter()
                    .zip(answered_ports.iter())
                    .map(|(i, p)| format!("m=[{i}] answered port={p}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!(
                    "Offer disabled {n} stream(s) with port=0 that the answer re-enabled — {rows} \
                     (callId {cid}, CSeq {cseq}) — RFC 3264 §8 / RFC3264-MUST-044",
                    n = stream_indexes.len(),
                    cseq = f.cseq,
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

/// RFC 3264 §8.3.2 live: the merged `payload-type-mapping-stable` rule
/// (`rfc_rules::rules::offer_answer`) — a payload type keeps the encoding its
/// sender first bound it to.
///
/// The live policy this bind adds to the rule body: surfaced at either end,
/// since the peer caches the binding and a remapping sender is not always a
/// recorded bind; plus the relay-lane skip.
pub struct PayloadTypeMappingStableRule;

impl CrossMessageAuditRule for PayloadTypeMappingStableRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::PayloadTypeMappingStable.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let relays = relay_lanes(events);
        surfaced_at_either_end(
            events,
            &rfc_rules::rules::offer_answer::PayloadTypeMappingStable,
            |f, cid| {
                let Decision::Violated(rfc_rules::Evidence::PayloadTypeRemapped {
                    payload_types,
                    prior_encodings,
                    encodings,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                let rows = payload_types
                    .iter()
                    .zip(prior_encodings.iter().zip(encodings.iter()))
                    .map(|(pt, (prev, now))| {
                        format!("payload-type {pt} was '{prev}' now '{now}'")
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                format!(
                    "Remapped {n} payload-type binding(s) it had already stated on this call — \
                     {rows} (callId {cid}, CSeq {cseq}); the peer decodes by the cached mapping — \
                     RFC 3264 §8.3.2 / RFC3264-MUST-047",
                    n = payload_types.len(),
                    cseq = f.cseq,
                )
            },
        )
        .into_iter()
        .filter(|(lane, _, _, _)| !relays.contains(lane))
        .collect()
    }
}

// ---------------------------------------------------------------------------
// The PEER seam: per-message obligations, one message at one vantage
// ---------------------------------------------------------------------------
//
// A per-message rule decides ONE message, so its vantage policy is the plain
// reading of the two the adapter already has. A rule whose subject the sender
// MINTS is surfaced at the emitter — the bind is charged for what it wrote, and
// a message it merely took carries its peer's `emitter` and drops out. A rule
// that correlates a message to the state THIS bind built is surfaced at the
// taker — the bind is the vantage, the far party is charged, and the recording
// is the only place that party's stream is checked at all.
//
// Two things the predecessor peer trait supplied come from the merged model
// instead: subject dispatch and the advisory flag stay per-rule below (the
// cross-message trait carries both), and the offending wire position — which a
// peer finding could never state — now falls out of the finding's anchor.

/// RFC 3261 §8.1.1.7 live: the merged `branch-prefix` rule at this bind's
/// vantage — the sender mints the branch, so the bind is charged for what it
/// emitted.
pub struct BranchPrefixRule;

impl CrossMessageAuditRule for BranchPrefixRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::BranchPrefix.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::BranchPrefix, |f, _cid| {
            match &f.decision {
                Decision::Violated(rfc_rules::Evidence::HeaderValueRejected {
                    on, value, ..
                }) => format!(
                    "{on} top Via branch \"{value}\" does not begin with the RFC 3261 magic \
                     cookie \"z9hG4bK\" (§8.1.1.7) — a downstream element cannot treat it as an \
                     RFC-3261 transaction id"
                ),
                Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) => {
                    format!(
                        "{on} has no top Via branch parameter (RFC 3261 §8.1.1.7 requires a \
                         \"z9hG4bK\"-prefixed branch on every request)"
                    )
                }
                _ => String::new(),
            }
        })
    }
}

/// RFC 3261 §8.1.1.6 live: the merged `max-forwards` rule at this bind's
/// vantage — the sender writes the header, so the bind is charged.
pub struct MaxForwardsRule;

impl CrossMessageAuditRule for MaxForwardsRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::MaxForwards.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::MaxForwards, |f, _cid| {
            match &f.decision {
                Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) => {
                    format!(
                        "{on} request is missing Max-Forwards — RFC 3261 §8.1.1.6 requires it on \
                         every request (a real downstream element cannot loop-protect this hop)"
                    )
                }
                Decision::Violated(rfc_rules::Evidence::HeaderValueRejected {
                    on,
                    value,
                    expected,
                    ..
                }) if expected == "at most 70" => format!(
                    "{on} Max-Forwards is {value}, exceeds 70 — RFC 3261 §8.1.1.6 (70 is the \
                     recommended initial value; a higher count was minted, not decremented)"
                ),
                Decision::Violated(rfc_rules::Evidence::HeaderValueRejected {
                    on, value, ..
                }) => format!(
                    "{on} has an invalid Max-Forwards value \"{value}\" — RFC 3261 §8.1.1.6 \
                     requires an integer in 0..=255"
                ),
                _ => String::new(),
            }
        })
    }
}

/// RFC 3261 §20.14 live: the merged `content-length` rule at this bind's
/// vantage — the sender writes header and body together.
pub struct ContentLengthRule;

impl CrossMessageAuditRule for ContentLengthRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ContentLength.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::ContentLength, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::HeaderValueRejected {
                value,
                expected,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "Content-Length mismatch: header says {value} but body is {expected} bytes — RFC \
                 3261 §20.14 (a strict peer truncates or desyncs on this)"
            )
        })
    }
}

/// RFC 3261 §7.4.1 live: the merged `content-type` rule at this bind's vantage.
pub struct ContentTypeRule;

impl CrossMessageAuditRule for ContentTypeRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ContentType.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::ContentType, |f, _cid| {
            if !f.violated() {
                return String::new();
            }
            "message carries a body but no Content-Type — RFC 3261 §7.4.1 requires Content-Type \
             whenever a body is present (the peer cannot interpret the body)"
                .to_string()
        })
    }
}

/// RFC 3261 §8.1.1.8 live: the merged `contact-presence` rule at this bind's
/// vantage.
pub struct ContactPresenceRule;

impl CrossMessageAuditRule for ContactPresenceRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ContactPresence.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::ContactPresence, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "{on} request is missing Contact — RFC 3261 §8.1.1.8 requires it on \
                 dialog-establishing methods (the dialog's remote target is undefined)"
            )
        })
    }
}

/// RFC 3261 §15.1 live: the merged `no-contact-on-bye` rule at this bind's
/// vantage.
pub struct NoContactOnByeRule;

impl CrossMessageAuditRule for NoContactOnByeRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoContactOnBye.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::NoContactOnBye, |f, _cid| {
            if !f.violated() {
                return String::new();
            }
            "BYE carries Contact — RFC 3261 §15.1 (BYE terminates the dialog, so target-refresh \
             has no meaning)"
                .to_string()
        })
    }
}

/// RFC 3261 §8.2.6.2 live: the merged `to-tag-presence` rule at this bind's
/// vantage — the responding UAS mints the tag.
pub struct ToTagPresenceRule;

impl CrossMessageAuditRule for ToTagPresenceRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ToTagPresence.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::ToTagPresence, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "{on} response is missing a To-tag — RFC 3261 §8.2.6.2 requires a UAS to add a \
                 To-tag to every response above 100 (the peer cannot dialog-match it)"
            )
        })
    }
}

/// RFC 3261 §16.6 live: the merged `no-record-route-from-ua` rule at this
/// bind's vantage.
///
/// The live policy this bind adds to the rule body: subject `{Proxy}`, so only a
/// lane that may be mistaken for a proxy is judged at all.
pub struct NoRecordRouteFromUaRule;

impl CrossMessageAuditRule for NoRecordRouteFromUaRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoRecordRouteFromUa.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Proxy])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::NoRecordRouteFromUa, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent {
                value, ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "B2BUA inserted Record-Route in a request — a B2BUA is a UA and MUST NOT use \
                 Record-Route (RFC 3261 §16.6). Found: {value}"
            )
        })
    }
}

/// RFC 3261 §8.1.3 / §17.1.3 live: the merged `response-echoes-request-via`
/// rule at this bind's vantage — surfaced at the TAKER, the UAC whose client
/// transaction the response fails to name.
pub struct ResponseEchoesRequestViaRule;

impl CrossMessageAuditRule for ResponseEchoesRequestViaRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ResponseEchoesRequestVia.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced_at_taker(
            events,
            &rfc_rules::rules::correlation::ResponseEchoesRequestVia,
            |f, _cid| {
                let Decision::Violated(rfc_rules::Evidence::ResponseViaDiverged {
                    response_branch,
                    request_branch,
                    response_vias,
                    request_vias,
                    ..
                }) = &f.decision
                else {
                    return String::new();
                };
                let mut parts = Vec::new();
                if response_branch != request_branch {
                    parts.push(format!(
                        "response top Via branch \"{response_branch}\" differs from the branch \
                         this bind sent \"{request_branch}\" (the response cannot be matched to \
                         its client transaction)"
                    ));
                }
                if response_vias != request_vias {
                    parts.push(format!(
                        "response carries {response_vias} Via header(s) but the sent request had \
                         {request_vias}"
                    ));
                }
                format!(
                    "{} — RFC 3261 §8.1.3 requires the response to echo the request's Via stack \
                     unchanged",
                    parts.join("; "),
                )
            },
        )
    }
}

/// RFC 3261 §8.1.3.3 live: the merged `response-correlation` rule at this
/// bind's vantage — surfaced at the TAKER, the party handed the phantom answer.
pub struct ResponseCorrelationRule;

impl CrossMessageAuditRule for ResponseCorrelationRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::ResponseCorrelation.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced_at_taker(events, &rfc_rules::rules::correlation::ResponseCorrelation, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::ResponseCseqPhantom {
                response_cseq,
                response_method,
                sent_cseqs,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "response CSeq {response_cseq} {response_method} does not echo any sent \
                 {response_method} CSeq [{nums}] — RFC 3261 §8.1.3.3 (the peer is responding to a \
                 phantom request)",
                nums = sent_cseqs.iter().map(u32::to_string).collect::<Vec<_>>().join(", "),
            )
        })
    }
}

/// RFC 3261 §12.2.1.1 live: the merged `mid-dialog-tags` rule at this bind's
/// vantage — surfaced at the TAKER, whose dialog the message fails to name.
pub struct MidDialogTagsRule;

impl CrossMessageAuditRule for MidDialogTagsRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::MidDialogTags.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced_at_taker(events, &rfc_rules::rules::correlation::MidDialogTags, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::DialogTagForeign {
                tag_header,
                tag,
                local_tags,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let what = if tag_header == "To" { "in-dialog request" } else { "response" };
            format!(
                "{tag_header}-tag mismatch: {what} carries {tag_header}-tag \"{tag}\" but this \
                 bind's local tags are [{expected}] — RFC 3261 §12.2.1.1",
                expected = local_tags.join(" | "),
            )
        })
    }
}

/// RFC 3261 §12.2.1.1 live: the merged `peer-uri-stable` rule at this bind's
/// vantage — surfaced at the TAKER, whose dialog matching the rewrite breaks.
pub struct PeerUriStableRule;

impl CrossMessageAuditRule for PeerUriStableRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::PeerUriStable.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced_at_taker(events, &rfc_rules::rules::correlation::PeerUriStable, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::PeerUriRewritten {
                sent_uri,
                dialog_uri,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "in-dialog From URI \"{sent_uri}\" differs from the dialog-established remote URI \
                 \"{dialog_uri}\" — RFC 3261 §12.2.1.1"
            )
        })
    }
}

/// RFC 3261 §12.1 live: the merged `dialog-call-id-stable` rule at this bind's
/// vantage — surfaced at the TAKER, left holding a dialog nothing matches.
pub struct DialogCallIdStableRule;

impl CrossMessageAuditRule for DialogCallIdStableRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::DialogCallIdStable.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced_at_taker(events, &rfc_rules::rules::correlation::DialogCallIdStable, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::DialogCallIdChanged {
                call_id,
                dialog_call_id,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "received in-dialog message Call-ID \"{call_id}\" differs from the dialog's \
                 confirmed Call-ID \"{dialog_call_id}\" — RFC 3261 §12.1 (Call-ID is immutable \
                 within a dialog)"
            )
        })
    }
}

/// RFC 3261 §9.1 live: the merged `cancel-request-uri` rule at this bind's
/// vantage — surfaced at the TAKER, whose server transaction the CANCEL misses.
pub struct CancelRequestUriRule;

impl CrossMessageAuditRule for CancelRequestUriRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::CancelRequestUri.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced_at_taker(events, &rfc_rules::rules::correlation::CancelRequestUri, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::CancelUriDiverged {
                cancel_uri,
                invite_uri,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "CANCEL Request-URI \"{cancel_uri}\" differs from the INVITE Request-URI \
                 \"{invite_uri}\" — RFC 3261 §9.1 (the CANCEL cannot match the INVITE server \
                 transaction)"
            )
        })
    }
}

/// RFC 3261 §9.1 live: the merged `cancel-via-branch` rule at this bind's
/// vantage — surfaced at the TAKER, whose INVITE the orphaned CANCEL leaves
/// ringing.
pub struct CancelViaBranchRule;

impl CrossMessageAuditRule for CancelViaBranchRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::CancelViaBranch.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced_at_taker(events, &rfc_rules::rules::correlation::CancelViaBranch, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::CancelBranchUnmatched {
                cancel_branch,
                invite_branches,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "CANCEL top Via branch \"{cancel_branch}\" matches no received INVITE's Via \
                 branch (saw: {known}) — RFC 3261 §9.1 (the CANCEL is routed to a different \
                 server transaction)",
                known = invite_branches.join(" | "),
            )
        })
    }
}

/// RFC 3261 §17.2.1 / §12.1.1 live: the merged `tag-consistency` rule at this
/// bind's vantage — the UAS mints the tag, so the bind is charged for its own
/// final.
///
/// The live policy this bind adds to the rule body: the finding is ADVISORY. A
/// forking B2BUA legitimately relays one early dialog's provisional (tag A) and
/// then answers 2xx off a later early dialog (tag B) on the same upstream INVITE
/// transaction — §12.1.2 / §13.2.2.4 permit a 2xx establishing a fresh dialog,
/// and per branch that is indistinguishable from a UAS tag flip. The finding is
/// recorded for review and never gates.
pub struct TagConsistencyRule;

impl CrossMessageAuditRule for TagConsistencyRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::TagConsistency.token()
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::correlation::TagConsistency, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::UasTagFlipped {
                status,
                branch,
                final_tag,
                provisional_tags,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "UAS To-tag mismatch on {status} (branch {branch}): prior provisional(s) \
                 established tag(s) [{seen}] but the final carries \"{final_tag}\" — RFC 3261 \
                 §17.2.1 / §12.1.1",
                seen = provisional_tags.join(", "),
            )
        })
    }
}

/// RFC 3262 §4 live: the merged `no-100rel-require-on-non-invite` rule at this
/// bind's vantage — the sender stamps the option tag, so the bind is charged.
pub struct No100relRequireOnNonInviteRule;

impl CrossMessageAuditRule for No100relRequireOnNonInviteRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::No100relRequireOnNonInvite.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::No100relRequireOnNonInvite, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent { on, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "{on} request carries Require: 100rel — RFC 3262 §4 admits it on INVITE only (a \
                 strict peer answers 420 Bad Extension)"
            )
        })
    }
}

/// RFC 3262 §3 live: the merged `reliable-1xx-headers` rule at this bind's
/// vantage — the responding UAS writes both rows.
///
/// The live policy this bind adds to the rule body: subject `{Uas}`, since the
/// reliable-provisional contract is the answering side's to state.
pub struct Reliable1xxHeadersRule;

impl CrossMessageAuditRule for Reliable1xxHeadersRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::Reliable1xxHeaders.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Uas])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::Reliable1xxHeaders, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::Reliable1xx { status, carried, .. }) =
                &f.decision
            else {
                return String::new();
            };
            if *status == 100 {
                return format!(
                    "100 (Trying) response carries {} — RFC 3262 §3 forbids a reliable 100",
                    carried.join(" and "),
                );
            }
            match carried.iter().find(|row| row.starts_with("RSeq:")) {
                None => format!(
                    "Reliable {status} response carries Require: 100rel but no RSeq header — RFC \
                     3262 §3 (the UAC has nothing to RAck)"
                ),
                Some(rseq) => format!(
                    "Reliable {status} response {rseq} is outside [1, 2^31-1] — RFC 3262 §3"
                ),
            }
        })
    }
}

/// RFC 3261 §8.1.1.2 live: the merged `no-to-tag-on-initial-request` rule at
/// this bind's vantage — the sender writes the To row, so the bind is charged.
pub struct NoToTagOnInitialRequestRule;

impl CrossMessageAuditRule for NoToTagOnInitialRequestRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoToTagOnInitialRequest.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::correlation::NoToTagOnInitialRequest, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent {
                on, value, ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "{on} request outside any dialog carries To-tag={value} — RFC 3261 §8.1.1.2 (it \
                 names a dialog that does not exist)"
            )
        })
    }
}

/// RFC 3261 §12.2.1.1 live: the merged `in-dialog-to-tag` rule at this bind's
/// vantage — the sender mints the header from its own dialog state.
///
/// The live policy this bind adds to the rule body: subject `{Uac, Uas}`, since
/// a declared proxy carries the originator's To through unchanged.
pub struct InDialogToTagRule;

impl CrossMessageAuditRule for InDialogToTagRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::InDialogToTag.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Uac, UaRole::Uas])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::correlation::InDialogToTag, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "{on} request inside a confirmed dialog carries no To-tag — RFC 3261 §12.2.1.1 \
                 requires the dialog's remote tag (the peer cannot dialog-match it)"
            )
        })
    }
}

/// RFC 3261 §8.2.2.3 live: the merged `no-require-on-cancel-or-ack` rule at
/// this bind's vantage — the sender writes the demand.
pub struct NoRequireOnCancelOrAckRule;

impl CrossMessageAuditRule for NoRequireOnCancelOrAckRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::NoRequireOnCancelOrAck.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::capability::NoRequireOnCancelOrAck, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent {
                on,
                header,
                value,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let what = if on.eq_ignore_ascii_case("ACK") {
                "ACK for a non-2xx response".to_string()
            } else {
                format!("{on} request")
            };
            format!(
                "{what} carries {header}: {value} — forbidden by RFC 3261 §8.2.2.3 (a \
                 transaction-management request imposes no extension)"
            )
        })
    }
}

/// RFC 3261 §9.1 live: the merged `cancel-cseq-method` rule at this bind's
/// vantage — the sender writes the CSeq row.
pub struct CancelCseqMethodRule;

impl CrossMessageAuditRule for CancelCseqMethodRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::CancelCseqMethod.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::wellformed::CancelCseqMethod, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { value, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "CANCEL request carries CSeq method={value} (expected CANCEL) — RFC 3261 §9.1"
            )
        })
    }
}

/// RFC 3261 §16.6 live: the merged `strict-route-shuffle-on-send` rule at this
/// bind's vantage — the forwarding hop is the party that runs the swap.
///
/// The live policy this bind adds to the rule body: subject `{Proxy}`, since
/// §16.6 step 6 is a forwarding element's step and a UA states the route set it
/// learned rather than shuffling one.
pub struct StrictRouteShuffleOnSendRule;

impl CrossMessageAuditRule for StrictRouteShuffleOnSendRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::StrictRouteShuffleOnSend.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Proxy])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::proxy::StrictRouteShuffleOnSend, |f, _cid| {
            let Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { on, value, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent {on} request still carries strict-route topmost Route entry \"{value}\" — \
                 RFC 3261 §16.6 step 6 swap may not have run"
            )
        })
    }
}

/// RFC 3264 §5-6 live: the merged `sdp-body-parseable` rule at this bind's
/// vantage — the sender minted the description.
pub struct SdpBodyParseableRule;

impl CrossMessageAuditRule for SdpBodyParseableRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::SdpBodyParseable.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::offer_answer::SdpBodyParseable, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::SdpBodyRejected { sdp_failure, .. }) =
                &f.decision
            else {
                return String::new();
            };
            format!(
                "Sent SDP body fails the RFC 3264 / RFC 4566 grammar check: {sdp_failure} (callId \
                 {cid})"
            )
        })
    }
}

/// RFC 3264 §6 / §8.4 live: the merged `c0-port-non-zero` rule at this bind's
/// vantage — the sender wrote both dispositions.
///
/// The live policy this bind adds to the rule body: subject `{Uac}`, because an
/// answerer (a B2BUA included) legitimately rejects a stream with `m=… 0` while
/// echoing a real, non-unspecified `c=`.
pub struct C0PortNonZeroRule;

impl CrossMessageAuditRule for C0PortNonZeroRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::C0PortNonZero.token()
    }

    fn subject(&self) -> std::collections::HashSet<UaRole> {
        std::collections::HashSet::from([UaRole::Uac])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        surfaced(events, &rfc_rules::rules::offer_answer::C0PortNonZero, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::HeldAndRejectedStreams {
                held_stream_indexes,
                held_streams,
                held_c_lines,
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            format!(
                "SDP body holds and rejects the same stream(s) — {rows} (callId {cid}) — RFC 3264 \
                 §6 / §8.4: an unspecified c= says \"hold\", port 0 says \"rejected\"",
                rows = stream_rows(held_stream_indexes, held_streams, held_c_lines, |i, m, c| {
                    format!("m=[{i}] {m} under c={c}")
                }),
            )
        })
    }
}

/// RFC 3261 §17 / §13.3.1.4, RFC 3262 §3 live: the merged `rung-byte-identical`
/// rule (`rfc_rules::rules::retransmit`) at this bind's vantage — a
/// retransmission is the same message, byte for byte (ADR-0029 X3).
///
/// The live policy this bind adds to the rule body: none, and it GATES. The
/// rule judges what a lane put on the wire TWICE, and a relay's rung is its own
/// re-send whatever it forwarded the first time — so there is no relay-lane
/// skip and every role is in subject. The SUT retains and repeats an opaque
/// datagram by construction, so a charge against it is a hole in that
/// construction, never a tolerance to add here.
pub struct RungByteIdenticalRule;

impl CrossMessageAuditRule for RungByteIdenticalRule {
    fn name(&self) -> &'static str {
        rfc_rules::RuleId::RungByteIdentical.token()
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        use rfc_rules::rules::retransmit::Class;
        surfaced(events, &rfc_rules::rules::retransmit::RungByteIdentical, |f, cid| {
            let Decision::Violated(rfc_rules::Evidence::RungDiverged {
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
                ..
            }) = &f.decision
            else {
                return String::new();
            };
            let what = match (status, rseq) {
                (Some(st), Some(rs)) => format!("{st} RSeq {rs} ({method})"),
                (Some(st), None) => format!("{st} ({method})"),
                (None, _) => format!("{method}, CSeq {}", f.cseq),
            };
            let line = |l: &Option<String>| match l {
                Some(l) => format!("`{l}`"),
                None => "<end of message>".to_string(),
            };
            let also = match divergent {
                0 | 1 => String::new(),
                n => format!(", and {} later copies diverge too", n - 1),
            };
            let clause = Class::from_label(class).map_or("RFC 3261 §17", Class::clause);
            format!(
                "Sent copy {rung} of {copies} of the {class} {what} (callId {cid}, branch {branch}) \
                 with other bytes than the first: the two diverge in the {region} at byte {offset} \
                 — first {}, this copy {}{also} — a retransmission is the same message, byte for \
                 byte ({clause})",
                line(first_line),
                line(rung_line),
            )
        })
    }
}

/// The merged-rule cross checks this adapter contributes to the default suite.
pub fn cross_rules() -> Vec<std::sync::Arc<dyn CrossMessageAuditRule>> {
    vec![
        std::sync::Arc::new(No200AfterCancelRule),
        std::sync::Arc::new(NoAckToDialogCreating2xxRule),
        std::sync::Arc::new(Unacked2xxNotClearedRule),
        std::sync::Arc::new(UnackedReliableProvisionalRule),
        std::sync::Arc::new(RackWithoutKnownInviteRule),
        std::sync::Arc::new(NoOverlappingReliableProvisionalsRule),
        std::sync::Arc::new(NonContiguousRseqRule),
        std::sync::Arc::new(NoPrackOfOutOfOrderRseqRule),
        std::sync::Arc::new(SingleFinalPerServerTxnRule),
        std::sync::Arc::new(CancelRouteEchoesInviteRule),
        std::sync::Arc::new(CancelAfter1xxRule),
        std::sync::Arc::new(NoCancelAfterFinalRule),
        std::sync::Arc::new(CseqInDialogOrderRule),
        std::sync::Arc::new(ResponseCseqMatchesTransactionRule),
        std::sync::Arc::new(AckCseqMatchesInviteRule),
        std::sync::Arc::new(MidDialogUriRule),
        std::sync::Arc::new(MidDialogRouteRule),
        std::sync::Arc::new(MidDialogWireDestinationRule),
        std::sync::Arc::new(RecordRoutePlacementRule),
        std::sync::Arc::new(RportEchoRule),
        std::sync::Arc::new(AllowSupportedOnInviteRule),
        std::sync::Arc::new(Proxy100TryingNotForwardedRule),
        std::sync::Arc::new(UnknownDialog481Rule),
        std::sync::Arc::new(UnsupportedMethod405AllowRule),
        std::sync::Arc::new(UnsupportedExtension420Rule),
        std::sync::Arc::new(Unsupported415AcceptsRule),
        std::sync::Arc::new(UnsupportedExtension421Rule),
        std::sync::Arc::new(NoTarget404Rule),
        std::sync::Arc::new(OptionsResponseEchoesRule),
        std::sync::Arc::new(AckRequireSubsetOfInviteRule),
        std::sync::Arc::new(AckPreservesInviteRouteRule),
        std::sync::Arc::new(StrictRouteRewriteHandledRule),
        std::sync::Arc::new(SerialRegisterRule),
        std::sync::Arc::new(RegisterNoRouteSetRule),
        std::sync::Arc::new(ConcurrentReInvite500Or491Rule),
        std::sync::Arc::new(NoByeOutsideOrEarlyDialogRule),
        std::sync::Arc::new(NoReInviteWhileInviteInProgressRule),
        std::sync::Arc::new(Proxy100WithinGraceRule),
        std::sync::Arc::new(UnackedInviteNon2xxFinalRule),
        std::sync::Arc::new(FailedReinviteTearsDownDialogRule),
        std::sync::Arc::new(No1xxAfterFinalRule),
        std::sync::Arc::new(RequireReliable1xxOnRequireRule),
        std::sync::Arc::new(ReliableNeedsClientOptInRule),
        std::sync::Arc::new(NoReliable1xxOnInDialogRule),
        std::sync::Arc::new(UnmatchedPrackProxiedRule),
        std::sync::Arc::new(Prack2xxOr481Rule),
        std::sync::Arc::new(Delay2xxOnUnackedReliable1xxWithSdpRule),
        std::sync::Arc::new(PrackAcceptedAfterFinalRule),
        std::sync::Arc::new(NoNewReliable1xxAfterFinalRule),
        std::sync::Arc::new(NoPrackOf100TryingRule),
        std::sync::Arc::new(PrackAnswers1xxOfferRule),
        std::sync::Arc::new(AckBodyAfterCompleteOfferAnswerRule),
        std::sync::Arc::new(Final2xxAnswersTheOfferRule),
        std::sync::Arc::new(SecondAnswerRepeatsTheFirstRule),
        std::sync::Arc::new(AnswerStreamMatchesOfferRule),
        std::sync::Arc::new(SdpOriginContinuityRule),
        std::sync::Arc::new(NoNewOfferWhileOfferPendingRule),
        std::sync::Arc::new(AnswerMLineCountMatchesOfferRule),
        std::sync::Arc::new(AnswerTLineEqualsOfferRule),
        std::sync::Arc::new(AnswerMediaTypeMatchesOfferRule),
        std::sync::Arc::new(DirectionPairValidRule),
        std::sync::Arc::new(RejectedStreamMinimalAnswerRule),
        std::sync::Arc::new(ReOfferMLineCountMonotonicRule),
        std::sync::Arc::new(ZeroPortPropagationRule),
        std::sync::Arc::new(PayloadTypeMappingStableRule),
        std::sync::Arc::new(BranchPrefixRule),
        std::sync::Arc::new(MaxForwardsRule),
        std::sync::Arc::new(ContentLengthRule),
        std::sync::Arc::new(ContentTypeRule),
        std::sync::Arc::new(ContactPresenceRule),
        std::sync::Arc::new(NoContactOnByeRule),
        std::sync::Arc::new(ToTagPresenceRule),
        std::sync::Arc::new(NoRecordRouteFromUaRule),
        std::sync::Arc::new(ResponseEchoesRequestViaRule),
        std::sync::Arc::new(ResponseCorrelationRule),
        std::sync::Arc::new(MidDialogTagsRule),
        std::sync::Arc::new(PeerUriStableRule),
        std::sync::Arc::new(DialogCallIdStableRule),
        std::sync::Arc::new(CancelRequestUriRule),
        std::sync::Arc::new(CancelViaBranchRule),
        std::sync::Arc::new(TagConsistencyRule),
        std::sync::Arc::new(No100relRequireOnNonInviteRule),
        std::sync::Arc::new(Reliable1xxHeadersRule),
        std::sync::Arc::new(CancelCseqMethodRule),
        std::sync::Arc::new(NoToTagOnInitialRequestRule),
        std::sync::Arc::new(InDialogToTagRule),
        std::sync::Arc::new(NoRequireOnCancelOrAckRule),
        std::sync::Arc::new(StrictRouteShuffleOnSendRule),
        std::sync::Arc::new(SdpBodyParseableRule),
        std::sync::Arc::new(C0PortNonZeroRule),
        std::sync::Arc::new(RungByteIdenticalRule),
    ]
}

#[cfg(test)]
mod tests {
    //! The LIVE policy this adapter adds to the merged bodies: subject
    //! dispatch, the relay-lane skip, and the wire position a finding anchors
    //! on. The rule semantics themselves are pinned in `rfc_rules::rules`.

    use std::collections::HashSet;

    use super::*;
    use crate::rfc_audit::{evaluate_rfc_findings, RfcFinding};
    use crate::types::{BindSummary, UdpPacket};

    // `to_sip_entries` resolves lanes by ADDRESS, so the traces use addr bind
    // keys: the SUT answering at :5080, the caller at :5060, the callee at :5070.
    const SUT: &str = "127.0.0.1:5080";
    const ALICE: &str = "127.0.0.1:5060";
    const BOB: &str = "127.0.0.1:5070";
    const A: &str = "sip:alice@127.0.0.1";
    const B: &str = "sip:bob@127.0.0.1";

    fn sent(bind: &str, raw: Vec<u8>, to: &str, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::SendCalled {
                bind_key: bind.to_string(),
                to: to.parse().unwrap(),
                msg: raw,
            },
            seq,
            at_ms: seq,
        }
    }

    fn recv(bind: &str, raw: Vec<u8>, src: &str, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                disposition: crate::types::RecvDisposition::Delivered,
                wire: crate::contracts::WireStamp::of_bytes(&raw),
                packet: UdpPacket { raw, src: src.parse().unwrap(), arrival_ms: seq },
            },
            seq,
            at_ms: seq,
        }
    }

    /// A `BindAcquire` declaring `roles` for `bind` — what subject dispatch in
    /// [`evaluate_rfc_findings`] reads.
    fn bind_roles(bind: &str, roles: HashSet<UaRole>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::BindAcquire {
                bind_key: bind.to_string(),
                summary: BindSummary {
                    addr: bind.parse().unwrap(),
                    queue_max: 256,
                    reuse_port: false,
                    roles,
                    has_pre_ingress: false,
                },
            },
            seq,
            at_ms: seq,
        }
    }

    fn req(method: &str, branch: &str, cseq: u32, ttag: Option<&str>) -> Vec<u8> {
        let to = match ttag {
            Some(t) => format!("<{B}>;tag={t}"),
            None => format!("<{B}>"),
        };
        format!(
            "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn resp(status: u16, cseq: u32, method: &str, ttag: &str, branch: &str) -> Vec<u8> {
        // Empty `ttag` ⇒ a tagless To (a 100 Trying); `;tag=` with an empty
        // value is not a wire token and even the lenient parser rejects it.
        let to = if ttag.is_empty() { format!("<{B}>") } else { format!("<{B}>;tag={ttag}") };
        format!(
            "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// The b-leg INVITE a B2BUA originates, carrying its OWN Call-ID — each leg
    /// is a distinct dialog, which is what keeps a B2BUA lane out of the relay
    /// classification that exempts a forwarding proxy.
    fn b_leg_invite(branch: &str) -> Vec<u8> {
        format!(
            "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=bft\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-b-leg@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A request the SUT sends on its own b-leg — the leg it drives as UAC,
    /// which is where a CANCEL of its own making rides.
    fn b_leg_req(method: &str, branch: &str, ttag: Option<&str>) -> Vec<u8> {
        let to = match ttag {
            Some(t) => format!("<{B}>;tag={t}"),
            None => format!("<{B}>"),
        };
        format!(
            "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=bft\r\n\
             To: {to}\r\n\
             Call-ID: cid-b-leg@127.0.0.1\r\n\
             CSeq: 1 {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// The far end's answer on that b-leg.
    fn b_leg_resp(status: u16, branch: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=bft\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-b-leg@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// RFC 3261 §9.1 at the CANCEL's sender: the SUT's own ACK is what proves
    /// the transaction was over. A CANCEL AHEAD of that ACK may have crossed
    /// the final in flight and is not charged; one behind it cannot have, and
    /// is.
    #[test]
    fn a_cancel_the_sut_sent_after_acking_its_own_final_is_flagged() {
        let crossed = vec![
            sent(SUT, b_leg_invite("z9hG4bK-b"), BOB, 0),
            recv(SUT, b_leg_resp(486, "z9hG4bK-b"), BOB, 1),
            sent(SUT, b_leg_req("CANCEL", "z9hG4bK-b", None), BOB, 2),
            sent(SUT, b_leg_req("ACK", "z9hG4bK-b", Some("bt")), BOB, 3),
        ];
        assert!(
            NoCancelAfterFinalRule.check_positioned(&crossed).is_empty(),
            "a CANCEL the ACK has not yet sealed may have crossed the final"
        );

        let late = vec![
            sent(SUT, b_leg_invite("z9hG4bK-b"), BOB, 0),
            recv(SUT, b_leg_resp(486, "z9hG4bK-b"), BOB, 1),
            sent(SUT, b_leg_req("ACK", "z9hG4bK-b", Some("bt")), BOB, 2),
            sent(SUT, b_leg_req("CANCEL", "z9hG4bK-b", None), BOB, 3),
        ];
        let out = NoCancelAfterFinalRule.check_positioned(&late);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "the lane that sent the CANCEL is charged");
        assert_eq!(out[0].3.as_deref(), Some(SUT));
        assert!(out[0].1.contains("486") && out[0].1.contains("§9.1"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(4), "offending points at the late CANCEL");
        assert!(!NoCancelAfterFinalRule.force_advisory(), "the late CANCEL gates");
    }

    /// The far end's own late CANCEL is not this bind's: the rule charges the
    /// UAC that sent it, and the live policy surfaces a finding only where the
    /// charged party IS the vantage.
    #[test]
    fn a_late_cancel_the_peer_sent_is_not_charged_to_this_bind() {
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
            recv(SUT, req("CANCEL", "z9hG4bK-i", 1, None), ALICE, 3),
        ];
        assert!(NoCancelAfterFinalRule.check_positioned(&evs).is_empty());
    }

    #[test]
    fn second_different_final_is_flagged_at_its_wire_position() {
        // The caller CANCELs, the SUT answers the INVITE 487 and the caller
        // ACKs — then a later decision authors a 480 on that transaction.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 3),
        ];
        let out = SingleFinalPerServerTxnRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "attributed to the lane that authored the second final");
        assert!(out[0].1.contains("480") && out[0].1.contains("487"), "{}", out[0].1);
        assert!(out[0].1.contains("z9hG4bK-i") && out[0].1.contains("INVITE"), "{}", out[0].1);
        // The wire view lists all four messages in order (an arrival from a
        // non-recorded sender is its own entry), so the late 480 is entry 4.
        assert_eq!(out[0].2, Some(4), "offending points at the late 480");
    }

    #[test]
    fn every_divergent_status_is_named_on_the_one_finding() {
        // One occasion per transaction: the detail names 480 AND the later 486,
        // so no status goes unreported.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-r", 1, None), ALICE, 0),
            sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 1),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 2),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 3),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 4),
            recv(SUT, req("ACK", "z9hG4bK-r", 1, Some("bt")), ALICE, 5),
        ];
        let out = SingleFinalPerServerTxnRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "one finding for the transaction: {out:?}");
        assert!(
            out[0].1.contains("480") && out[0].1.contains("486") && out[0].1.contains("487"),
            "{}",
            out[0].1
        );
        assert_eq!(out[0].2, Some(3), "attributed to the FIRST copy of the offending final");
    }

    #[test]
    fn same_status_retransmissions_are_silent() {
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-g", 1, None), ALICE, 0),
            sent(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-g"), ALICE, 1),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-g"), ALICE, 2),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-g"), ALICE, 3),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-g"), ALICE, 4),
            recv(SUT, req("ACK", "z9hG4bK-g", 1, Some("bt")), ALICE, 5),
        ];
        assert!(SingleFinalPerServerTxnRule.check_positioned(&evs).is_empty());
    }

    #[test]
    fn later_1xx_stays_with_no_1xx_after_final() {
        // A provisional after the final is the sibling rule's finding, never
        // this one's — the ownership boundary, asserted from both sides.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-p", 1, None), ALICE, 0),
            sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-p"), ALICE, 1),
            sent(SUT, resp(183, 1, "INVITE", "bt", "z9hG4bK-p"), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-p", 1, Some("bt")), ALICE, 3),
        ];
        assert!(
            SingleFinalPerServerTxnRule.check_positioned(&evs).is_empty(),
            "a 1xx is not a final — this rule does not judge it"
        );
        assert_eq!(
            No1xxAfterFinalRule.check(&evs).len(),
            1,
            "the sibling rule owns the after-final provisional"
        );
    }

    #[test]
    fn relay_lane_forwarding_two_finals_is_not_judged() {
        // A lane that forwards both directions of one Call-ID relays what the
        // upstream produced; the divergent pair is flagged on the upstream lane.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
            sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
            sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 3),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 4),
        ];
        assert!(SingleFinalPerServerTxnRule.check_positioned(&evs).is_empty());
    }

    #[test]
    fn b2bua_lane_authoring_its_a_leg_finals_is_judged() {
        // The same shape as the relay case, except the forwarded INVITE carries
        // the B2BUA's own b-leg Call-ID: the a-leg finals are the B2BUA's own
        // emissions, so the relay exemption must not swallow them.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
            sent(SUT, b_leg_invite("z9hG4bK-b"), BOB, 1),
            sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 3),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 4),
        ];
        let out = SingleFinalPerServerTxnRule.check(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
    }

    /// A request carrying an explicit header block — the §9.1 Route rules read
    /// the bytes, so a builder that can vary them is what those tests need.
    fn req_with(method: &str, branch: &str, extra: &str) -> Vec<u8> {
        format!(
            "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 {method}\r\n\
             Max-Forwards: 70\r\n\
             {extra}Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn divergent_cancel_route_is_flagged_at_its_wire_position() {
        let evs = vec![
            sent(SUT, req_with("INVITE", "z9hG4bK-r", "Route: <sip:p1@h;lr>\r\n"), BOB, 0),
            sent(SUT, req_with("CANCEL", "z9hG4bK-r", "Route: <sip:p2@h;lr>\r\n"), BOB, 1),
        ];
        let out = CancelRouteEchoesInviteRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "attributed to the lane that sent the CANCEL");
        assert!(out[0].1.contains("differ") && out[0].1.contains("z9hG4bK-r"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the CANCEL");

        let echoed = vec![
            sent(SUT, req_with("INVITE", "z9hG4bK-r", "Route: <sip:p1@h;lr>\r\n"), BOB, 0),
            sent(SUT, req_with("CANCEL", "z9hG4bK-r", "Route: <sip:p1@h;lr>\r\n"), BOB, 1),
        ];
        assert!(CancelRouteEchoesInviteRule.check_positioned(&echoed).is_empty());
    }

    #[test]
    fn relay_lane_passing_a_cancel_through_is_not_judged() {
        // A lane that forwards both directions of one Call-ID passes the
        // upstream UAC's CANCEL through; a divergence is the upstream's.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
            sent(SUT, req_with("INVITE", "z9hG4bK-b", "Route: <sip:p1@h;lr>\r\n"), BOB, 1),
            sent(SUT, req_with("CANCEL", "z9hG4bK-b", "Route: <sip:p2@h;lr>\r\n"), BOB, 2),
        ];
        assert!(CancelRouteEchoesInviteRule.check_positioned(&evs).is_empty());
    }

    #[test]
    fn eager_cancel_is_advisory_and_noted_on_a_forwarding_lane() {
        // The rule never gates (ADR-0028), and a B2BUA/AS lane authoring its
        // own b-leg CANCEL is judged on it — no relay exemption here.
        assert!(CancelAfter1xxRule.force_advisory());
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
            sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
            sent(SUT, req("CANCEL", "z9hG4bK-b", 1, None), BOB, 2),
        ];
        let out = CancelAfter1xxRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("before any received"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(3), "offending points at the CANCEL");

        // A taken provisional discharges it.
        let waited = vec![
            recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
            sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
            recv(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-b"), BOB, 2),
            sent(SUT, req("CANCEL", "z9hG4bK-b", 1, None), BOB, 3),
        ];
        assert!(CancelAfter1xxRule.check_positioned(&waited).is_empty());
    }

    #[test]
    fn proxy_declared_lane_sends_no_eager_cancel_finding() {
        // A `{Proxy}`-only lane forwards the upstream UAC's CANCEL at the
        // upstream's timing: exempt, where a `{Uac, Uas}` lane is judged.
        let wire = vec![
            sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
            sent(SUT, req("CANCEL", "z9hG4bK-b", 1, None), BOB, 2),
        ];
        let mut ua = vec![bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0)];
        ua.extend(wire.clone());
        assert_eq!(CancelAfter1xxRule.check(&ua).len(), 1, "a UA lane is judged");

        let mut proxy = vec![bind_roles(SUT, HashSet::from([UaRole::Proxy]), 0)];
        proxy.extend(wire);
        assert!(CancelAfter1xxRule.check(&proxy).is_empty(), "a {{Proxy}} lane is exempt");
    }

    /// The CSeq family is surfaced at the TAKER: the lane that RECEIVED the
    /// stream reports it, the finding names the far UAC as the offender, and
    /// the wire position pins the reusing request — which is what keeps a
    /// party-scoped waiver resolving to the sender, not to the reporting lane.
    #[test]
    fn a_cseq_reuse_is_reported_on_the_lane_that_took_it() {
        let evs = vec![
            recv(SUT, req("OPTIONS", "z9hG4bK-o", 2, Some("bt")), ALICE, 0),
            recv(SUT, req("BYE", "z9hG4bK-b", 2, Some("bt")), ALICE, 1),
        ];
        let out = CseqInDialogOrderRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "reported on the lane that took the stream");
        assert!(out[0].1.contains("reuses a prior request's CSeq"), "{}", out[0].1);
        assert!(out[0].1.contains("to-tag=bt"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the reusing BYE");
    }

    /// A fully recorded fabric carries every datagram twice — the sender's
    /// `SendCalled` and the taker's `RecvItem`. The taker vantage reports it
    /// ONCE: the sender's own view judges the same stream but is not its taker.
    #[test]
    fn a_recorded_sender_does_not_double_report_the_taker_finding() {
        let evs = vec![
            sent(ALICE, req("OPTIONS", "z9hG4bK-o", 2, Some("bt")), SUT, 0),
            recv(SUT, req("OPTIONS", "z9hG4bK-o", 2, Some("bt")), ALICE, 1),
            sent(ALICE, req("BYE", "z9hG4bK-b", 2, Some("bt")), SUT, 2),
            recv(SUT, req("BYE", "z9hG4bK-b", 2, Some("bt")), ALICE, 3),
        ];
        let out = CseqInDialogOrderRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "one report, at the taker: {out:?}");
        assert_eq!(out[0].0, SUT);
    }

    /// §8.1.3.5 at the taker: the SUT opened the transaction, so its OWN send
    /// is what the response is judged against — a `200 (INVITE)` carrying a
    /// number no request on that branch used is charged to the callee and
    /// reported on the lane that took it.
    #[test]
    fn a_response_cseq_off_its_transaction_is_reported_at_the_taker() {
        let evs = vec![
            sent(SUT, req("INVITE", "z9hG4bK-i", 1, None), BOB, 0),
            recv(SUT, resp(200, 2, "INVITE", "bt", "z9hG4bK-i"), BOB, 1),
        ];
        let out = ResponseCseqMatchesTransactionRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("does not match its transaction"), "{}", out[0].1);
        assert!(out[0].1.contains("z9hG4bK-i"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the response");

        // The response the transaction did open for settles it.
        let matched = vec![
            sent(SUT, req("INVITE", "z9hG4bK-i", 1, None), BOB, 0),
            recv(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), BOB, 1),
        ];
        assert!(ResponseCseqMatchesTransactionRule.check_positioned(&matched).is_empty());
    }

    /// §13.2.2.4 at the taker: the UAS that cannot match the ACK to its INVITE
    /// server transaction is the lane that reports it.
    #[test]
    fn an_ack_on_an_unknown_cseq_is_reported_at_the_taker() {
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            recv(SUT, req("ACK", "z9hG4bK-a", 3, Some("bt")), ALICE, 1),
        ];
        let out = AckCseqMatchesInviteRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("acknowledges no INVITE"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the ACK");

        // The ACK that reuses its INVITE's CSeq is silent.
        let matched = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 1),
        ];
        assert!(AckCseqMatchesInviteRule.check_positioned(&matched).is_empty());
    }

    /// An in-dialog request the SUT sent, with caller-chosen Request-URI,
    /// From/To URIs and extra header rows — the §12 rules read those off the
    /// bytes, so a builder that can vary them is what those tests need.
    #[allow(clippy::too_many_arguments)]
    fn in_dialog(
        method: &str,
        r_uri: &str,
        branch: &str,
        cseq: u32,
        from_uri: &str,
        to_uri: &str,
        extra: &str,
    ) -> Vec<u8> {
        format!(
            "{method} {r_uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{from_uri}>;tag=at\r\n\
             To: <{to_uri}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             {extra}Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A response carrying extra header rows (Record-Route, Allow, …).
    fn resp_with(status: u16, cseq: u32, method: &str, branch: &str, extra: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// The establishing exchange every §12 test opens with: the SUT's INVITE
    /// and the 200 that confirms the dialog, with `extra` on the 200.
    fn established(extra: &str) -> Vec<Stamped<SignalingNetworkEvent>> {
        vec![
            sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
            recv(SUT, resp_with(200, 1, "INVITE", "z9hG4bK-i", extra), BOB, 1),
        ]
    }

    /// The dialog-creating INVITE (no To tag) the SUT sends.
    fn in_dialog_less_to_tag(branch: &str) -> Vec<u8> {
        format!(
            "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// §12.2.1.1 at the EMITTER: the lane that rewrote the dialog's From URI
    /// mid-dialog is the lane charged, and the position pins the BYE.
    #[test]
    fn a_rewritten_mid_dialog_uri_is_reported_on_the_lane_that_sent_it() {
        let mut evs = established("");
        evs.push(sent(
            SUT,
            in_dialog("BYE", B, "z9hG4bK-b", 2, "sip:eve@127.0.0.1", B, ""),
            BOB,
            2,
        ));
        let out = MidDialogUriRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "attributed to the lane that sent the request");
        assert!(out[0].1.contains("From URI") && out[0].1.contains("sip:eve@127.0.0.1"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(3), "offending points at the BYE");

        let mut clean = established("");
        clean.push(sent(SUT, in_dialog("BYE", B, "z9hG4bK-b", 2, A, B, ""), BOB, 2));
        assert!(MidDialogUriRule.check_positioned(&clean).is_empty());
    }

    /// A relay carries the originator's From/To through unchanged: per-UA URI
    /// stability is not its invariant, so a forwarding lane is not judged.
    #[test]
    fn a_relay_lane_carrying_a_foreign_uri_is_not_judged() {
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
            sent(SUT, in_dialog_less_to_tag("z9hG4bK-b"), BOB, 1),
            recv(SUT, resp_with(200, 1, "INVITE", "z9hG4bK-b", ""), BOB, 2),
            sent(SUT, in_dialog("BYE", B, "z9hG4bK-y", 2, "sip:eve@127.0.0.1", B, ""), BOB, 3),
        ];
        assert!(MidDialogUriRule.check_positioned(&evs).is_empty());
    }

    /// §12.2.1.1 route reproduction at the emitter: the 200's Record-Route
    /// stack reversed is what the BYE owes.
    #[test]
    fn a_dropped_mid_dialog_route_set_is_reported_on_the_lane_that_sent_it() {
        let mut evs = established("Record-Route: <sip:p1@127.0.0.1;lr>\r\n");
        evs.push(sent(SUT, in_dialog("BYE", B, "z9hG4bK-b", 2, A, B, ""), BOB, 2));
        let out = MidDialogRouteRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("omits Route header"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(3), "offending points at the BYE");

        let mut replayed = established("Record-Route: <sip:p1@127.0.0.1;lr>\r\n");
        replayed.push(sent(
            SUT,
            in_dialog("BYE", B, "z9hG4bK-b", 2, A, B, "Route: <sip:p1@127.0.0.1;lr>\r\n"),
            BOB,
            2,
        ));
        assert!(MidDialogRouteRule.check_positioned(&replayed).is_empty());
    }

    /// §8.1.2 + RFC 3263 §4 at the emitter: the bytes went past the
    /// destination the request's own Request-URI names.
    #[test]
    fn bytes_sent_past_the_derived_destination_are_reported_at_the_emitter() {
        let mut evs = established("");
        evs.push(sent(SUT, in_dialog("BYE", "sip:bob@127.0.0.1:5070", "z9hG4bK-b", 2, A, B, ""), "127.0.0.1:9999", 2));
        let out = MidDialogWireDestinationRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("wire-sent to 127.0.0.1:9999"), "{}", out[0].1);

        let mut clean = established("");
        clean.push(sent(SUT, in_dialog("BYE", "sip:bob@127.0.0.1:5070", "z9hG4bK-b", 2, A, B, ""), BOB, 2));
        assert!(MidDialogWireDestinationRule.check_positioned(&clean).is_empty());
    }

    /// §12.1.1 at the TAKER: the lane that TOOK the 100 reports it, and the far
    /// endpoint that leaked the Record-Route is the one the text describes.
    #[test]
    fn record_route_on_a_100_is_reported_at_the_taker() {
        let evs = vec![
            sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
            recv(
                SUT,
                resp(100, 1, "INVITE", "", "z9hG4bK-i"),
                BOB,
                1,
            ),
        ];
        assert!(RecordRoutePlacementRule.check_positioned(&evs).is_empty(), "a bare 100 is clean");

        let leaked = vec![
            sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
            recv(
                SUT,
                format!(
                    "SIP/2.0 100 Trying\r\n\
                     Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-i\r\n\
                     From: <{A}>;tag=at\r\n\
                     To: <{B}>\r\n\
                     Call-ID: cid-1@127.0.0.1\r\n\
                     CSeq: 1 INVITE\r\n\
                     Record-Route: <sip:p1@127.0.0.1;lr>\r\n\
                     Content-Length: 0\r\n\r\n"
                )
                .into_bytes(),
                BOB,
                1,
            ),
        ];
        let out = RecordRoutePlacementRule.check_positioned(&leaked);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "reported on the lane that took it");
        assert!(out[0].1.contains("100 Trying carries Record-Route"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the 100");
    }

    /// RFC 3581 §4 at the taker, and it never gates.
    #[test]
    fn a_dropped_rport_is_advisory_and_reported_at_the_taker() {
        assert!(RportEchoRule.force_advisory());
        let ask = |tail: &str| {
            format!(
                "OPTIONS {B} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-o{tail}\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 5 OPTIONS\r\n\
                 Max-Forwards: 70\r\n\
                 Content-Length: 0\r\n\r\n"
            )
            .into_bytes()
        };
        let answer = |tail: &str| {
            format!(
                "SIP/2.0 200 OK\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-o{tail}\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 5 OPTIONS\r\n\
                 Content-Length: 0\r\n\r\n"
            )
            .into_bytes()
        };
        let evs = vec![sent(SUT, ask(";rport"), BOB, 0), recv(SUT, answer(""), BOB, 1)];
        let out = RportEchoRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "reported on the lane that asked");
        assert!(out[0].1.contains("dropped the rport"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the response");

        let echoed =
            vec![sent(SUT, ask(";rport"), BOB, 0), recv(SUT, answer(";rport=5080"), BOB, 1)];
        assert!(RportEchoRule.check_positioned(&echoed).is_empty());
    }

    /// §13.2.1 / §20.37 at the taker: ONE finding names both headers.
    #[test]
    fn a_re_invite_with_no_capabilities_is_reported_once_at_the_taker() {
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            recv(SUT, req("INVITE", "z9hG4bK-r", 2, Some("bt")), ALICE, 1),
        ];
        let out = AllowSupportedOnInviteRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "one finding, both headers on it: {out:?}");
        assert_eq!(out[0].0, SUT, "reported on the lane that took the re-INVITE");
        assert!(out[0].1.contains("missing Allow:") && out[0].1.contains("missing Supported:"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the re-INVITE");

        let advertised = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            recv(
                SUT,
                in_dialog("INVITE", B, "z9hG4bK-r", 2, A, B, "Allow: INVITE, ACK\r\nSupported: 100rel\r\n"),
                ALICE,
                1,
            ),
        ];
        assert!(AllowSupportedOnInviteRule.check_positioned(&advertised).is_empty());
    }

    /// §16.7 step 5 at the taker: one 100 per INVITE sent is owed, and only the
    /// copy past the budget is reported — once, whatever the count.
    #[test]
    fn an_extra_100_is_reported_once_at_the_taker() {
        let owed = vec![
            sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
            recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 1),
        ];
        assert!(Proxy100TryingNotForwardedRule.check_positioned(&owed).is_empty());

        let mut extra = owed;
        extra.push(recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 2));
        extra.push(recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 3));
        let out = Proxy100TryingNotForwardedRule.check_positioned(&extra);
        assert_eq!(out.len(), 1, "one finding for the transaction: {out:?}");
        assert_eq!(out[0].0, SUT, "reported on the lane that took them");
        assert!(out[0].1.contains("received 3 100 Trying"), "{}", out[0].1);
        assert!(out[0].1.contains("against 1 INVITE(s) sent"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(3), "offending points at the first excess 100");
    }

    #[test]
    fn proxy_declared_lane_is_out_of_subject() {
        // Subject dispatch: the same trace fires for a `{Uac, Uas}` bind and is
        // silent for a `{Proxy}`-declared one.
        let wire = |bind: &str| {
            vec![
                recv(bind, req("INVITE", "z9hG4bK-s", 1, None), ALICE, 1),
                sent(bind, resp(487, 1, "INVITE", "bt", "z9hG4bK-s"), ALICE, 2),
                recv(bind, req("ACK", "z9hG4bK-s", 1, Some("bt")), ALICE, 3),
                sent(bind, resp(603, 1, "INVITE", "bt", "z9hG4bK-s"), ALICE, 4),
            ]
        };
        let fired = |evs: &[Stamped<SignalingNetworkEvent>]| -> Vec<RfcFinding> {
            evaluate_rfc_findings(evs)
                .into_iter()
                .filter(|f| f.rule == rfc_rules::RuleId::SingleFinalPerServerTxn.token())
                .collect()
        };

        let mut ua = vec![bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0)];
        ua.extend(wire(SUT));
        let found = fired(&ua);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].advisory, "the rule gates");

        let mut proxy = vec![bind_roles(SUT, HashSet::from([UaRole::Proxy]), 0)];
        proxy.extend(wire(SUT));
        assert!(fired(&proxy).is_empty(), "a {{Proxy}} bind is not the subject");
    }

    // ---- the §8.2 / §11.2 / §12.2.2 / §16.3 reject-and-capability family ---

    /// §8.2.1 at the taker: the lane that TOOK the unknown verb is the lane
    /// charged, and the finding names what it answered instead.
    #[test]
    fn an_unrecognised_method_is_flagged_on_the_lane_that_took_it() {
        let clean = vec![
            recv(SUT, req_with("FROBNICATE", "z9hG4bK-x", ""), ALICE, 0),
            sent(SUT, resp_with(405, 1, "FROBNICATE", "z9hG4bK-x", "Allow: INVITE, BYE\r\n"), ALICE, 1),
        ];
        assert!(UnsupportedMethod405AllowRule.check_positioned(&clean).is_empty());

        let served = vec![
            recv(SUT, req_with("FROBNICATE", "z9hG4bK-x", ""), ALICE, 0),
            sent(SUT, resp_with(200, 1, "FROBNICATE", "z9hG4bK-x", ""), ALICE, 1),
        ];
        let out = UnsupportedMethod405AllowRule.check_positioned(&served);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "the lane that took the verb owed the 405");
        assert!(out[0].1.contains("405") && out[0].1.contains("got 200 / Allow=0"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(1), "offending points at the request it answered wrong");
    }

    /// §8.2.2 at the taker: an unsupported `Require` served instead of refused,
    /// with the tag named.
    #[test]
    fn an_unsupported_require_served_is_flagged() {
        let clean = vec![
            recv(SUT, req_with("INVITE", "z9hG4bK-i", "Require: frobnicate\r\n"), ALICE, 0),
            sent(SUT, resp_with(420, 1, "INVITE", "z9hG4bK-i", "Unsupported: frobnicate\r\n"), ALICE, 1),
        ];
        assert!(UnsupportedExtension420Rule.check_positioned(&clean).is_empty());

        let served = vec![
            recv(SUT, req_with("INVITE", "z9hG4bK-i", "Require: frobnicate\r\n"), ALICE, 0),
            sent(SUT, resp_with(200, 1, "INVITE", "z9hG4bK-i", ""), ALICE, 1),
        ];
        let out = UnsupportedExtension420Rule.check_positioned(&served);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("[frobnicate]") && out[0].1.contains("420"), "{}", out[0].1);
    }

    /// §8.2.3 and §21.4.15 at the sender: the lane that SENT the bare final is
    /// the lane charged.
    #[test]
    fn a_bare_415_and_a_bare_421_are_flagged_on_their_sender() {
        let bare_415 = vec![sent(SUT, resp_with(415, 1, "INVITE", "z9hG4bK-i", ""), ALICE, 0)];
        let out = Unsupported415AcceptsRule.check_positioned(&bare_415);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("Accept / Accept-Encoding / Accept-Language"), "{}", out[0].1);

        let with_accept = vec![sent(
            SUT,
            resp_with(415, 1, "INVITE", "z9hG4bK-i", "Accept: application/sdp\r\n"),
            ALICE,
            0,
        )];
        assert!(Unsupported415AcceptsRule.check_positioned(&with_accept).is_empty());

        let bare_421 = vec![sent(SUT, resp_with(421, 1, "INVITE", "z9hG4bK-i", ""), ALICE, 0)];
        let out = UnsupportedExtension421Rule.check_positioned(&bare_421);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("Require"), "{}", out[0].1);

        let listed = vec![sent(
            SUT,
            resp_with(421, 1, "INVITE", "z9hG4bK-i", "Require: 100rel\r\n"),
            ALICE,
            0,
        )];
        assert!(UnsupportedExtension421Rule.check_positioned(&listed).is_empty());
    }

    /// §11.2 is advisory and reads at the sender of the 2xx.
    #[test]
    fn a_bare_options_200_is_advisory_at_its_sender() {
        assert!(OptionsResponseEchoesRule.force_advisory());
        let clean = vec![
            recv(SUT, req_with("OPTIONS", "z9hG4bK-o", ""), ALICE, 0),
            sent(SUT, resp_with(200, 1, "OPTIONS", "z9hG4bK-o", "Allow: INVITE\r\n"), ALICE, 1),
        ];
        assert!(OptionsResponseEchoesRule.check_positioned(&clean).is_empty());

        let bare = vec![
            recv(SUT, req_with("OPTIONS", "z9hG4bK-o", ""), ALICE, 0),
            sent(SUT, resp_with(200, 1, "OPTIONS", "z9hG4bK-o", ""), ALICE, 1),
        ];
        let out = OptionsResponseEchoesRule.check_positioned(&bare);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("Allow/Supported/Accept"), "{}", out[0].1);
    }

    /// §12.2.2 at the taker: a BYE naming a peer tag this lane never confirmed,
    /// served instead of refused 481.
    #[test]
    fn a_request_for_an_unknown_dialog_is_flagged_on_the_lane_that_served_it() {
        /// A BYE alice sent the SUT, with a caller-chosen From tag.
        fn bye(from_tag: &str) -> Vec<u8> {
            format!(
                "BYE {B} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-b\r\n\
                 From: <{A}>;tag={from_tag}\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 2 BYE\r\n\
                 Max-Forwards: 70\r\n\
                 Content-Length: 0\r\n\r\n"
            )
            .into_bytes()
        }
        let confirmed = |from_tag: &str, bye_status: u16| {
            vec![
                recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
                sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
                recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
                recv(SUT, bye(from_tag), ALICE, 3),
                sent(SUT, resp(bye_status, 2, "BYE", "bt", "z9hG4bK-b"), ALICE, 4),
            ]
        };
        assert!(
            UnknownDialog481Rule.check_positioned(&confirmed("at", 200)).is_empty(),
            "the dialog the 200 confirmed is known"
        );
        assert!(
            UnknownDialog481Rule.check_positioned(&confirmed("zz", 481)).is_empty(),
            "the 481 discharges it"
        );
        let out = UnknownDialog481Rule.check_positioned(&confirmed("zz", 200));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "the lane that owed the 481 is charged");
        assert!(out[0].1.contains("481") && out[0].1.contains("answered 200"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(4), "offending points at the BYE it served");
    }

    /// §16.3: advisory, `{Proxy}`-only, and — the regression this rule exists
    /// for — a §16.6 forward on a FRESH branch is a resolved target, not a
    /// no-target outcome.
    #[test]
    fn no_target_404_is_advisory_proxy_scoped_and_survives_a_branch_rewrite() {
        assert!(NoTarget404Rule.force_advisory());
        assert_eq!(NoTarget404Rule.subject(), HashSet::from([UaRole::Proxy]));

        let unforwarded = vec![
            recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
            sent(SUT, resp(500, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 1),
        ];
        let out = NoTarget404Rule.check_positioned(&unforwarded);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("expected 404"), "{}", out[0].1);

        let answered_404 = vec![
            recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
            sent(SUT, resp(404, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 1),
        ];
        assert!(NoTarget404Rule.check_positioned(&answered_404).is_empty());

        let forwarded = vec![
            recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
            sent(SUT, req("INVITE", "z9hG4bK-out", 1, None), BOB, 1),
            recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-out"), BOB, 2),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 3),
        ];
        assert!(
            NoTarget404Rule.check_positioned(&forwarded).is_empty(),
            "{:?}",
            NoTarget404Rule.check_positioned(&forwarded)
        );

        let relayed = vec![
            recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
            recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-other"), BOB, 1),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 2),
        ];
        assert!(
            NoTarget404Rule.check_positioned(&relayed).is_empty(),
            "a relayed downstream rejection resolved a target"
        );
    }

    /// §13.2.2.4 / §17.1.1.3 at the emitter: the lane that SENT the ACK is the
    /// lane charged, and the position pins the ACK itself.
    #[test]
    fn a_divergent_ack_is_flagged_on_the_lane_that_sent_it() {
        let evs = vec![
            sent(SUT, req_with("INVITE", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n"), BOB, 0),
            recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), BOB, 1),
            sent(SUT, req_with("ACK", "z9hG4bK-i", "Require: timer\r\n"), BOB, 2),
        ];
        let routes = AckPreservesInviteRouteRule.check_positioned(&evs);
        assert_eq!(routes.len(), 1, "{routes:?}");
        assert_eq!(routes[0].0, SUT, "attributed to the lane that sent the ACK");
        assert!(routes[0].1.contains("differ"), "{}", routes[0].1);
        assert_eq!(routes[0].2, Some(3), "offending points at the ACK");

        let requires = AckRequireSubsetOfInviteRule.check_positioned(&evs);
        assert_eq!(requires.len(), 1, "{requires:?}");
        assert!(requires[0].1.contains("subset"), "{}", requires[0].1);
        assert_eq!(requires[0].2, Some(3));

        // The ACK that echoes its INVITE surfaces nothing at all.
        let clean = vec![
            sent(SUT, req_with("INVITE", "z9hG4bK-i", "Require: 100rel\r\n"), BOB, 0),
            recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), BOB, 1),
            sent(SUT, req_with("ACK", "z9hG4bK-i", "Require: 100rel\r\n"), BOB, 2),
        ];
        assert!(AckPreservesInviteRouteRule.check_positioned(&clean).is_empty());
        assert!(AckRequireSubsetOfInviteRule.check_positioned(&clean).is_empty());
    }

    #[test]
    fn relay_lane_passing_an_ack_through_is_not_judged() {
        // A lane that forwards both directions of one Call-ID passes the
        // upstream UAC's ACK through, Route and Require included.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
            sent(SUT, req_with("INVITE", "z9hG4bK-b", "Route: <sip:p1@h;lr>\r\n"), BOB, 1),
            recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-b"), BOB, 2),
            sent(SUT, req_with("ACK", "z9hG4bK-b", "Require: timer\r\n"), BOB, 3),
        ];
        assert!(AckPreservesInviteRouteRule.check_positioned(&evs).is_empty());
        assert!(AckRequireSubsetOfInviteRule.check_positioned(&evs).is_empty());
    }

    /// §16.4 is proxy behaviour: a `{Proxy}`-declared lane is judged, a UA lane
    /// is not — the subject, not the body, is what keeps a UA off this rule.
    #[test]
    fn strict_route_is_judged_only_on_a_declared_proxy_lane() {
        let wire = vec![recv(
            SUT,
            req_with("INVITE", "z9hG4bK-in", "Route: <sip:strict@h>\r\n"),
            ALICE,
            1,
        )];
        let mut proxy = vec![bind_roles(SUT, HashSet::from([UaRole::Proxy]), 0)];
        proxy.extend(wire.clone());
        let found: Vec<RfcFinding> = evaluate_rfc_findings(&proxy)
            .into_iter()
            .filter(|f| f.rule == StrictRouteRewriteHandledRule.name())
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].lane, SUT);
        assert!(found[0].detail.contains("strict-route"), "{}", found[0].detail);
        assert_eq!(found[0].offending, Some(1), "offending points at the request taken");

        let mut ua = vec![bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0)];
        ua.extend(wire);
        assert!(
            !evaluate_rfc_findings(&ua)
                .iter()
                .any(|f| f.rule == StrictRouteRewriteHandledRule.name()),
            "a UA lane forwards nothing and is not judged",
        );
    }

    /// A REGISTER the lane SENT, carrying a Route header — §10.2 owes none.
    #[test]
    fn a_register_carrying_route_is_flagged_on_the_lane_that_sent_it() {
        let evs = vec![sent(
            SUT,
            req_with("REGISTER", "z9hG4bK-r", "Route: <sip:p@h;lr>\r\n"),
            BOB,
            0,
        )];
        let out = RegisterNoRouteSetRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("Route"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(1), "offending points at the REGISTER");

        let clean = vec![sent(SUT, req_with("REGISTER", "z9hG4bK-r", ""), BOB, 0)];
        assert!(RegisterNoRouteSetRule.check_positioned(&clean).is_empty());
    }

    /// §10.2 serialisation live: a second binding for one AOR while the first
    /// is unanswered, reported on the sending lane.
    #[test]
    fn a_racing_register_is_flagged_on_the_lane_that_sent_it() {
        let register = |branch: &str, contact: &str| {
            format!(
                "REGISTER {A} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{A}>\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 1 REGISTER\r\n\
                 Contact: {contact}\r\n\
                 Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
            )
            .into_bytes()
        };
        let evs = vec![
            sent(SUT, register("z9hG4bK-1", "<sip:a@1>"), BOB, 0),
            sent(SUT, register("z9hG4bK-2", "<sip:a@2>"), BOB, 1),
        ];
        let out = SerialRegisterRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("still pending"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the second REGISTER");

        // Answered first: the next binding is free to differ.
        let serialised = vec![
            sent(SUT, register("z9hG4bK-1", "<sip:a@1>"), BOB, 0),
            recv(SUT, resp(200, 1, "REGISTER", "rt", "z9hG4bK-1"), BOB, 1),
            sent(SUT, register("z9hG4bK-2", "<sip:a@2>"), BOB, 2),
        ];
        assert!(SerialRegisterRule.check_positioned(&serialised).is_empty());
    }

    /// Every merged rule reaches the live suite. A rung that gives a rule a
    /// body and forgets to surface it here would otherwise run nowhere and say
    /// nothing — the adapter is the ONLY live consumer, so the two registries
    /// are one fact stated twice.
    #[test]
    fn every_merged_rule_has_a_live_adapter() {
        let mut surfaced: Vec<&str> = cross_rules().iter().map(|r| r.name()).collect();
        surfaced.sort_unstable();
        let before = surfaced.len();
        surfaced.dedup();
        assert_eq!(before, surfaced.len(), "a rule is surfaced twice");
        let mut declared: Vec<&str> =
            rfc_rules::RuleId::ALL.iter().map(|r| r.token()).collect();
        declared.sort_unstable();
        assert_eq!(surfaced, declared);
    }

    // ── the §14 / §15 / §16.7 / §17.1.1.3 family ────────────────────────────

    /// A request the CALLEE originates: From/To reversed relative to the
    /// establishing INVITE, which is the orientation a callee-initiated BYE or
    /// re-INVITE rides.
    fn from_callee(method: &str, branch: &str, cseq: u32) -> Vec<u8> {
        format!(
            "{method} {A} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5070;branch={branch}\r\n\
             From: <{B}>;tag=bt\r\n\
             To: <{A}>;tag=at\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// §13.3.1.4 through the projection: the ACK discharges the UAS's 2xx, a
    /// BYE on the dialog discharges it too, and a retransmitted 2xx re-opens
    /// nothing. The leak — neither ACKed nor BYE'd — GATES.
    #[test]
    fn unacked_2xx_is_discharged_by_the_ack_or_a_bye_and_otherwise_gates() {
        let acked = vec![
            recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            recv(BOB, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 2),
        ];
        assert!(Unacked2xxNotClearedRule.check(&acked).is_empty());

        let byed = vec![
            recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            // A retransmit of the same 2xx must not re-open the obligation.
            sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 2),
            sent(BOB, from_callee("BYE", "z9hG4bK-b", 2), ALICE, 3),
        ];
        assert!(Unacked2xxNotClearedRule.check(&byed).is_empty());

        let leaked = vec![
            recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        ];
        let f = Unacked2xxNotClearedRule.check(&leaked);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("never ACKed"), "{}", f[0].1);
        assert!(!Unacked2xxNotClearedRule.force_advisory(), "the leak gates");
    }

    /// §14.2 at the UAS lane: the racing re-INVITE surfaces on the lane that
    /// TOOK it, pointed at that message, and a compliant 491 surfaces nothing.
    #[test]
    fn a_racing_re_invite_surfaces_at_the_lane_that_took_it() {
        let evs = vec![
            recv(BOB, req("INVITE", "z9hG4bK-1", 2, Some("bt")), ALICE, 0),
            recv(BOB, req("INVITE", "z9hG4bK-2", 3, Some("bt")), ALICE, 1),
            sent(BOB, resp(200, 3, "INVITE", "bt", "z9hG4bK-2"), ALICE, 2),
        ];
        let out = ConcurrentReInvite500Or491Rule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, BOB, "charged to the UAS that owed the 491");
        assert!(out[0].1.contains("491 or 500"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the racing re-INVITE");

        let compliant = vec![
            recv(BOB, req("INVITE", "z9hG4bK-1", 2, Some("bt")), ALICE, 0),
            recv(BOB, req("INVITE", "z9hG4bK-2", 3, Some("bt")), ALICE, 1),
            sent(BOB, resp(491, 3, "INVITE", "bt", "z9hG4bK-2"), ALICE, 2),
            sent(BOB, resp(200, 2, "INVITE", "bt", "z9hG4bK-1"), ALICE, 3),
        ];
        assert!(ConcurrentReInvite500Or491Rule.check_positioned(&compliant).is_empty());
    }

    /// §15 at the sending lane: a BYE with no peer tag is off-dialog, and a
    /// callee that BYEs before it has accepted is the early-dialog shape.
    #[test]
    fn a_bye_off_dialog_and_an_early_callee_bye_both_surface() {
        let off = vec![sent(ALICE, req("BYE", "z9hG4bK-b", 2, None), BOB, 0)];
        let f = NoByeOutsideOrEarlyDialogRule.check_positioned(&off);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].0, ALICE);
        assert!(f[0].1.contains("outside any dialog"), "{}", f[0].1);

        let early = vec![
            recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(BOB, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            sent(BOB, from_callee("BYE", "z9hG4bK-b", 2), ALICE, 2),
        ];
        let f = NoByeOutsideOrEarlyDialogRule.check_positioned(&early);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].0, BOB, "the callee is charged");
        assert!(f[0].1.contains("early dialog"), "{}", f[0].1);

        // Accepted first: the same BYE is the ordinary teardown.
        let accepted = vec![
            recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            recv(BOB, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 2),
            sent(BOB, from_callee("BYE", "z9hG4bK-b", 2), ALICE, 3),
        ];
        assert!(NoByeOutsideOrEarlyDialogRule.check_positioned(&accepted).is_empty());
    }

    /// §14.1 at the sending lane: the re-INVITE that overtook its own prior
    /// transaction's ACK names the RFC 6026 phase it collided with.
    #[test]
    fn a_re_invite_overtaking_the_ack_surfaces_at_the_uac_lane() {
        let evs = vec![
            sent(ALICE, req("INVITE", "z9hG4bK-1", 2, Some("bt")), BOB, 0),
            recv(ALICE, resp(200, 2, "INVITE", "bt", "z9hG4bK-1"), BOB, 1),
            sent(ALICE, req("INVITE", "z9hG4bK-2", 3, Some("bt")), BOB, 2),
        ];
        let out = NoReInviteWhileInviteInProgressRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, ALICE);
        assert!(out[0].1.contains("not yet ACKed"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(3), "offending points at the overtaking re-INVITE");
    }

    /// §16.7 stays ADVISORY and proxy-scoped: a paused test clock advances
    /// virtual time no real latency corresponds to.
    #[test]
    fn the_100_trying_grace_is_advisory_and_proxy_scoped() {
        assert!(Proxy100WithinGraceRule.force_advisory());
        assert_eq!(Proxy100WithinGraceRule.subject(), HashSet::from([UaRole::Proxy]));

        let silent = vec![recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0)];
        let f = Proxy100WithinGraceRule.check_positioned(&silent);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("100 Trying") && f[0].1.contains("no response sent"), "{}", f[0].1);

        // Answered inside the grace: §16.7 owes nothing.
        let prompt = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 5),
        ];
        assert!(Proxy100WithinGraceRule.check_positioned(&prompt).is_empty());

        // Past it, the finding names the Δ actually observed.
        let late = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 350),
        ];
        let f = Proxy100WithinGraceRule.check_positioned(&late);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("Δ=350ms"), "{}", f[0].1);
    }

    /// §17.1.1.3 GATES at the rejecting UAS lane, and only there: the UAC that
    /// merely TOOK a reject owes nothing, and a 2xx is another rule's business.
    #[test]
    fn an_unacked_reject_gates_at_the_uas_lane() {
        let evs = vec![
            recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(BOB, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        ];
        let out = UnackedInviteNon2xxFinalRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, BOB);
        assert!(out[0].1.contains("never ACKed"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the un-ACKed final");
        assert!(!UnackedInviteNon2xxFinalRule.force_advisory(), "promoted to gating");

        let acked = vec![
            recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(BOB, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            recv(BOB, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
        ];
        assert!(UnackedInviteNon2xxFinalRule.check_positioned(&acked).is_empty());

        let uac_side = vec![
            sent(ALICE, req("INVITE", "z9hG4bK-u", 1, None), BOB, 0),
            recv(ALICE, resp(486, 1, "INVITE", "bt", "z9hG4bK-u"), BOB, 1),
        ];
        assert!(UnackedInviteNon2xxFinalRule.check_positioned(&uac_side).is_empty());
    }

    /// §14.1's abandoned transaction: a provisional and then silence on a
    /// dialog nothing ever BYE'd, surfaced at the lane that sent the re-INVITE.
    #[test]
    fn an_abandoned_re_invite_surfaces_unless_the_dialog_was_byed() {
        let evs = vec![
            sent(ALICE, req("INVITE", "z9hG4bK-re", 2, Some("bt")), BOB, 0),
            recv(ALICE, resp(183, 2, "INVITE", "bt", "z9hG4bK-re"), BOB, 1),
        ];
        let out = FailedReinviteTearsDownDialogRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, ALICE);
        assert!(out[0].1.contains("silently torn down"), "{}", out[0].1);
        assert!(!FailedReinviteTearsDownDialogRule.force_advisory());

        // "Absent another cause": a BYE is an independent teardown.
        let byed = vec![
            sent(ALICE, req("INVITE", "z9hG4bK-re", 2, Some("bt")), BOB, 0),
            recv(ALICE, resp(100, 2, "INVITE", "", "z9hG4bK-re"), BOB, 1),
            sent(ALICE, req("BYE", "z9hG4bK-bye", 3, Some("bt")), BOB, 2),
        ];
        assert!(FailedReinviteTearsDownDialogRule.check_positioned(&byed).is_empty());
    }

    /// §13.3.1.1 at the UAS lane: a NEW provisional after the final fires and
    /// points at itself; a relay lane forwarding its upstream's is not judged.
    #[test]
    fn a_late_provisional_fires_at_the_uas_lane_and_not_on_a_relay() {
        assert_eq!(No1xxAfterFinalRule.subject(), HashSet::from([UaRole::Uas]));
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 3),
            sent(SUT, resp(181, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 4),
        ];
        let out = No1xxAfterFinalRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "attributed to the UAS lane");
        assert_eq!(out[0].2, Some(5), "offending points at the late 181");

        // A retransmitted final and a reordered pre-final provisional are not
        // new emissions.
        let retransmits = vec![
            sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 0),
            sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 2),
            sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 3),
        ];
        assert!(No1xxAfterFinalRule.check_positioned(&retransmits).is_empty());

        // A lane that forwards both directions of one Call-ID relays what the
        // upstream produced; the late 1xx is flagged on the upstream lane.
        let relayed = vec![
            recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
            sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
            sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 3),
            sent(SUT, resp(183, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 4),
        ];
        assert!(No1xxAfterFinalRule.check_positioned(&relayed).is_empty());
    }

    // ── the RFC 3262 family ─────────────────────────────────────────────────
    //
    // Fixtures that can carry the reliable-provisional headers, an `RAck` and a
    // body: the whole family reads those, and the `Content-Length` is what a
    // rule asks about a body from a head-only vantage.

    const SDP: &str = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
                       m=audio 5004 RTP/AVP 0\r\n";

    fn body_rows(sdp: bool) -> String {
        match sdp {
            true => format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n", SDP.len()),
            false => "Content-Length: 0\r\n".to_string(),
        }
    }

    /// The body bytes those rows describe — the live adapter parses the whole
    /// datagram, so a declared length with nothing behind it is not a message.
    fn body(sdp: bool) -> &'static str {
        if sdp {
            SDP
        } else {
            ""
        }
    }

    /// The reliable-provisional markers RFC 3262 §3 requires of both halves.
    fn reliable_rows(rseq: u64) -> String {
        format!("Require: 100rel\r\nRSeq: {rseq}\r\n")
    }

    /// An INVITE with caller-chosen option-tag rows, To tag and body.
    fn invite_3262(branch: &str, extra: &str, to_tag: Option<&str>, sdp: bool) -> Vec<u8> {
        let to = match to_tag {
            Some(t) => format!("<{B}>;tag={t}"),
            None => format!("<{B}>"),
        };
        format!(
            "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n{extra}{}\r\n{}",
            body_rows(sdp),
            body(sdp),
        )
        .into_bytes()
    }

    /// An INVITE-transaction response with caller-chosen rows and body.
    fn inv_resp_3262(status: u16, branch: &str, extra: &str, sdp: bool) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n{extra}{}\r\n{}",
            body_rows(sdp),
            body(sdp),
        )
        .into_bytes()
    }

    /// An in-dialog UPDATE (RFC 3311) and a response on its transaction — a
    /// non-INVITE request, which no reliable provisional may answer.
    fn update_3262(branch: &str, extra: &str) -> Vec<u8> {
        format!(
            "UPDATE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 2 UPDATE\r\n\
             Max-Forwards: 70\r\n{extra}Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn update_resp_3262(status: u16, branch: &str, extra: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 2 UPDATE\r\n{extra}Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A PRACK whose `RAck` is spelled out verbatim.
    fn prack_3262(branch: &str, rack: &str, sdp: bool) -> Vec<u8> {
        format!(
            "PRACK {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n\
             Max-Forwards: 70\r\n{}\r\n{}",
            body_rows(sdp),
            body(sdp),
        )
        .into_bytes()
    }

    /// The response on that PRACK's own server transaction.
    fn prack_resp_3262(status: u16, branch: &str) -> Vec<u8> {
        resp(status, 2, "PRACK", "bt", branch)
    }

    // ── the obligation half, ported in rung 2 ───────────────────────────────

    #[test]
    fn a_second_reliable_provisional_waits_for_the_prack_of_the_last() {
        let serialised = vec![
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
            recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
            sent(SUT, prack_resp_3262(200, "z9hG4bK-p"), ALICE, 2),
            sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(2), false), ALICE, 3),
        ];
        assert!(NoOverlappingReliableProvisionalsRule.check(&serialised).is_empty());

        let overlapped = vec![
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
            sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(2), false), ALICE, 1),
        ];
        let out = NoOverlappingReliableProvisionalsRule.check(&overlapped);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-012"), "{}", out[0].1);
    }

    #[test]
    fn an_rseq_gap_is_flagged_at_the_offending_provisional() {
        let contiguous = vec![
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(5), false), ALICE, 0),
            sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(6), false), ALICE, 1),
        ];
        assert!(NonContiguousRseqRule.check(&contiguous).is_empty());

        let gapped = vec![
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(5), false), ALICE, 0),
            sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(8), false), ALICE, 1),
        ];
        let out = NonContiguousRseqRule.check_positioned(&gapped);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-013"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending = the non-contiguous 183");
    }

    #[test]
    fn a_reliable_provisional_the_uac_never_pracked_is_flagged() {
        let pracked = vec![
            sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), BOB, 0),
            recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 1),
            sent(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), BOB, 2),
        ];
        assert!(UnackedReliableProvisionalRule.check(&pracked).is_empty());

        let unpracked = vec![
            sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), BOB, 0),
            recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 1),
        ];
        let out = UnackedReliableProvisionalRule.check(&unpracked);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-021"), "{}", out[0].1);

        // The merged rule needs the 100rel negotiation WITNESSED: a provisional
        // whose INVITE this vantage never carried settles nothing.
        let unwitnessed =
            vec![recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 0)];
        assert!(UnackedReliableProvisionalRule.check(&unwitnessed).is_empty());
    }

    #[test]
    fn pracking_out_of_order_is_flagged_at_the_prack() {
        let in_order = vec![
            recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 0),
            sent(SUT, prack_3262("z9hG4bK-p1", "1 1 INVITE", false), BOB, 1),
            recv(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(2), false), BOB, 2),
            sent(SUT, prack_3262("z9hG4bK-p2", "2 1 INVITE", false), BOB, 3),
        ];
        assert!(NoPrackOfOutOfOrderRseqRule.check(&in_order).is_empty());

        let jumped = vec![
            recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 0),
            recv(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(5), false), BOB, 1),
            sent(SUT, prack_3262("z9hG4bK-p", "5 1 INVITE", false), BOB, 2),
        ];
        let out = NoPrackOfOutOfOrderRseqRule.check_positioned(&jumped);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-024"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(3), "offending = the PRACK, not the 1xx");
    }

    // ── the §3/§4/§5 negotiation half ───────────────────────────────────────

    #[test]
    fn a_plain_18x_against_require_100rel_is_flagged_at_its_wire_position() {
        let evs = vec![
            recv(SUT, invite_3262("z9hG4bK-i", "Require: 100rel\r\n", None, false), ALICE, 0),
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", "", false), ALICE, 1),
            sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 3),
        ];
        let out = RequireReliable1xxOnRequireRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        assert!(out[0].1.contains("MUST-001/-002"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the plain 180");

        // Rejecting the extension is §3's other half: nothing to answer for.
        let rejected = vec![
            recv(SUT, invite_3262("z9hG4bK-i", "Require: 100rel\r\n", None, false), ALICE, 0),
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", "", false), ALICE, 1),
            sent(SUT, inv_resp_3262(420, "z9hG4bK-i", "Unsupported: 100rel\r\n", false), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 3),
        ];
        assert!(RequireReliable1xxOnRequireRule.check(&rejected).is_empty());
    }

    #[test]
    fn a_relay_lane_is_not_judged_on_the_reliable_provisional_negotiation() {
        // A lane that forwards both directions of one Call-ID relays what the
        // upstream UAS produced; the plain 18x is the upstream's emission.
        let evs = vec![
            recv(SUT, invite_3262("z9hG4bK-a", "Require: 100rel\r\n", None, false), ALICE, 0),
            sent(SUT, invite_3262("z9hG4bK-b", "Require: 100rel\r\n", None, false), BOB, 1),
            sent(SUT, inv_resp_3262(180, "z9hG4bK-a", "", false), ALICE, 2),
        ];
        assert!(RequireReliable1xxOnRequireRule.check(&evs).is_empty());
    }

    #[test]
    fn a_reliable_1xx_without_client_opt_in_is_advisory_on_the_uas_lane() {
        assert!(ReliableNeedsClientOptInRule.force_advisory());
        assert_eq!(
            ReliableNeedsClientOptInRule.subject(),
            HashSet::from([UaRole::Uas]),
            "the UAS is the party §3 binds here",
        );
        let evs = vec![
            recv(SUT, invite_3262("z9hG4bK-i", "", None, false), ALICE, 0),
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 1),
        ];
        let out = ReliableNeedsClientOptInRule.check(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-004"), "{}", out[0].1);

        let opted = vec![
            recv(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), ALICE, 0),
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 1),
        ];
        assert!(ReliableNeedsClientOptInRule.check(&opted).is_empty());
    }

    /// A re-INVITE is an INVITE, so a reliable provisional to one is clean at
    /// this bind too (RFC 3262 §3 scopes the mechanism by method; its To-tag
    /// bar is the proxy's, "unlike a UAS").
    #[test]
    fn a_reliable_1xx_answering_a_re_invite_is_clean() {
        let evs = vec![
            recv(
                SUT,
                invite_3262("z9hG4bK-re", "Supported: 100rel\r\n", Some("bt"), false),
                ALICE,
                0,
            ),
            sent(SUT, inv_resp_3262(180, "z9hG4bK-re", &reliable_rows(1), false), ALICE, 1),
        ];
        assert!(NoReliable1xxOnInDialogRule.check(&evs).is_empty());

        let initial = vec![
            recv(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), ALICE, 0),
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 1),
        ];
        assert!(NoReliable1xxOnInDialogRule.check(&initial).is_empty());

        // The offence the rule is for: a reliable provisional to an UPDATE.
        let update = vec![
            recv(SUT, update_3262("z9hG4bK-u", "Supported: 100rel\r\n"), ALICE, 0),
            sent(SUT, update_resp_3262(183, "z9hG4bK-u", &reliable_rows(1)), ALICE, 1),
        ];
        let out = NoReliable1xxOnInDialogRule.check(&update);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-005") && out[0].1.contains("UPDATE"), "{}", out[0].1);
    }

    #[test]
    fn a_prack_absorbed_by_a_proxy_bind_is_advisory_on_that_bind() {
        assert!(UnmatchedPrackProxiedRule.force_advisory());
        assert_eq!(UnmatchedPrackProxiedRule.subject(), HashSet::from([UaRole::Proxy]));
        let evs = vec![
            recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 0),
            recv(SUT, prack_3262("z9hG4bK-p", "9 1 INVITE", false), ALICE, 1),
        ];
        let out = UnmatchedPrackProxiedRule.check(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "charged to the bind that took the PRACK");
        assert!(out[0].1.contains("MUST-006"), "{}", out[0].1);

        let matched = vec![
            recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 0),
            recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
        ];
        assert!(UnmatchedPrackProxiedRule.check(&matched).is_empty());
    }

    #[test]
    fn a_prack_draws_2xx_on_a_match_and_481_without_one() {
        let matched_but_rejected = vec![
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
            recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
            sent(SUT, prack_resp_3262(481, "z9hG4bK-p"), ALICE, 2),
        ];
        let out = Prack2xxOr481Rule.check(&matched_but_rejected);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-009"), "{}", out[0].1);

        let unmatched_but_accepted = vec![
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
            recv(SUT, prack_3262("z9hG4bK-p", "9 1 INVITE", false), ALICE, 1),
            sent(SUT, prack_resp_3262(200, "z9hG4bK-p"), ALICE, 2),
        ];
        let out = Prack2xxOr481Rule.check(&unmatched_but_accepted);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-010"), "{}", out[0].1);

        let honoured = vec![
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
            recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
            sent(SUT, prack_resp_3262(200, "z9hG4bK-p"), ALICE, 2),
        ];
        assert!(Prack2xxOr481Rule.check(&honoured).is_empty());
    }

    #[test]
    fn the_2xx_over_an_unacked_reliable_offer_is_flagged_at_the_2xx() {
        let early = vec![
            sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(1), true), ALICE, 0),
            sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 1),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
        ];
        let out = Delay2xxOnUnackedReliable1xxWithSdpRule.check_positioned(&early);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-014") && out[0].1.contains("RSeq=1"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the 2xx");

        let waited = vec![
            sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(1), true), ALICE, 0),
            recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", true), ALICE, 1),
            sent(SUT, prack_resp_3262(200, "z9hG4bK-p"), ALICE, 2),
            sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 3),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 4),
        ];
        assert!(Delay2xxOnUnackedReliable1xxWithSdpRule.check(&waited).is_empty());
    }

    #[test]
    fn a_late_prack_rejected_after_the_final_is_flagged() {
        let rejected = vec![
            sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 0),
            recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
            sent(SUT, prack_resp_3262(481, "z9hG4bK-p"), ALICE, 2),
        ];
        let out = PrackAcceptedAfterFinalRule.check(&rejected);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-015"), "{}", out[0].1);

        let accepted = vec![
            sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 0),
            recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
            sent(SUT, prack_resp_3262(200, "z9hG4bK-p"), ALICE, 2),
        ];
        assert!(PrackAcceptedAfterFinalRule.check(&accepted).is_empty());
    }

    #[test]
    fn a_new_reliable_1xx_after_the_final_is_flagged_and_a_retransmit_is_not() {
        let stray = vec![
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
            sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 1),
            sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(2), false), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 3),
        ];
        let out = NoNewReliable1xxAfterFinalRule.check_positioned(&stray);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-016") && out[0].1.contains("RSeq=2"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(3), "offending points at the stray 183");

        let clean = vec![
            sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
            sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 1),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
        ];
        assert!(NoNewReliable1xxAfterFinalRule.check(&clean).is_empty());
    }

    #[test]
    fn pracking_a_100_trying_is_flagged_at_the_prack() {
        let evs = vec![
            sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), BOB, 0),
            recv(SUT, inv_resp_3262(100, "z9hG4bK-i", &reliable_rows(1), false), BOB, 1),
            sent(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), BOB, 2),
        ];
        let out = NoPrackOf100TryingRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-019"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(3), "offending points at the PRACK");

        let ignored = vec![
            sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), BOB, 0),
            recv(SUT, inv_resp_3262(100, "z9hG4bK-i", &reliable_rows(1), false), BOB, 1),
        ];
        assert!(NoPrackOf100TryingRule.check(&ignored).is_empty());
    }

    #[test]
    fn a_bodiless_prack_for_a_1xx_offer_is_named_at_either_end() {
        assert!(PrackAnswers1xxOfferRule.force_advisory());
        assert_eq!(
            PrackAnswers1xxOfferRule.subject(),
            HashSet::from([UaRole::Uac, UaRole::Uas])
        );
        // The bind SENT the offending PRACK: charged party and vantage are one.
        let sender = vec![
            recv(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(1), true), BOB, 0),
            sent(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), BOB, 1),
        ];
        let out = PrackAnswers1xxOfferRule.check(&sender);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-025"), "{}", out[0].1);

        // The bind only TOOK it: the offender is no recorded bind at all, and
        // the recording is the only place that stream is checked.
        let taker = vec![
            sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(1), true), ALICE, 0),
            recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
        ];
        let out = PrackAnswers1xxOfferRule.check(&taker);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);

        // The standard setup: the INVITE carried the offer, so the 183's body
        // is the ANSWER and the bodiless PRACK owes nothing.
        let standard = vec![
            sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, true), BOB, 0),
            recv(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(1), true), BOB, 1),
            sent(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), BOB, 2),
        ];
        assert!(PrackAnswers1xxOfferRule.check(&standard).is_empty());
    }

    // ── the §13.2 / RFC 3264 offer/answer family ────────────────────────────

    const AUDIO_OFFER: &str = "v=0\r\n\
o=alice 424242 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";

    const AUDIO_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n";

    const T38_OFFER: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=image 27500 udptl t38\r\n";

    const T38_ANSWER: &str = "v=0\r\n\
o=bob 117241 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=image 60788 udptl t38\r\n";

    /// The harness default body that must never ride an ACK: a fresh session
    /// (`o=bob 1 1`) offering audio over RTP/AVP.
    const STRAY_ANSWER: &str = "v=0\r\n\
o=bob 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 20000 RTP/AVP 0\r\n";

    /// A request carrying an optional SDP body.
    fn req_sdp(
        method: &str,
        branch: &str,
        cseq: u32,
        ttag: Option<&str>,
        body: Option<&str>,
    ) -> Vec<u8> {
        let to = match ttag {
            Some(t) => format!("<{B}>;tag={t}"),
            None => format!("<{B}>"),
        };
        let body = body.unwrap_or("");
        let ctype = if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
        let mut v = format!(
            "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             {ctype}Content-Length: {}\r\n\r\n",
            body.len(),
        )
        .into_bytes();
        v.extend_from_slice(body.as_bytes());
        v
    }

    /// A response carrying an optional SDP body.
    fn resp_sdp(
        status: u16,
        cseq: u32,
        method: &str,
        branch: &str,
        body: Option<&str>,
    ) -> Vec<u8> {
        let body = body.unwrap_or("");
        let ctype = if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
        let mut v = format!(
            "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             {ctype}Content-Length: {}\r\n\r\n",
            body.len(),
        )
        .into_bytes();
        v.extend_from_slice(body.as_bytes());
        v
    }

    /// INVITE(offer) → 180(answer) → 200 → ACK, then a T.38 re-INVITE(offer) →
    /// 200(answer) → ACK, all from the CALLER's bind. The ACK bodies are the
    /// caller's choice.
    fn t38_call(
        first_ack: Option<&str>,
        second_ack: Option<&str>,
    ) -> Vec<Stamped<SignalingNetworkEvent>> {
        vec![
            sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), BOB, 0),
            recv(ALICE, resp_sdp(180, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), BOB, 1),
            recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), BOB, 2),
            sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), first_ack), BOB, 3),
            sent(ALICE, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(T38_OFFER)), BOB, 4),
            recv(ALICE, resp_sdp(200, 3, "INVITE", "z9hG4bK-i3", Some(T38_ANSWER)), BOB, 5),
            sent(ALICE, req_sdp("ACK", "z9hG4bK-a3", 3, Some("bt"), second_ack), BOB, 6),
        ]
    }

    #[test]
    fn an_ack_body_on_a_completed_round_is_flagged_at_its_wire_position() {
        assert!(AckBodyAfterCompleteOfferAnswerRule.check(&t38_call(None, None)).is_empty());

        let out = AckBodyAfterCompleteOfferAnswerRule.check_positioned(&t38_call(
            None,
            Some(STRAY_ANSWER),
        ));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, ALICE, "charged to the lane that sent the ACK");
        assert!(out[0].1.contains("audio/RTP/AVP"), "{}", out[0].1);
        assert!(out[0].1.contains("§13.2.2.4"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(7), "offending points at the ACK that carried the body");

        // The establishing round is closed by the 180, not by the bodyless 200.
        let first = AckBodyAfterCompleteOfferAnswerRule.check(&t38_call(Some(STRAY_ANSWER), None));
        assert_eq!(first.len(), 1, "{first:?}");
    }

    #[test]
    fn a_delayed_offer_ack_carries_the_answer_and_is_clean() {
        // INVITE without SDP → the 2xx holds the offer → the ACK holds the answer.
        let evs = vec![
            sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, None), BOB, 0),
            recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_OFFER)), BOB, 1),
            sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), Some(AUDIO_ANSWER)), BOB, 2),
        ];
        assert!(AckBodyAfterCompleteOfferAnswerRule.check(&evs).is_empty());
    }

    /// The same as [`resp_sdp`], made reliable: `Require: 100rel` and an
    /// `RSeq`, so its description binds (RFC 3262 §5).
    fn reliable_resp_sdp(status: u16, cseq: u32, branch: &str, body: Option<&str>) -> Vec<u8> {
        let plain = resp_sdp(status, cseq, "INVITE", branch, body);
        let text = String::from_utf8(plain).unwrap();
        text.replacen("CSeq:", "Require: 100rel\r\nRSeq: 1\r\nCSeq:", 1).into_bytes()
    }

    #[test]
    fn a_final_leaving_the_offer_unanswered_is_flagged_at_its_wire_position() {
        assert!(!Final2xxAnswersTheOfferRule.force_advisory(), "a MUST gates");

        // Judged from the ANSWERER's bind: it takes the offer and ends the
        // round with a 200 stating no plan of its own.
        let clean = vec![
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
            sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 1),
            recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
        ];
        assert!(Final2xxAnswersTheOfferRule.check(&clean).is_empty());

        let silent = vec![
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
            sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 1),
            recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
        ];
        let out = Final2xxAnswersTheOfferRule.check_positioned(&silent);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, BOB, "reported on the answerer's own lane");
        assert_eq!(out[0].3, Some(BOB.to_string()), "charged to the answerer");
        assert!(out[0].1.contains("audio/RTP/AVP"), "{}", out[0].1);
        assert!(out[0].1.contains("§13.2.1"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the 200 that answered nothing");
    }

    #[test]
    fn a_reliable_provisional_answer_discharges_the_final() {
        let evs = vec![
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
            sent(BOB, reliable_resp_sdp(183, 1, "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 1),
            recv(BOB, req_sdp("PRACK", "z9hG4bK-p1", 2, Some("bt"), None), ALICE, 2),
            sent(BOB, resp_sdp(200, 2, "PRACK", "z9hG4bK-p1", None), ALICE, 3),
            sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 4),
            recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 5),
        ];
        assert!(Final2xxAnswersTheOfferRule.check(&evs).is_empty(), "RFC 3262 §5 answered it");
    }

    #[test]
    fn a_silent_answerer_is_named_on_the_lane_that_took_it() {
        // The callee is no recorded bind; the caller's lane is where the
        // recording checks the round at all.
        let evs = vec![
            sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), BOB, 0),
            recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), BOB, 1),
            sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), BOB, 2),
        ];
        let out = Final2xxAnswersTheOfferRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, ALICE, "reported at the vantage that took the 200");
        assert_eq!(out[0].3, Some(BOB.to_string()), "charged to the answerer all the same");
    }

    #[test]
    fn a_failure_final_and_a_cancelled_round_owe_no_answer() {
        let rejected = vec![
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
            sent(BOB, resp_sdp(486, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 1),
            recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
        ];
        assert!(Final2xxAnswersTheOfferRule.check(&rejected).is_empty());

        let cancelled = vec![
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
            sent(BOB, resp_sdp(180, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 1),
            recv(BOB, req_sdp("CANCEL", "z9hG4bK-i1", 1, None, None), ALICE, 2),
            sent(BOB, resp_sdp(200, 1, "CANCEL", "z9hG4bK-i1", None), ALICE, 3),
            sent(BOB, resp_sdp(487, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 4),
            recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 5),
        ];
        assert!(Final2xxAnswersTheOfferRule.check(&cancelled).is_empty());
    }

    #[test]
    fn an_answer_re_typing_the_offered_stream_is_flagged_on_the_answerer() {
        // Judged from the ANSWERER's bind: it takes the offer and sends back a
        // stream of another type.
        let clean = vec![
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(T38_OFFER)), ALICE, 0),
            sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(T38_ANSWER)), ALICE, 1),
            recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
        ];
        assert!(AnswerStreamMatchesOfferRule.check(&clean).is_empty());

        let retyped = vec![
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(T38_OFFER)), ALICE, 0),
            sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 1),
            recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
        ];
        let out = AnswerStreamMatchesOfferRule.check_positioned(&retyped);
        assert_eq!(out.len(), 1, "one answer is one finding: {out:?}");
        assert_eq!(out[0].0, BOB);
        assert!(out[0].1.contains("m=[0]"), "{}", out[0].1);
        assert!(out[0].1.contains("image 27500 udptl"), "{}", out[0].1);
        assert!(out[0].1.contains("audio 60788 RTP/AVP"), "{}", out[0].1);
        assert!(out[0].1.contains("§6"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the answering 200");
    }

    #[test]
    fn an_origin_that_leaves_the_session_is_advisory_and_positioned() {
        assert!(SdpOriginContinuityRule.force_advisory());
        assert_eq!(SdpOriginContinuityRule.subject(), HashSet::from([UaRole::Uac, UaRole::Uas]));
        assert!(SdpOriginContinuityRule.check(&t38_call(None, None)).is_empty());

        let out = SdpOriginContinuityRule.check_positioned(&t38_call(None, Some(STRAY_ANSWER)));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, ALICE);
        assert!(out[0].1.contains("o=bob 1 1 IN IP4 127.0.0.1"), "{}", out[0].1);
        assert!(out[0].1.contains("o=alice 424242 2 IN IP4 10.0.0.1"), "{}", out[0].1);
        assert!(out[0].1.contains("different session"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(7), "offending points at the ACK carrying the foreign origin");
    }

    #[test]
    fn a_relay_lane_carrying_both_agents_origins_is_not_judged() {
        // A transparent proxy forwards alice's offer AND bob's answer on one
        // bind, so its sent stream interleaves o=alice and o=bob for one
        // Call-ID. That is the two agents' continuity, never the relay's.
        let inv = req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER));
        let ok = resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER));
        let evs = vec![
            recv(SUT, inv.clone(), ALICE, 0),
            sent(SUT, inv, BOB, 1),
            recv(SUT, ok.clone(), BOB, 2),
            sent(SUT, ok, ALICE, 3),
        ];
        assert!(SdpOriginContinuityRule.check(&evs).is_empty(), "{:?}", SdpOriginContinuityRule.check(&evs));
        assert!(AnswerStreamMatchesOfferRule.check(&evs).is_empty());
        assert!(AckBodyAfterCompleteOfferAnswerRule.check(&evs).is_empty());
    }

    // ── the RFC 3264 offer/answer-model rules ───────────────────────────────

    /// One audio stream, `sendrecv`, PT 0 bound to PCMU and PT 96 to opus.
    const OFFER_1AUDIO: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

    const ANSWER_1AUDIO: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

    /// An answer wrong four ways at once: an added stream, a rewritten `t=`,
    /// video where audio was offered, and a bare port-0 rejection.
    const BAD_ANSWER: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=100 200\r\n\
m=video 50000 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n\
m=audio 0 RTP/AVP\r\n";

    const OFFER_2MEDIA: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 49172 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

    const ANSWER_2MEDIA: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 50002 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

    /// INVITE(offer) → 200(answer) → ACK on the CALLER's bind: the caller sends
    /// the offer and TAKES the answer, so the answerer is no recorded bind.
    fn sdp_round(offer: &str, answer: &str) -> Vec<Stamped<SignalingNetworkEvent>> {
        vec![
            sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(offer)), BOB, 0),
            recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(answer)), BOB, 1),
            sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), BOB, 2),
        ]
    }

    #[test]
    fn a_second_offer_over_an_unanswered_one_is_advisory_at_either_end() {
        assert!(NoNewOfferWhileOfferPendingRule.force_advisory());
        assert!(NoNewOfferWhileOfferPendingRule.check(&t38_call(None, None)).is_empty());

        let glare = vec![
            sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), BOB, 0),
            sent(ALICE, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(T38_OFFER)), BOB, 1),
        ];
        let out = NoNewOfferWhileOfferPendingRule.check_positioned(&glare);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, ALICE, "charged to the lane that sent the second offer");
        assert!(out[0].1.contains("MUST-002"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2), "offending points at the second offer");

        // The same act at the lane that only TOOK the two offers: the offender
        // is no recorded bind, and the recording is where it is checked at all.
        let taker = vec![
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(T38_OFFER)), ALICE, 1),
        ];
        let out = NoNewOfferWhileOfferPendingRule.check(&taker);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, BOB, "reported on the lane that took it");
    }

    #[test]
    fn a_bad_answer_is_named_at_the_offerers_lane() {
        assert!(AnswerMLineCountMatchesOfferRule
            .check(&sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO))
            .is_empty());

        let evs = sdp_round(OFFER_1AUDIO, BAD_ANSWER);
        for (rule, must, quote) in [
            (
                Box::new(AnswerMLineCountMatchesOfferRule) as Box<dyn CrossMessageAuditRule>,
                "MUST-018",
                "video/RTP/AVP",
            ),
            (Box::new(AnswerTLineEqualsOfferRule), "MUST-019", "100 200"),
            (Box::new(AnswerMediaTypeMatchesOfferRule), "MUST-022", "m=[0]"),
            (Box::new(RejectedStreamMinimalAnswerRule), "MUST-021", "m=[1]"),
        ] {
            let out = rule.check_positioned(&evs);
            assert_eq!(out.len(), 1, "{}: {out:?}", rule.name());
            assert_eq!(out[0].0, ALICE, "{}: reported on the lane that took it", rule.name());
            assert!(out[0].1.contains(must), "{}: {}", rule.name(), out[0].1);
            assert!(out[0].1.contains(quote), "{}: {}", rule.name(), out[0].1);
            assert_eq!(out[0].2, Some(2), "{}: offending points at the answer", rule.name());
        }

        // The answerer's OWN lane names it too — it is the charged party.
        let at_answerer = vec![
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(OFFER_1AUDIO)), ALICE, 0),
            sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(BAD_ANSWER)), ALICE, 1),
            recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
        ];
        let out = AnswerMLineCountMatchesOfferRule.check(&at_answerer);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, BOB);
    }

    #[test]
    fn the_direction_and_zero_port_pairings_are_advisory() {
        assert!(DirectionPairValidRule.force_advisory());
        assert!(ZeroPortPropagationRule.force_advisory());

        let held = OFFER_1AUDIO.replace("a=sendrecv", "a=inactive");
        let out = DirectionPairValidRule.check_positioned(&sdp_round(&held, ANSWER_1AUDIO));
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-023"), "{}", out[0].1);
        assert!(out[0].1.contains("answer 'sendrecv' invalid for offer 'inactive'"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2));

        let disabled = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
        assert!(DirectionPairValidRule.check(&sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO)).is_empty());
        let out = ZeroPortPropagationRule.check_positioned(&sdp_round(&disabled, ANSWER_1AUDIO));
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("MUST-044"), "{}", out[0].1);
        assert!(out[0].1.contains("m=[0] answered port=50000"), "{}", out[0].1);
    }

    #[test]
    fn a_shrinking_re_offer_is_charged_only_to_the_lane_that_sent_it() {
        let mut evs = sdp_round(OFFER_2MEDIA, ANSWER_2MEDIA);
        evs.push(sent(
            ALICE,
            req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(OFFER_1AUDIO)),
            BOB,
            3,
        ));
        let out = ReOfferMLineCountMonotonicRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, ALICE);
        assert!(out[0].1.contains("MUST-042"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(4), "offending points at the shrinking re-offer");

        // The lane that only TOOK that stream authored nothing: the rule judges
        // what a lane SENT, so it is charged at the sender's vantage alone.
        let taker = vec![
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(OFFER_2MEDIA)), ALICE, 0),
            sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(ANSWER_2MEDIA)), ALICE, 1),
            recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
            recv(BOB, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(OFFER_1AUDIO)), ALICE, 3),
        ];
        assert!(ReOfferMLineCountMonotonicRule.check(&taker).is_empty());
    }

    #[test]
    fn a_rebound_payload_type_is_named_at_either_end() {
        assert!(PayloadTypeMappingStableRule
            .check(&sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO))
            .is_empty());

        let reoffer = OFFER_1AUDIO.replace("a=rtpmap:96 opus/48000/2", "a=rtpmap:96 H264/90000");
        let mut evs = sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO);
        evs.push(sent(ALICE, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(&reoffer)), BOB, 3));
        let out = PayloadTypeMappingStableRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, ALICE);
        assert!(out[0].1.contains("MUST-047"), "{}", out[0].1);
        assert!(out[0].1.contains("payload-type 96 was 'opus/48000/2' now 'H264/90000'"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(4));
    }

    #[test]
    fn a_relay_lane_carrying_a_negotiation_is_not_judged_on_it() {
        // The relay forwards both agents' descriptions on one Call-ID and has
        // no offer/answer state of its own, so the bad answer it passes through
        // is the upstream's and is flagged on the upstream lane.
        let inv = req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(OFFER_1AUDIO));
        let ok = resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(BAD_ANSWER));
        let evs = vec![
            recv(SUT, inv.clone(), ALICE, 0),
            sent(SUT, inv, BOB, 1),
            recv(SUT, ok.clone(), BOB, 2),
            sent(SUT, ok, ALICE, 3),
        ];
        let rules: Vec<Box<dyn CrossMessageAuditRule>> = vec![
            Box::new(NoNewOfferWhileOfferPendingRule),
            Box::new(AnswerMLineCountMatchesOfferRule),
            Box::new(AnswerTLineEqualsOfferRule),
            Box::new(AnswerMediaTypeMatchesOfferRule),
            Box::new(DirectionPairValidRule),
            Box::new(RejectedStreamMinimalAnswerRule),
            Box::new(ReOfferMLineCountMonotonicRule),
            Box::new(ZeroPortPropagationRule),
            Box::new(PayloadTypeMappingStableRule),
        ];
        for rule in rules {
            let out = rule.check(&evs);
            assert!(out.is_empty(), "{}: {out:?}", rule.name());
        }
    }

    // ── the peer seam: one message at one vantage ───────────────────────────
    //
    // What this adapter adds to the per-message bodies: which END of the
    // offending message a finding is reported on, the `{Proxy}` narrowing, the
    // advisory flag, and the wire position a peer finding could never state.

    /// A request whose top Via branch carries no magic cookie.
    fn legacy_branch_invite() -> Vec<u8> {
        format!(
            "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=legacy-2543\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Contact: <{A}>\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A sender-minted defect is charged to the lane that WROTE it, at the wire
    /// position of the offending message — and a lane that merely took those
    /// bytes is charged nothing.
    #[test]
    fn a_minted_defect_is_charged_to_its_sender_and_positioned() {
        let out = BranchPrefixRule.check_positioned(&[sent(
            ALICE,
            legacy_branch_invite(),
            BOB,
            0,
        )]);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, ALICE, "the sender is charged");
        assert!(out[0].1.contains("magic cookie"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(1), "the offending wire entry");

        let taken = BranchPrefixRule.check(&[recv(SUT, legacy_branch_invite(), ALICE, 0)]);
        assert!(taken.is_empty(), "a lane that only TOOK the bytes minted nothing: {taken:?}");
    }

    /// A rule that judges what a lane's PEER sent IT is reported on the lane
    /// that took it — the recording is the only place that peer's stream is
    /// checked at all.
    #[test]
    fn a_peer_side_defect_is_reported_on_the_lane_that_took_it() {
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            recv(SUT, req("CANCEL", "z9hG4bK-other", 1, None), ALICE, 1),
        ];
        let out = CancelViaBranchRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "reported on the lane that took the CANCEL");
        assert!(out[0].1.contains("matches no received INVITE"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(2));

        // The same CANCEL matching the INVITE's branch discharges it.
        let clean = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            recv(SUT, req("CANCEL", "z9hG4bK-i", 1, None), ALICE, 1),
        ];
        assert!(CancelViaBranchRule.check(&clean).is_empty());
    }

    /// A B2BUA Record-Route is judged only on a lane that declared itself a
    /// proxy — the one that could be mistaken for one.
    #[test]
    fn the_record_route_rule_is_narrowed_to_declared_proxies() {
        let rr = format!(
            "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-i\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Record-Route: <sip:127.0.0.1:5080;callRef=7;leg=a>\r\n\
             Contact: <{A}>\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes();
        let named = |roles: HashSet<UaRole>| -> Vec<RfcFinding> {
            let evs =
                vec![bind_roles(SUT, roles, 0), sent(SUT, rr.clone(), BOB, 1)];
            evaluate_rfc_findings(&evs)
                .into_iter()
                .filter(|f| f.rule == NoRecordRouteFromUaRule.name())
                .collect()
        };
        let as_proxy = named(HashSet::from([UaRole::Proxy]));
        assert_eq!(as_proxy.len(), 1, "{as_proxy:?}");
        assert!(as_proxy[0].detail.contains("MUST NOT use Record-Route"), "{}", as_proxy[0].detail);
        assert!(named(HashSet::from([UaRole::Uas])).is_empty(), "a plain UAS lane is not judged");
    }

    /// A UAS To-tag flip is SURFACED and never gates: a forking B2BUA answering
    /// off a later early dialog is indistinguishable from one per branch.
    #[test]
    fn a_uas_tag_flip_is_advisory() {
        let evs = vec![
            sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 0),
            sent(SUT, resp(200, 1, "INVITE", "bt2", "z9hG4bK-i"), ALICE, 1),
        ];
        let f: Vec<RfcFinding> = evaluate_rfc_findings(&evs)
            .into_iter()
            .filter(|f| f.rule == TagConsistencyRule.name())
            .collect();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].lane, SUT);
        assert!(f[0].advisory, "the tag flip is informational");
        assert!(f[0].detail.contains("To-tag mismatch"), "{}", f[0].detail);
    }

    // ── rack-without-known-invite (merged, via this adapter) ────────────────
    //
    // The merged rule charges the PRACK's SENDER, so the fixtures are the
    // sender's own lane: the INVITE it opened and the PRACK it then emitted.

    fn prack_with_rack(rack: &str) -> Vec<u8> {
        format!(
            "PRACK {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-p\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-rack@127.0.0.1\r\n\
             CSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn rack_invite(branch: &str, cseq: u32) -> Vec<u8> {
        format!(
            "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-rack@127.0.0.1\r\n\
             CSeq: {cseq} INVITE\r\n\
             Max-Forwards: 70\r\n\
             Contact: <{A}>\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn rack_matching_opened_invite_is_clean() {
        // RAck "1 1 INVITE" → references INVITE CSeq 1, which alice opened.
        let evs = vec![
            sent(ALICE, rack_invite("z9hG4bK-i", 1), BOB, 0),
            sent(ALICE, prack_with_rack("1 1 INVITE"), BOB, 1),
        ];
        assert!(RackWithoutKnownInviteRule.check(&evs).is_empty());
    }

    #[test]
    fn rack_referencing_unknown_invite_is_flagged() {
        // RAck references INVITE CSeq 7 — never opened.
        let evs = vec![
            sent(ALICE, rack_invite("z9hG4bK-i", 1), BOB, 0),
            sent(ALICE, prack_with_rack("1 7 INVITE"), BOB, 1),
        ];
        let f = RackWithoutKnownInviteRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].0, ALICE, "the PRACK's sender is charged");
        assert!(f[0].1.contains("names no INVITE this bind opened"), "{}", f[0].1);
    }

    /// The finding quotes the WHOLE RAck (a bare CSeq-num reads as the PRACK's
    /// own CSeq) and, with one INVITE opened, names the header the sender
    /// should have emitted.
    #[test]
    fn rack_finding_quotes_the_header_and_names_the_expected_value() {
        let evs = vec![
            sent(ALICE, rack_invite("z9hG4bK-i", 1), BOB, 0),
            sent(ALICE, prack_with_rack("12800221 101 INVITE"), BOB, 1),
        ];
        let f = RackWithoutKnownInviteRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("\"RAck: 12800221 101 INVITE\""), "{}", f[0].1);
        assert!(f[0].1.contains("expected \"RAck: 12800221 1 INVITE\""), "{}", f[0].1);
        assert!(f[0].1.contains("NOT from the PRACK's"), "{}", f[0].1);
    }

    /// Several INVITEs opened (initial + re-INVITE) leave no single correct
    /// value, so the finding lists what was opened and offers no repair.
    #[test]
    fn rack_finding_offers_no_expected_value_when_ambiguous() {
        let evs = vec![
            sent(ALICE, rack_invite("z9hG4bK-i", 1), BOB, 0),
            sent(ALICE, rack_invite("z9hG4bK-i2", 5), BOB, 1),
            sent(ALICE, prack_with_rack("1 101 INVITE"), BOB, 2),
        ];
        let f = RackWithoutKnownInviteRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("opened INVITE CSeq: 1, 5"), "{}", f[0].1);
        assert!(!f[0].1.contains("expected"), "{}", f[0].1);
    }

    /// A vantage that only TOOK the PRACK charges nobody it can see: the sender
    /// is a foreign endpoint, and the live policy charges the vantage.
    #[test]
    fn rack_on_a_receiving_bind_charges_nothing() {
        let evs = vec![
            recv(SUT, rack_invite("z9hG4bK-i", 1), ALICE, 0),
            recv(SUT, prack_with_rack("1 7 INVITE"), ALICE, 1),
        ];
        assert!(RackWithoutKnownInviteRule.check(&evs).is_empty());
    }

    // ── the §17 / §13.3.1.4 / RFC 3262 §3 rung family ────────────────────────

    /// The same datagram with its From and To rows swapped: every header the
    /// first copy carried, and not the same response.
    fn recomposed(raw: Vec<u8>) -> Vec<u8> {
        let text = String::from_utf8(raw).unwrap();
        let mut rows: Vec<&str> = text.split("\r\n").collect();
        rows.swap(2, 3);
        rows.join("\r\n").into_bytes()
    }

    /// The violation, at its wire position: the SUT answered 200 and its
    /// repeat was re-composed rather than repeated.
    #[test]
    fn a_re_composed_2xx_rung_is_charged_to_the_lane_that_sent_it() {
        let ok = resp(200, 1, "INVITE", "bt", "z9hG4bK-i");
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, ok.clone(), ALICE, 1),
            sent(SUT, recomposed(ok), ALICE, 2),
        ];
        let out = RungByteIdenticalRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "attributed to the lane that re-composed its rung");
        assert_eq!(out[0].3.as_deref(), Some(SUT));
        let detail = &out[0].1;
        assert!(detail.contains("2xx final") && detail.contains("200 (INVITE)"), "{detail}");
        assert!(detail.contains("z9hG4bK-i") && detail.contains("cid-1@127.0.0.1"), "{detail}");
        assert!(detail.contains("copy 1 of 2") && detail.contains("in the head at byte"), "{detail}");
        assert!(detail.contains("first `From: <sip:alice@127.0.0.1>;tag=at`"), "{detail}");
        assert!(detail.contains("this copy `To: <sip:bob@127.0.0.1>;tag=bt`"), "{detail}");
        assert!(detail.contains("§13.3.1.4"), "{detail}");
        assert_eq!(out[0].2, Some(3), "offending points at the divergent copy");
        assert!(!RungByteIdenticalRule.force_advisory(), "a divergent rung gates");
    }

    /// The LB proxy's own Record-Route cookie may differ between two copies of
    /// one forwarded INVITE: the lane key IS the emitter's `ip:port`, spelled
    /// as the proxy's Record-Route URI spells it, so the rule masks that row —
    /// and no other.
    #[test]
    fn a_re_stamped_own_record_route_cookie_is_not_charged_but_a_foreign_row_is() {
        const WORKER: &str = "127.0.0.1:5091";
        let forwarded = |cookie: &str, foreign: &str| {
            format!(
                "INVITE {B} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-p;rport\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i;received=127.0.0.1;rport=5060\r\n\
                 Record-Route: <sip:127.0.0.1:5080;outbound;lr>\r\n\
                 Record-Route: <sip:127.0.0.1:5080;w_pri=b1;w_bak={cookie};lr>\r\n\
                 Record-Route: <sip:10.0.0.7:5060;{foreign}>\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 1 INVITE\r\n\
                 Max-Forwards: 69\r\n\
                 Content-Length: 0\r\n\r\n"
            )
            .into_bytes()
        };
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, forwarded("b2", "lr"), WORKER, 1),
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 2),
            sent(SUT, forwarded("\"\"", "lr"), WORKER, 3),
        ];
        assert!(RungByteIdenticalRule.check_positioned(&evs).is_empty());

        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, forwarded("b2", "lr"), WORKER, 1),
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 2),
            sent(SUT, forwarded("\"\"", "x=1;lr"), WORKER, 3),
        ];
        let out = RungByteIdenticalRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
        let detail = &out[0].1;
        assert!(detail.contains("INVITE request"), "{detail}");
        assert!(detail.contains("first `Record-Route: <sip:10.0.0.7:5060;lr>`"), "{detail}");
        assert!(detail.contains("this copy `Record-Route: <sip:10.0.0.7:5060;x=1;lr>`"), "{detail}");
    }

    /// The obliged behaviour: the same bytes again is nothing to report, and
    /// the projection's repeat mark does not change that.
    #[test]
    fn a_byte_identical_2xx_rung_is_not_charged() {
        let ok = resp(200, 1, "INVITE", "bt", "z9hG4bK-i");
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, ok.clone(), ALICE, 1),
            sent(SUT, ok.clone(), ALICE, 2),
            sent(SUT, ok, ALICE, 3),
        ];
        assert!(RungByteIdenticalRule.check_positioned(&evs).is_empty());
    }

    /// A B2BUA relaying two forks' 2xx sends two messages under one branch and
    /// CSeq; the tag tells them apart, and neither is a rung of the other.
    #[test]
    fn another_forks_2xx_is_not_a_rung() {
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            sent(SUT, resp(200, 1, "INVITE", "bt2", "z9hG4bK-i"), ALICE, 2),
        ];
        assert!(RungByteIdenticalRule.check_positioned(&evs).is_empty());
    }

    /// The far end's re-composed rung is not this bind's: the rule charges the
    /// emitter, and the live policy surfaces a finding only where the charged
    /// party IS the vantage.
    #[test]
    fn a_re_composed_rung_the_peer_sent_is_not_charged_to_this_bind() {
        let bye = req("BYE", "z9hG4bK-b", 2, Some("bt"));
        let evs = vec![
            recv(SUT, bye.clone(), ALICE, 0),
            recv(SUT, recomposed(bye), ALICE, 1),
        ];
        assert!(RungByteIdenticalRule.check_positioned(&evs).is_empty());
    }

    /// A reliable provisional the SUT re-sent under the same `RSeq` with other
    /// bytes is charged under RFC 3262 §3; the next `RSeq` is another message.
    #[test]
    fn a_re_composed_reliable_provisional_rung_is_charged() {
        let reliable = |rseq: u32| {
            format!(
                "SIP/2.0 183 Session Progress\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 1 INVITE\r\n\
                 Require: 100rel\r\n\
                 RSeq: {rseq}\r\n\
                 Content-Length: 0\r\n\r\n"
            )
            .into_bytes()
        };
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, reliable(1), ALICE, 1),
            sent(SUT, recomposed(reliable(1)), ALICE, 2),
            sent(SUT, reliable(2), ALICE, 3),
        ];
        let out = RungByteIdenticalRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].1.contains("reliable provisional 183 RSeq 1 (INVITE)"), "{}", out[0].1);
        assert!(out[0].1.contains("RFC 3262 §3"), "{}", out[0].1);
        assert_eq!(out[0].2, Some(3));
    }
}
