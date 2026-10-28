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

use std::borrow::Cow;
use std::collections::HashSet;

use layer_harness::{LaneKey, Stamped};

use rfc_rules::rules::Obligation;
use rfc_rules::{Decision, RuleId, WireView};

use crate::contracts::{audit_visible_event, CrossMessageAuditRule, SignalingNetworkEvent};
use crate::report::wire_positions_by_stamp;
use crate::rfc_audit::dropped::Dropped;
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
mod view;
mod wellformed;

use view::{bind_views, observation};

/// Run `rule` over every bind view and keep the violated, non-relayed findings
/// the `vantage` predicate accepts for the bind the view belongs to. Every
/// non-relayed occasion at that vantage, violated or not, is noted for the
/// run's rule population.
fn at_vantage(
    events: &[Stamped<SignalingNetworkEvent>],
    rule: &dyn Obligation,
    vantage: impl Fn(&rfc_rules::Finding, &LaneKey, &str) -> bool,
    detail: impl Fn(&rfc_rules::Finding, &str) -> String,
) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
    let positions = wire_positions_by_stamp(events);
    let mut out = Vec::new();
    for view in bind_views(events) {
        let obs = observation(&view.msgs);
        for f in rule.eval(&WireView { msgs: &view.msgs, obs: &obs }) {
            let (seq, cid) = &view.meta[f.anchor];
            if f.relayed || !vantage(&f, &view.bind, cid) {
                continue;
            }
            crate::rfc_audit::occasions::note(&view.bind, &f);
            if !f.violated() {
                continue;
            }
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
    /// As [`Emitter`](Self::Emitter), for a verdict that rests on a message
    /// the emitter never sent: the taker's audit view holds none the loss model
    /// dropped, so an emitter that is no recorded bind is not judged at a taker
    /// that recorded such a drop from it on the call.
    EmitterUnlessLost,
    /// The emitter's own lane and no other: a verdict that orders what the
    /// emitter sent after what it took. A taker's lane times its own sends at
    /// sending and the emitter's at arrival, so it cannot order them: an
    /// emitter that is no recorded bind is not judged.
    EmitterOnly,
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

    const fn emitter_unless_lost(id: RuleId, detail: Detail) -> Live {
        Live::at(Vantage::EmitterUnlessLost, id, detail)
    }

    const fn emitter_only(id: RuleId, detail: Detail) -> Live {
        Live::at(Vantage::EmitterOnly, id, detail)
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
    /// The row's findings over `events`, the binds of `scoped_out` taken out of
    /// it. The rules judge the audit-visible events; the arrivals the loss model
    /// dropped only tell [`Vantage::EmitterUnlessLost`] where a sender's
    /// traffic is incomplete.
    fn found(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
        scoped_out: &HashSet<LaneKey>,
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        let visible: Cow<'_, [Stamped<SignalingNetworkEvent>]> = if events
            .iter()
            .all(|s| audit_visible_event(&s.event))
        {
            Cow::Borrowed(events)
        } else {
            Cow::Owned(events.iter().filter(|s| audit_visible_event(&s.event)).cloned().collect())
        };
        let all = events;
        let events = &*visible;
        let body = self.id.rule().body;
        let admits = self.admits;
        let found = match self.vantage {
            Vantage::Emitter => {
                let recorded = Recorded::of(events, scoped_out);
                let falls_back = !self.proxy_only();
                at_vantage(
                    events,
                    body,
                    |f, b, _| {
                        let own = !falls_back || recorded.holds(&f.emitter);
                        let at = if own { &f.emitter } else { &f.taker };
                        at == b && admits(f)
                    },
                    self.detail,
                )
            }
            Vantage::EmitterUnlessLost => {
                let recorded = Recorded::of(events, scoped_out);
                let dropped = Dropped::of(all);
                at_vantage(
                    events,
                    body,
                    |f, b, cid| {
                        if recorded.holds(&f.emitter) {
                            return f.emitter == *b && admits(f);
                        }
                        f.taker == *b && admits(f) && !dropped.holds(b, &f.emitter, cid)
                    },
                    self.detail,
                )
            }
            Vantage::EmitterOnly => {
                at_vantage(events, body, |f, b, _| f.emitter == *b && admits(f), self.detail)
            }
            Vantage::Taker => {
                at_vantage(events, body, |f, b, _| f.taker == *b && admits(f), self.detail)
            }
            Vantage::EitherEnd => at_vantage(
                events,
                body,
                |f, b, _| (f.emitter == *b || f.taker == *b) && admits(f),
                self.detail,
            ),
            Vantage::TakerElseEmitter => {
                // The taker's lane must be in the view to report there.
                let recorded = Recorded::of(events, &HashSet::new());
                at_vantage(
                    events,
                    body,
                    |f, b, _| {
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
