//! The live adapter: the recorded event channel projected into the
//! `rfc-rules` wire model, per bind, and the merged rules surfaced as
//! cross-message audit findings.
//!
//! Observation policy lives here, not in the rules: the harness controls
//! shutdown and drains transit before close, so every view is CLOSED — an
//! absence at end-of-stream decides immediately, and the rules' capture-side
//! observability windows collapse. Consumer policy: a finding is
//! surfaced only when it is `Violated` and not relay-attributed, at the
//! vantage its rule's row names — an undecidable occasion stays silent.
//!
//! Projection reads each arrival's production-time [`WireStamp`](crate::WireStamp)
//! (parse-once + the §17.2 repeat mark) and dedups SENDS by transaction key
//! here, at projection — the send-side twin of that stamp. A copy is a repeat
//! only when the audit view holds an earlier copy: the first copy the view
//! holds of a chain whose first sighting it does not (modeled loss) is new.
//!
//! Each rule is surfaced by its `Live` row: vantage, skipped lanes, roles,
//! severity and detail line, the detail a fn named after the rule's token. A
//! rule with no row is surfaced at the emitter with a generic detail line.

use std::collections::{HashMap, HashSet};

use layer_harness::{LaneKey, Stamped};
use sip_message::{SipMessage, SipParser};

