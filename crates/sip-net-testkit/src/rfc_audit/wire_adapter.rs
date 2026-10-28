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
//! detail reads `<charged party>: <what it did>`, the same on whichever lane
//! reports it.
//! Every rule has a row: `every_merged_rule_has_a_live_adapter` names one
//! without.

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
use crate::rfc_audit::recorded::Recorded;
use crate::rfc_audit::relay_lanes::relay_lanes;
use crate::types::UaRole;

mod ack;
mod cancel;
mod capability;
mod correlation;
mod cseq;
mod dialog;
mod final_response;
mod offer_answer;
mod prack;
mod proxy;
mod register;
mod reinvite;
mod retransmit;
mod scoped;
mod via;
mod wellformed;

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
                    packet.raw.to_vec(),
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
                format!("{}: {}", f.emitter, detail(&f, cid)),
                positions.get(seq).copied(),
                Some(f.emitter.clone()),
            ));
        }
    }
    out
}

/// Where the live audit reports a rule's finding. The charged party is always
/// the finding's `emitter`; the vantage is the lane that REPORTS it. Subject
/// dispatch judges the charged party's declared roles, whatever the vantage.
#[derive(Clone, Copy)]
enum Vantage {
    /// The emitter's own lane: what a sender mints or owes. An emitter that is
    /// no recorded bind, in the view or scoped out of it, is reported once, at
    /// the taker, where the recording holds what it sent; it declared no roles,
    /// so every row scoped to a user-agent role judges it.
    /// A row scoped to proxies alone judges how a proxy handles what it took,
    /// which no taker shows of a party it cannot tell is one: it never falls back.
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
    /// Once, at the taker where its lane is in the view and at the emitter
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
/// the detail line a finding reads as.
#[derive(Clone, Copy)]
struct Live {
    id: RuleId,
    vantage: Vantage,
    skip: Skip,
    subject: &'static [UaRole],
    advisory: bool,
    /// An advisory row that gates a finding charged to a system under test.
    gates_sut: bool,
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
            gates_sut: false,
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

    /// Advisory for the simulated parties, gating for a system under test:
    /// a harness that knows which recorded parties are under test fails the
    /// run on a finding charged to one ([`rfc_rule_gates_sut`]).
    const fn gating_the_sut(mut self) -> Live {
        self.gates_sut = true;
        self
    }

    const fn skip(mut self, skip: Skip) -> Live {
        self.skip = skip;
        self
    }

    /// Whether the row is scoped to proxies and to no user-agent role.
    fn proxy_only(&self) -> bool {
        !self.subject.is_empty()
            && !self.subject.iter().any(|r| matches!(r, UaRole::Uac | UaRole::Uas))
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
        self.found(events, &HashSet::new())
    }
}

impl Live {
    /// The row's findings over `events`, the binds of `scoped_out` taken out of it.
    fn found(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
        scoped_out: &HashSet<LaneKey>,
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let body = self.id.rule().body;
        let admits = self.admits;
        let found = match self.vantage {
            Vantage::Emitter => {
                let recorded = Recorded::of(events, scoped_out);
                let falls_back = !self.proxy_only();
                at_vantage(
                    events,
                    body,
                    |f, b| {
                        let own = !falls_back || recorded.holds(&f.emitter);
                        let at = if own { &f.emitter } else { &f.taker };
                        at == b && admits(f)
                    },
                    self.detail,
                )
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
                // The taker's lane must be in the view to report there.
                let recorded = Recorded::of(events, &HashSet::new());
                at_vantage(
                    events,
                    body,
                    |f, b| {
                        let at = if recorded.holds(&f.taker) { &f.taker } else { &f.emitter };
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

/// Every family's live rows.
const FAMILIES: &[&[Live]] = &[
    ack::LIVE,
    cancel::LIVE,
    capability::LIVE,
    correlation::LIVE,
    cseq::LIVE,
    dialog::LIVE,
    final_response::LIVE,
    offer_answer::LIVE,
    prack::LIVE,
    proxy::LIVE,
    register::LIVE,
    reinvite::LIVE,
    retransmit::LIVE,
    via::LIVE,
    wellformed::LIVE,
];

/// The live rows of `id`.
fn rows_of(id: RuleId) -> Vec<Live> {
    FAMILIES.iter().flat_map(|f| f.iter()).filter(|l| l.id == id).copied().collect()
}

/// Whether the advisory rule `token` gates a finding charged to a system under
/// test: a fake stack may omit what it asks, the stack under test may not.
pub fn rfc_rule_gates_sut(token: &str) -> bool {
    RuleId::ALL
        .iter()
        .filter(|id| id.token() == token)
        .flat_map(|&id| rows_of(id))
        .any(|live| live.gates_sut)
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

pub use scoped::cross_rules_scoped;
#[cfg(test)]
use scoped::Scoped;

#[cfg(test)]
mod tests;