use rfc_rules::rules::Obligation;
use rfc_rules::{Decision, Kind, Msg, Observation, RuleId, WireView};

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
    // The repeat chains (keyed by their first sighting's seq) whose first
    // copy this view holds. A chain whose first sighting is not in the view
    // (modeled loss) starts at the first copy that is: the endpoint took it
    // as new.
    let mut held_chains: std::collections::HashSet<u64> = std::collections::HashSet::new();
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
                let chain = wire.repeat_of.unwrap_or(s.seq);
                (
                    bind_key,
                    parsed.clone(),
                    !held_chains.insert(chain),
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
        let at_us = (s.at_ms * 1_000).max(view.msgs.last().map_or(0, |m| m.at_us + 1));
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
/// the `vantage` predicate accepts for the bind the view belongs to. Every
/// non-relayed occasion at that vantage, violated or not, is noted for the
/// run's rule population.
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
            if f.relayed || !vantage(&f, &view.bind) {
                continue;
            }
            crate::rfc_audit::occasions::note(&view.bind, &f);
            if !f.violated() {
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

/// Where the live audit reports a rule's finding. The charged party is always
/// the finding's `emitter`; the vantage is the lane that REPORTS it.
#[derive(Clone, Copy)]
enum Vantage {
    /// The emitter's own lane: what a sender mints or owes.
    Emitter,
    /// The taker's lane: a rule that audits what a bind's PEER sent it. The
    /// recording is the only place that stream is checked, so it is reported on
    /// the lane that took it, where a scoped waiver's party attribution still
    /// resolves to the true offender off the offending entry's sender.
    Taker,
    /// Both ends of the offending message: a two-party negotiation, where an
    /// offence whose sender is no recorded bind is still named on the lane that
    /// took it.
    EitherEnd,
    /// Once, at the taker where it is a recorded bind and at the emitter
    /// otherwise: an omission the taker is OWED, whose lane is where the missing
    /// message had to arrive.
    TakerElseEmitter,
}

/// The lanes a rule leaves unjudged.
#[derive(Clone, Copy)]
enum Skip {
    None,
    /// Lanes that forward both directions of one Call-ID: they relay what the
    /// upstream authored, which is flagged on the upstream lane. Recording
    /// granularity: a lane classified relay in any dialog is unjudged throughout.
    RelayLanes,
    /// Lanes declared `{Proxy}` only: a transparent relay forwards the
    /// upstream's request at the upstream's timing.
    ProxyOnlyLanes,
}

impl Skip {
    fn lanes(self, events: &[Stamped<SignalingNetworkEvent>]) -> HashSet<LaneKey> {
        match self {
            Skip::None => HashSet::new(),
            Skip::RelayLanes => relay_lanes(events),
            Skip::ProxyOnlyLanes => crate::rfc_audit::bind_roles_of(events)
                .into_iter()
                .filter(|(_, roles)| roles.len() == 1 && roles.contains(&UaRole::Proxy))
                .map(|(lane, _)| lane)
                .collect(),
        }
    }
}

/// How the live audit surfaces one rule: its vantage, the lanes it skips, the
/// roles it applies to, whether it only advises, which findings it admits, and
/// the detail line a finding reads as. A rule with no row is surfaced at the
/// emitter with [`generic_detail`].
#[derive(Clone, Copy)]
struct Live {
    id: RuleId,
    vantage: Vantage,
    skip: Skip,
    subject: &'static [UaRole],
    advisory: bool,
    admits: fn(&rfc_rules::Finding) -> bool,
    detail: Detail,
}

type Detail = fn(&rfc_rules::Finding, &str) -> String;

impl Live {
    const fn emitter(id: RuleId, detail: Detail) -> Live {
        Live::at(Vantage::Emitter, id, detail)
    }

    const fn taker(id: RuleId, detail: Detail) -> Live {
        Live::at(Vantage::Taker, id, detail)
    }

    const fn either_end(id: RuleId, detail: Detail) -> Live {
        Live::at(Vantage::EitherEnd, id, detail)
    }

    const fn taker_else_emitter(id: RuleId, detail: Detail) -> Live {
        Live::at(Vantage::TakerElseEmitter, id, detail)
    }

    const fn at(vantage: Vantage, id: RuleId, detail: Detail) -> Live {
        Live {
            id,
            vantage,
            skip: Skip::None,
            subject: &[],
            advisory: false,
            admits: |_| true,
            detail,
        }
    }

    /// Applies to binds declaring one of `roles` (every role by default).
    const fn roles(mut self, roles: &'static [UaRole]) -> Live {
        self.subject = roles;
        self
    }

    /// Reported, never gating.
    const fn advisory(mut self) -> Live {
        self.advisory = true;
        self
    }

    const fn skip(mut self, skip: Skip) -> Live {
        self.skip = skip;
        self
    }

    /// Surfaces only the findings `admits` accepts.
    const fn admits(mut self, admits: fn(&rfc_rules::Finding) -> bool) -> Live {
        self.admits = admits;
        self
    }
}

impl CrossMessageAuditRule for Live {
    fn name(&self) -> &'static str {
        self.id.token()
    }

    fn subject(&self) -> HashSet<UaRole> {
        if self.subject.is_empty() {
            crate::types::all_ua_roles()
        } else {
            self.subject.iter().copied().collect()
        }
    }

    fn force_advisory(&self) -> bool {
        self.advisory
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let body = self.id.rule().body;
        let admits = self.admits;
        let found = match self.vantage {
            Vantage::Emitter => {
                at_vantage(events, body, |f, b| f.emitter == *b && admits(f), self.detail)
            }
            Vantage::Taker => {
                at_vantage(events, body, |f, b| f.taker == *b && admits(f), self.detail)
            }
            Vantage::EitherEnd => at_vantage(
                events,
                body,
                |f, b| (f.emitter == *b || f.taker == *b) && admits(f),
                self.detail,
            ),
            Vantage::TakerElseEmitter => {
                let recorded: HashSet<LaneKey> =
                    bind_views(events).into_iter().map(|v| v.bind).collect();
                at_vantage(
                    events,
                    body,
                    |f, b| {
                        let at = if recorded.contains(&f.taker) { &f.taker } else { &f.emitter };
                        at == b && admits(f)
                    },
                    self.detail,
                )
            }
        };
        let skipped = self.skip.lanes(events);
        found.into_iter().filter(|(lane, _, _, _)| !skipped.contains(lane)).collect()
    }
}

/// The detail line of a rule with no row of its own.
fn generic_detail(f: &rfc_rules::Finding, cid: &str) -> String {
    format!("Breaks {} (callId {cid}, CSeq {})", f.rule, f.cseq)
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

fn no_200_after_cancel(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn no_ack_to_dialog_creating_2xx(f: &rfc_rules::Finding, cid: &str) -> String {
    format!(
        "Took a 2xx confirming the dialog (callId {cid}, To-tag {tt}) to its own \
         INVITE (CSeq {cseq}) and never ACKed it; RFC 3261 §13.2.2.4 has the UAC \
         core ACK every 2xx received",
        tt = evidence_to_tag(f),
        cseq = f.cseq,
    )
}

fn unacked_2xx_not_cleared(f: &rfc_rules::Finding, cid: &str) -> String {
    format!(
        "Sent a 2xx to an INVITE (callId {cid}, To-tag {tt}) that was never ACKed \
         and the dialog was never BYE'd — the answered call leaks; RFC 3261 \
         §13.3.1.4 requires retransmitting the 2xx and, on no ACK, BYE-ing the \
         dialog",
        tt = evidence_to_tag(f),
    )
}

fn unacked_reliable_provisional(f: &rfc_rules::Finding, cid: &str) -> String {
    let (rseq, status) = match &f.decision {
        Decision::Violated(rfc_rules::Evidence::Unacked { rseq, status, .. }) => (*rseq, *status),
        _ => (0, 0),
    };
    format!(
        "Received reliable 1xx (status {status}, RSeq={rseq}, callId {cid}, CSeq \
         {cseq}) — UAC did not send a matching PRACK (RFC 3262 §4 / RFC3262-MUST-021)",
        cseq = f.cseq,
    )
}

fn rack_without_known_invite(f: &rfc_rules::Finding, cid: &str) -> String {
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
    let seen = known_cseqs.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
    format!(
        "Sent \"RAck: {rack_rseq} {rack_cseq} INVITE\" (callId {cid}) — its CSeq-num \
         field ({rack_cseq}) names no INVITE this bind opened (opened INVITE CSeq: \
         {seen}){expected} — RFC 3262 §7.2 (the RAck CSeq-num+method are copied from \
         the acknowledged 1xx's CSeq, NOT from the PRACK's own CSeq; this PRACK \
         matches no reliable provisional and settles nothing, so the 1xx keeps \
         retransmitting)"
    )
}

fn no_overlapping_reliable_provisionals(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::Overlapping { rseq, status, unacked_rseq, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent second reliable 1xx (status {status}, RSeq={rseq}, callId {cid}) before \
         prior RSeq {unacked_rseq} PRACKed — RFC 3262 §3 / RFC3262-MUST-012"
    )
}

fn non_contiguous_rseq(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RseqGap { rseq, prior_rseq, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent reliable 1xx RSeq={rseq} not contiguous with prior RSeq={prior_rseq} \
         (callId {cid}) — RFC 3262 §3 / RFC3262-MUST-013"
    )
}

fn no_prack_of_out_of_order_rseq(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::OutOfOrderRack {
        rack_rseq, expected_rseq, ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent PRACK for out-of-order RSeq={rack_rseq} (expected {expected_rseq}, callId \
         {cid}) — UAC must PRACK in order (RFC 3262 §4 / RFC3262-MUST-024)"
    )
}

fn single_final_per_server_txn(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn cancel_route_echoes_invite(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn cancel_after_1xx(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::EagerCancel { branch, .. }) = &f.decision else {
        return String::new();
    };
    format!(
        "Sent CANCEL (callId {cid}, branch {branch}) before any received 1xx for the \
         INVITE — {{Uac}} RFC 3261 §9.1 / RFC3261-MUST-048"
    )
}

fn no_cancel_after_final(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::LateCancel {
        final_status, since_final_us, ..
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
}

/// The `Call-ID=… from-tag=… to-tag=…` descriptor the CSeq findings name the
/// dialog by.
fn dialog_desc(call_id: &str, from_tag: &str, to_tag: &str) -> String {
    format!(
        "Call-ID={call_id} from-tag={from_tag} to-tag={}",
        if to_tag.is_empty() { "<none>" } else { to_tag },
    )
}

fn cseq_in_dialog_order(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn response_cseq_matches_transaction(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn ack_cseq_matches_invite(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn mid_dialog_uri(f: &rfc_rules::Finding, _cid: &str) -> String {
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
}

fn mid_dialog_route(f: &rfc_rules::Finding, _cid: &str) -> String {
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
}

fn mid_dialog_wire_destination(f: &rfc_rules::Finding, _cid: &str) -> String {
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
    let lead =
        if *from_route { "first route URI resolves to " } else { "Request-URI resolves to " };
    format!(
        "in-dialog {method} wire-sent to {sent_to} but {lead}{target_host}:{target_port} \
         (\"{target_uri}\") — RFC 3261 §8.1.2 + RFC 3263 §4"
    )
}

fn record_route_placement(f: &rfc_rules::Finding, _cid: &str) -> String {
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
}

fn rport_echo(f: &rfc_rules::Finding, _cid: &str) -> String {
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
}

fn allow_supported_on_invite(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::CapabilitiesNotAdvertised {
        missing, status, ..
    }) = &f.decision
    else {
        return String::new();
    };
    let label = if *status == 0 { "re-INVITE".to_string() } else { format!("{status} OK INVITE") };
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
}

fn proxy_100_trying_not_forwarded(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn unknown_dialog_481(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn unsupported_method_405_allow(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn unsupported_extension_420(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn unsupported_415_accepts(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn unsupported_extension_421(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ResponseHeadersMissing { branch, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent 421 response (callId {cid}, branch {branch}) lacks Require header listing \
         required extensions — {{Uas}} RFC 3261 §21.4.15 / RFC3261-MUST-182"
    )
}

fn no_target_404(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::NoTargetFinal { method, branch, status, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "{{Proxy}} received request {method} (callId {cid}, branch {branch}) and emitted \
         final response {status} without forwarding — expected 404 (RFC 3261 §16.3 / \
         RFC3261-MUST-105)"
    )
}

fn options_response_echoes(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ResponseHeadersMissing { branch, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent 2xx OPTIONS response (callId {cid}, branch {branch}) lacks \
         Allow/Supported/Accept headers — {{Uas}} RFC 3261 §11.2 / RFC3261-MUST-059"
    )
}

fn ack_require_subset_of_invite(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn ack_preserves_invite_route(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn strict_route_rewrite_handled(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::StrictRouteNotRewritten {
        branch,
        first_route,
        forwarded_request_uri,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let forwarded = if forwarded_request_uri.is_empty() { "<none>" } else { forwarded_request_uri };
    format!(
        "{{Proxy}} received strict-route request (callId {cid}, branch {branch}; first \
         Route={first_route}) but outgoing Request-URI={forwarded} — RFC 3261 §16.4 / \
         RFC3261-MUST-100"
    )
}

fn serial_register(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn register_no_route_set(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RegisterCarriesRoute { .. }) = &f.decision else {
        return String::new();
    };
    format!(
        "Sent REGISTER (callId {cid}) carries Route header — {{Uac}} RFC 3261 §10.2 / \
         RFC3261-MUST-051"
    )
}

fn concurrent_re_invite_500_or_491(f: &rfc_rules::Finding, cid: &str) -> String {
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
         respond 500+Retry-After when the same sender sent both, 491 when they cross; \
         got {answered_status}/RetryAfter={retry_after} (RFC 3261 §14.2)"
    )
}

fn no_bye_outside_or_early_dialog(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ByeOffDialog { early_dialog, .. }) = &f.decision
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
}

fn no_re_invite_while_invite_in_progress(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn proxy_100_within_grace(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn unacked_invite_non_2xx_final(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::UnackedReject { status, branch, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Took a non-2xx final {status} to an INVITE (callId {cid}, branch {branch}) and \
         never ACKed it to the UAS that sent it — the INVITE transaction never \
         completes and the reject retransmits to Timer H; RFC 3261 §17.1.1.3 makes \
         the ACK mandatory (hop-by-hop, so a proxy in the path owes the UAS its own \
         synthesized ACK)"
    )
}

fn failed_reinvite_tears_down_dialog(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn no_1xx_after_final(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn require_reliable_1xx_on_require(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::Unreliable1xx { status, branch, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "INVITE required 100rel (callId {cid}, branch {branch}) but sent 1xx response \
         {status} lacks Require:100rel/RSeq and no 420 Unsupported:100rel was sent — RFC \
         3262 §3 / RFC3262-MUST-001/-002"
    )
}

fn reliable_needs_client_opt_in(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::UnsolicitedReliable1xx { status, branch, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent reliable 1xx (status {status}, callId {cid}, branch {branch}) — matching \
         INVITE neither Supported:100rel nor Require:100rel (RFC 3262 §3 / \
         RFC3262-MUST-004)"
    )
}

fn no_reliable_1xx_on_in_dialog(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::InDialogReliable1xx {
        status, method, branch, ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent reliable 1xx on a non-INVITE request (status {status}, callId {cid}, \
         branch {branch}, method {method}) — forbidden per RFC 3262 §3 / \
         RFC3262-MUST-005"
    )
}

fn unmatched_prack_proxied(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn prack_2xx_or_481(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn delay_2xx_on_unacked_reliable_1xx_with_sdp(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AnsweredOverUnackedOffer {
        unacked_rseqs,
        branch,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let rseqs = unacked_rseqs.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
    format!(
        "Sent 2xx INVITE response while reliable 1xx (RSeq={rseqs}) with SDP still \
             unacked (callId {cid}, branch {branch}) — RFC 3262 §3 / RFC3262-MUST-014"
    )
}

fn no_new_reliable_1xx_after_final(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::Reliable1xxAfterFinal { rseq, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent new reliable 1xx (RSeq={rseq}, callId {cid}) after final INVITE response — \
         forbidden per RFC 3262 §3 / RFC3262-MUST-016"
    )
}

fn no_prack_of_100_trying(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::PrackedTrying { rseq, .. }) = &f.decision else {
        return String::new();
    };
    format!(
        "Sent PRACK references RSeq {rseq} from a received 100 Trying carrying \
         Require:100rel (callId {cid}) — UAC MUST ignore 100rel on 100 Trying (RFC 3262 \
         §4 / RFC3262-MUST-019)"
    )
}

fn prack_answers_1xx_offer(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::PrackWithoutAnswer { rack_rseq, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "PRACK for reliable-1xx-with-offer (RSeq={rack_rseq}, callId {cid}) carries no \
         body — RFC 3262 §5 / RFC3262-MUST-025"
    )
}

fn ack_body_after_complete_offer_answer(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AckBodyOnClosedRound { streams, .. }) = &f.decision
    else {
        return String::new();
    };
    let table = if streams.is_empty() { "no m= line".to_string() } else { streams.join(", ") };
    format!(
        "Sent an ACK (callId {cid}, CSeq {cseq}) carrying a session description \
         ({table}) although the offer/answer exchange it acknowledges was already \
         complete — expected no body on this ACK (Content-Length: 0), RFC 3261 \
         §13.2.2.4 / §13.2.1",
        cseq = f.cseq,
    )
}

fn final_2xx_answers_the_offer(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn delayed_offer_answered_in_ack(f: &rfc_rules::Finding, cid: &str) -> String {
    if !f.violated() {
        return String::new();
    }
    format!(
        "Sent an ACK (callId {cid}, CSeq {cseq}) carrying no session description \
         although the 2xx it acknowledges carried the offer (the INVITE had none) — \
         the ACK MUST carry the answer, RFC 3261 §13.2.2.4 / §13.2.1",
        cseq = f.cseq,
    )
}

fn second_answer_repeats_the_first(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn answer_stream_matches_offer(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn sdp_origin_continuity(f: &rfc_rules::Finding, cid: &str) -> String {
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
             {prior_session_version} → {session_version} (expected it unchanged or +1)"
        )
    };
    format!(
        "Sent \"{origin_line}\" after \"{prior_origin_line}\" on the same call (callId \
         {cid}, CSeq {cseq}) — {offence} — RFC 4566 §5.2 / RFC 3264 §8",
        cseq = f.cseq,
    )
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

fn no_new_offer_while_offer_pending(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::OfferWhilePending { pending_offer_cseq, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent an SDP offer (callId {cid}, CSeq {cseq}) while its own offer on CSeq \
         {pending_offer_cseq} was still unanswered — an offer is answered before the \
         next one goes out (RFC 3264 §5 / RFC3264-MUST-002)",
        cseq = f.cseq,
    )
}

fn answer_m_line_count_matches_offer(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn answer_t_line_equals_offer(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn answer_media_type_matches_offer(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn direction_pair_valid(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::DirectionPairInvalid {
        stream_indexes,
        offered_directions,
        answered_directions,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let rows = stream_rows(stream_indexes, offered_directions, answered_directions, |i, o, a| {
        format!("m=[{i}] answer '{a}' invalid for offer '{o}'")
    });
    format!(
        "Answer direction invalid for the offer's at {n} stream position(s) — {rows} \
         (callId {cid}, CSeq {cseq}) — RFC 3264 §6.1 / RFC3264-MUST-023",
        n = stream_indexes.len(),
        cseq = f.cseq,
    )
}

fn rejected_stream_minimal_answer(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn re_offer_m_line_count_monotonic(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn zero_port_propagation(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn payload_type_mapping_stable(f: &rfc_rules::Finding, cid: &str) -> String {
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
        .map(|(pt, (prev, now))| format!("payload-type {pt} was '{prev}' now '{now}'"))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Remapped {n} payload-type binding(s) it had already stated on this call — \
         {rows} (callId {cid}, CSeq {cseq}); the peer decodes by the cached mapping — \
         RFC 3264 §8.3.2 / RFC3264-MUST-047",
        n = payload_types.len(),
        cseq = f.cseq,
    )
}

fn branch_prefix(f: &rfc_rules::Finding, _cid: &str) -> String {
    match &f.decision {
        Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { on, value, .. }) => {
            format!(
                        "{on} top Via branch \"{value}\" does not begin with the RFC 3261 magic \
                         cookie \"z9hG4bK\" (§8.1.1.7) — a downstream element cannot treat it as an \
                         RFC-3261 transaction id"
                    )
        }
        Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) => {
            format!(
                "{on} has no top Via branch parameter (RFC 3261 §8.1.1.7 requires a \
                             \"z9hG4bK\"-prefixed branch on every request)"
            )
        }
        _ => String::new(),
    }
}

fn max_forwards(f: &rfc_rules::Finding, _cid: &str) -> String {
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
        Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { on, value, .. }) => {
            format!(
                "{on} has an invalid Max-Forwards value \"{value}\" — RFC 3261 §8.1.1.6 \
                         requires an integer in 0..=255"
            )
        }
        _ => String::new(),
    }
}

fn content_length(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { value, expected, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Content-Length mismatch: header says {value} but body is {expected} bytes — RFC \
         3261 §20.14 (a strict peer truncates or desyncs on this)"
    )
}

fn content_type(f: &rfc_rules::Finding, _cid: &str) -> String {
    if !f.violated() {
        return String::new();
    }
    "message carries a body but no Content-Type — RFC 3261 §7.4.1 requires Content-Type \
     whenever a body is present (the peer cannot interpret the body)"
        .to_string()
}

fn contact_presence(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} request is missing Contact — RFC 3261 §8.1.1.8 requires it on \
         dialog-establishing methods (the dialog's remote target is undefined)"
    )
}

fn no_contact_on_bye(f: &rfc_rules::Finding, _cid: &str) -> String {
    if !f.violated() {
        return String::new();
    }
    "BYE carries Contact — RFC 3261 §15.1 (BYE terminates the dialog, so target-refresh \
     has no meaning)"
        .to_string()
}

fn to_tag_presence(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} response is missing a To-tag — RFC 3261 §8.2.6.2 requires a UAS to add a \
         To-tag to every response above 100 (the peer cannot dialog-match it)"
    )
}

fn from_tag_presence(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} request is missing a From-tag — RFC 3261 §8.1.1.3 requires a UAC to add a \
         From-tag to every request (the peer cannot dialog-match it)"
    )
}

fn no_record_route_from_ua(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent { value, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "B2BUA inserted Record-Route in a request — a B2BUA is a UA and MUST NOT use \
         Record-Route (RFC 3261 §16.6). Found: {value}"
    )
}

fn response_echoes_request_via(f: &rfc_rules::Finding, _cid: &str) -> String {
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
}

fn response_correlation(f: &rfc_rules::Finding, _cid: &str) -> String {
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
}

fn mid_dialog_tags(f: &rfc_rules::Finding, _cid: &str) -> String {
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
}

fn peer_uri_stable(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::PeerUriRewritten { sent_uri, dialog_uri, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "in-dialog From URI \"{sent_uri}\" differs from the dialog-established remote URI \
         \"{dialog_uri}\" — RFC 3261 §12.2.1.1"
    )
}

fn dialog_call_id_stable(f: &rfc_rules::Finding, _cid: &str) -> String {
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
}

fn cancel_request_uri(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::CancelUriDiverged {
        cancel_uri, invite_uri, ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "CANCEL Request-URI \"{cancel_uri}\" differs from the INVITE Request-URI \
         \"{invite_uri}\" — RFC 3261 §9.1 (the CANCEL cannot match the INVITE server \
         transaction)"
    )
}

fn cancel_via_branch(f: &rfc_rules::Finding, _cid: &str) -> String {
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
}

/// The status a violated `tag-consistency` finding flipped the tag on; `None`
/// for every other decision.
fn tag_flip_status(f: &rfc_rules::Finding) -> Option<u16> {
    match &f.decision {
        Decision::Violated(rfc_rules::Evidence::UasTagFlipped { status, .. }) => Some(*status),
        _ => None,
    }
}

/// The detail line both `tag-consistency` arms report, off the flip evidence.
fn tag_flip_detail(f: &rfc_rules::Finding, _cid: &str) -> String {
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
        "UAS To-tag mismatch on {status} (branch {branch}): prior provisional(s) established \
         tag(s) [{seen}] but the final carries \"{final_tag}\" — RFC 3261 §17.2.1 / §12.1.1",
        seen = provisional_tags.join(", "),
    )
}

/// A flip on a 2xx: a forking B2BUA may answer 2xx off a later early dialog
/// (§12.1.2, §13.2.2.4), which per branch reads as a flip, so this arm advises.
fn tag_flip_2xx(f: &rfc_rules::Finding) -> bool {
    tag_flip_status(f).is_some_and(|status| (200..300).contains(&status))
}

/// A flip on a non-2xx final: it establishes no dialog (§12.1.1), so the
/// caller's early dialog is never terminated, and this arm gates.
fn tag_flip_non_2xx(f: &rfc_rules::Finding) -> bool {
    tag_flip_status(f).is_some_and(|status| !(200..300).contains(&status))
}

fn no_100rel_require_on_non_invite(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent { on, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} request carries Require: 100rel — RFC 3262 §4 admits it on INVITE only (a \
         strict peer answers 420 Bad Extension)"
    )
}

fn reliable_1xx_headers(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::Reliable1xx { status, carried, .. }) = &f.decision
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
        Some(rseq) => {
            format!("Reliable {status} response {rseq} is outside [1, 2^31-1] — RFC 3262 §3")
        }
    }
}

fn no_to_tag_on_initial_request(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent { on, value, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} request outside any dialog carries To-tag={value} — RFC 3261 §8.1.1.2 (it \
         names a dialog that does not exist)"
    )
}

fn in_dialog_to_tag(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} request inside a confirmed dialog carries no To-tag — RFC 3261 §12.2.1.1 \
         requires the dialog's remote tag (the peer cannot dialog-match it)"
    )
}

fn in_dialog_from_tag(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::DialogRemoteTagForeign {
        method,
        from_tag,
        dialog_remote_tags,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{method} (callId {cid}, CSeq {cseq}) taken inside a confirmed dialog carries \
         From-tag \"{from_tag}\", but the dialogs confirmed under its To-tag have \
         remote tags [{remotes}] — RFC 3261 §12.2.1.1 / §12.1.2 (the request names no \
         dialog its taker holds)",
        cseq = f.cseq,
        remotes = dialog_remote_tags.join(" | "),
    )
}

fn no_require_on_cancel_or_ack(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent {
        on, header, value, ..
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
}

fn cancel_cseq_method(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { value, .. }) = &f.decision
    else {
        return String::new();
    };
    format!("CANCEL request carries CSeq method={value} (expected CANCEL) — RFC 3261 §9.1")
}

fn strict_route_shuffle_on_send(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { on, value, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent {on} request still carries strict-route topmost Route entry \"{value}\" — \
         RFC 3261 §16.6 step 6 swap may not have run"
    )
}

fn sdp_body_parseable(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::SdpBodyRejected { sdp_failure, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent SDP body fails the RFC 3264 / RFC 4566 grammar check: {sdp_failure} (callId \
         {cid})"
    )
}

fn c0_port_non_zero(f: &rfc_rules::Finding, cid: &str) -> String {
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
}

fn rung_byte_identical(f: &rfc_rules::Finding, cid: &str) -> String {
    use rfc_rules::rules::retransmit::Class;
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
}

/// The live surfacing of every rule that has one; [`cross_rules`] gives the
/// others the default row.
const LIVE: &[Live] = &[
    // Live stamps are strictly monotonic, so a 2xx sent before the CANCEL arrived is a race the
    // rule clears.
    Live::emitter(RuleId::No200AfterCancel, no_200_after_cancel),
    Live::emitter(RuleId::NoAckToDialogCreating2xx, no_ack_to_dialog_creating_2xx),
    Live::emitter(RuleId::Unacked2xxNotCleared, unacked_2xx_not_cleared),
    Live::emitter(RuleId::UnackedReliableProvisional, unacked_reliable_provisional),
    Live::emitter(RuleId::RackWithoutKnownInvite, rack_without_known_invite),
    Live::emitter(RuleId::NoOverlappingReliableProvisionals, no_overlapping_reliable_provisionals),
    Live::emitter(RuleId::NonContiguousRseq, non_contiguous_rseq),
    Live::emitter(RuleId::NoPrackOfOutOfOrderRseq, no_prack_of_out_of_order_rseq),
    Live::emitter(RuleId::SingleFinalPerServerTxn, single_final_per_server_txn)
        .roles(&[UaRole::Uas])
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::CancelRouteEchoesInvite, cancel_route_echoes_invite)
        .skip(Skip::RelayLanes),
    // Advisory (ADR-0028). A `{Proxy}`-only lane forwards its upstream's CANCEL at the upstream's
    // timing; a B2BUA authors its own and is judged.
    Live::emitter(RuleId::CancelAfter1xx, cancel_after_1xx).advisory().skip(Skip::ProxyOnlyLanes),
    // No relay skip: a lane forwarding an upstream CANCEL never ACKed the final itself.
    Live::emitter(RuleId::NoCancelAfterFinal, no_cancel_after_final),
    Live::taker(RuleId::CseqInDialogOrder, cseq_in_dialog_order),
    Live::taker(RuleId::ResponseCseqMatchesTransaction, response_cseq_matches_transaction),
    Live::taker(RuleId::AckCseqMatchesInvite, ack_cseq_matches_invite),
    Live::emitter(RuleId::MidDialogUri, mid_dialog_uri).skip(Skip::RelayLanes),
    Live::emitter(RuleId::MidDialogRoute, mid_dialog_route).skip(Skip::RelayLanes),
    Live::emitter(RuleId::MidDialogWireDestination, mid_dialog_wire_destination)
        .skip(Skip::RelayLanes),
    Live::taker(RuleId::RecordRoutePlacement, record_route_placement).skip(Skip::RelayLanes),
    Live::taker(RuleId::RportEcho, rport_echo).advisory().skip(Skip::RelayLanes),
    // Advisory: both clauses are SHOULDs.
    Live::taker(RuleId::AllowSupportedOnInvite, allow_supported_on_invite)
        .advisory()
        .skip(Skip::RelayLanes),
    Live::taker(RuleId::Proxy100TryingNotForwarded, proxy_100_trying_not_forwarded)
        .skip(Skip::RelayLanes),
    // No relay skip: a relay face knows both its peers, so the rule's peer-tag key never charges
    // it.
    Live::emitter(RuleId::UnknownDialog481, unknown_dialog_481),
    Live::emitter(RuleId::UnsupportedMethod405Allow, unsupported_method_405_allow),
    Live::emitter(RuleId::UnsupportedExtension420, unsupported_extension_420),
    Live::emitter(RuleId::Unsupported415Accepts, unsupported_415_accepts),
    Live::emitter(RuleId::UnsupportedExtension421, unsupported_extension_421),
    // §16.3 is proxy behaviour: a UAS rejecting a call it took is no resolution failure.
    Live::emitter(RuleId::NoTarget404, no_target_404).roles(&[UaRole::Proxy]).advisory(),
    // Advisory: §11.2's header set is a SHOULD.
    Live::emitter(RuleId::OptionsResponseEchoes, options_response_echoes).advisory(),
    Live::emitter(RuleId::AckRequireSubsetOfInvite, ack_require_subset_of_invite)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::AckPreservesInviteRoute, ack_preserves_invite_route)
        .skip(Skip::RelayLanes),
    // §16.4 is proxy behaviour.
    Live::emitter(RuleId::StrictRouteRewriteHandled, strict_route_rewrite_handled)
        .roles(&[UaRole::Proxy]),
    Live::emitter(RuleId::SerialRegister, serial_register),
    Live::emitter(RuleId::RegisterNoRouteSet, register_no_route_set),
    // No relay skip: a face passing two racing re-INVITEs on is the UAS of two server transactions
    // it never serialised.
    Live::emitter(RuleId::ConcurrentReInvite500Or491, concurrent_re_invite_500_or_491),
    // No relay skip: the peer-tag dialog key spares a face that saw the dialog created.
    Live::emitter(RuleId::NoByeOutsideOrEarlyDialog, no_bye_outside_or_early_dialog),
    // No relay skip: the ordered dialog key spares a lane relaying both parties' crossing re-
    // INVITEs (glare).
    Live::emitter(RuleId::NoReInviteWhileInviteInProgress, no_re_invite_while_invite_in_progress),
    // §16.7 binds proxies; a UAS answering an INVITE is under §8.2.6.
    Live::emitter(RuleId::Proxy100WithinGrace, proxy_100_within_grace)
        .roles(&[UaRole::Proxy])
        .advisory(),
    // Gates: the harness UAC ACKs every non-2xx final (§17.1.1.3); a test modelling a peer that
    // never ACKs waives it.
    Live::taker_else_emitter(RuleId::UnackedInviteNon2xxFinal, unacked_invite_non_2xx_final),
    // No relay skip: a relaying lane opens its own client transaction on its own branch.
    Live::emitter(RuleId::FailedReinviteTearsDownDialog, failed_reinvite_tears_down_dialog),
    Live::emitter(RuleId::No1xxAfterFinal, no_1xx_after_final)
        .roles(&[UaRole::Uas])
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::RequireReliable1xxOnRequire, require_reliable_1xx_on_require)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::ReliableNeedsClientOptIn, reliable_needs_client_opt_in)
        .roles(&[UaRole::Uas])
        .advisory()
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::NoReliable1xxOnInDialog, no_reliable_1xx_on_in_dialog)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::UnmatchedPrackProxied, unmatched_prack_proxied)
        .roles(&[UaRole::Proxy])
        .advisory()
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::Prack2xxOr481, prack_2xx_or_481).skip(Skip::RelayLanes),
    Live::emitter(
        RuleId::Delay2xxOnUnackedReliable1xxWithSdp,
        delay_2xx_on_unacked_reliable_1xx_with_sdp,
    )
    .skip(Skip::RelayLanes),
    Live::emitter(RuleId::NoNewReliable1xxAfterFinal, no_new_reliable_1xx_after_final)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::NoPrackOf100Trying, no_prack_of_100_trying).skip(Skip::RelayLanes),
    // Gates: each offer is read within its own call and early dialog, so a B2BUA's two legs never
    // meet in it.
    Live::either_end(RuleId::PrackAnswers1xxOffer, prack_answers_1xx_offer)
        .roles(&[UaRole::Uac, UaRole::Uas])
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::AckBodyAfterCompleteOfferAnswer, ack_body_after_complete_offer_answer)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::Final2xxAnswersTheOffer, final_2xx_answers_the_offer)
        .skip(Skip::RelayLanes),
    Live::taker_else_emitter(RuleId::DelayedOfferAnsweredInAck, delayed_offer_answered_in_ack)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::SecondAnswerRepeatsTheFirst, second_answer_repeats_the_first)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::AnswerStreamMatchesOffer, answer_stream_matches_offer)
        .skip(Skip::RelayLanes),
    // Origin continuity is per originator: a proxy's stream interleaves both agents' origins.
    Live::emitter(RuleId::SdpOriginContinuity, sdp_origin_continuity)
        .roles(&[UaRole::Uac, UaRole::Uas])
        .advisory()
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::NoNewOfferWhileOfferPending, no_new_offer_while_offer_pending)
        .advisory()
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::AnswerMLineCountMatchesOffer, answer_m_line_count_matches_offer)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::AnswerTLineEqualsOffer, answer_t_line_equals_offer)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::AnswerMediaTypeMatchesOffer, answer_media_type_matches_offer)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::DirectionPairValid, direction_pair_valid)
        .advisory()
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::RejectedStreamMinimalAnswer, rejected_stream_minimal_answer)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::ReOfferMLineCountMonotonic, re_offer_m_line_count_monotonic)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::ZeroPortPropagation, zero_port_propagation)
        .advisory()
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::PayloadTypeMappingStable, payload_type_mapping_stable)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::BranchPrefix, branch_prefix),
    Live::emitter(RuleId::MaxForwards, max_forwards),
    Live::emitter(RuleId::ContentLength, content_length),
    Live::emitter(RuleId::ContentType, content_type),
    Live::emitter(RuleId::ContactPresence, contact_presence),
    Live::emitter(RuleId::NoContactOnBye, no_contact_on_bye),
    Live::emitter(RuleId::ToTagPresence, to_tag_presence),
    Live::emitter(RuleId::FromTagPresence, from_tag_presence),
    Live::emitter(RuleId::NoRecordRouteFromUa, no_record_route_from_ua).roles(&[UaRole::Proxy]),
    Live::taker(RuleId::ResponseEchoesRequestVia, response_echoes_request_via),
    Live::taker(RuleId::ResponseCorrelation, response_correlation),
    Live::taker(RuleId::MidDialogTags, mid_dialog_tags),
    Live::taker(RuleId::PeerUriStable, peer_uri_stable),
    Live::taker(RuleId::DialogCallIdStable, dialog_call_id_stable),
    Live::taker(RuleId::CancelRequestUri, cancel_request_uri),
    Live::taker(RuleId::CancelViaBranch, cancel_via_branch),
    Live::emitter(RuleId::TagConsistency, tag_flip_detail).advisory().admits(tag_flip_2xx),
    Live::emitter(RuleId::TagConsistency, tag_flip_detail).admits(tag_flip_non_2xx),
    Live::emitter(RuleId::No100relRequireOnNonInvite, no_100rel_require_on_non_invite),
    // The reliable-provisional rows are the answering side's to write.
    Live::emitter(RuleId::Reliable1xxHeaders, reliable_1xx_headers).roles(&[UaRole::Uas]),
    Live::emitter(RuleId::NoToTagOnInitialRequest, no_to_tag_on_initial_request),
    // A declared proxy carries the originator's To through unchanged.
    Live::emitter(RuleId::InDialogToTag, in_dialog_to_tag).roles(&[UaRole::Uac, UaRole::Uas]),
    Live::taker_else_emitter(RuleId::InDialogFromTag, in_dialog_from_tag),
    Live::emitter(RuleId::NoRequireOnCancelOrAck, no_require_on_cancel_or_ack),
    Live::emitter(RuleId::CancelCseqMethod, cancel_cseq_method),
    // §16.6 step 6 is a forwarding element's step.
    Live::emitter(RuleId::StrictRouteShuffleOnSend, strict_route_shuffle_on_send)
        .roles(&[UaRole::Proxy]),
    Live::emitter(RuleId::SdpBodyParseable, sdp_body_parseable),
    Live::emitter(RuleId::C0PortNonZero, c0_port_non_zero).roles(&[UaRole::Uac]),
    // Gates on every role, no relay skip: a lane's rung is its own re-send whatever it forwarded
    // first.
    Live::emitter(RuleId::RungByteIdentical, rung_byte_identical),
];

/// The live rows of `id`, or the default row when it has none.
fn rows_of(id: RuleId) -> Vec<Live> {
    let rows: Vec<Live> = LIVE.iter().filter(|l| l.id == id).copied().collect();
    if rows.is_empty() {
        vec![Live::emitter(id, generic_detail)]
    } else {
        rows
    }
}

/// The merged-rule cross checks this adapter contributes to the default suite:
/// the live rows of every rule of [`RuleId::ALL`].
pub fn cross_rules() -> Vec<std::sync::Arc<dyn CrossMessageAuditRule>> {
    RuleId::ALL
        .iter()
        .flat_map(|&id| rows_of(id))
        .map(|live| std::sync::Arc::new(live) as std::sync::Arc<dyn CrossMessageAuditRule>)
        .collect()
}

#[cfg(test)]
mod tests;
