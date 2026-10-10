//! The flow half of an arrival: attribute it, learn from it, and match it
//! against the leg's armed expects.

use pivot_schema::bundle::{Failure, LadderSide};
use sip_message::{Method, SipMessage};
use tokio::time::Instant;

use super::arbitration::HELD_TAKEN;
use super::completion::outcome_of;
use super::dispatch::opens_dialog;
use super::unscripted::{OWED_BYE_FINAL, REJECT_ACK};
use super::Runner;
use crate::cancel;
use crate::gate::{self, GateVerdict, Inbound};
use crate::plan::{CompiledStep, Discriminator};
use crate::program::ItemKind;
use crate::progress;
use crate::resolve::Resolver;

impl<'a, 'p> Runner<'a, 'p> {
    /// The FLOW half of `dispatch`: attribute the datagram to a leg, learn what
    /// it teaches, and match it against that leg's armed expects.
    ///
    /// `held_seq` names an arrival already in the recording — a datagram
    /// released back into the flow after a hold-out — so a deferral never
    /// doubles a message in the bundle nor restates the instant it landed.
    pub(super) async fn deliver(
        &mut self,
        actor: &str,
        inbound: Inbound,
        message: SipMessage,
        bytes: Vec<u8>,
        repeat: bool,
        held_seq: Option<u64>,
    ) -> bool {
        let leg = match self.leg_of(actor, &inbound) {
            Ok(leg) => leg,
            Err(detail) => {
                self.record_arrival("unattributed", bytes, None, Some(&detail), repeat);
                // A datagram belonging to no leg of this flow stops nothing: it
                // is the failure it is, and every armed expect is as satisfiable
                // as it was (§11.2).
                self.instance.fail(Failure::UnexpectedDatagram {
                    leg: format!("(actor {actor})"),
                    arrived: inbound.arrived(),
                    detail: Some(detail),
                });
                return true;
            }
        };
        if opens_dialog(&inbound) {
            self.instance.leg_mut(&leg).mscml.remember(&inbound);
        }
        if self.owed_bye_final(&leg, &inbound) {
            self.record_arrival(&leg, bytes, None, Some(OWED_BYE_FINAL), repeat);
            return true;
        }
        if self.unscripted_reject_ack(&leg, &inbound) {
            self.record_arrival(&leg, bytes, None, Some(REJECT_ACK), repeat);
            return true;
        }
        let seq = match held_seq {
            Some(seq) => {
                self.instance.recording().renote(&leg, seq, HELD_TAKEN);
                Some(seq)
            }
            None => {
                self.record_arrival(&leg, bytes.clone(), None, None, repeat);
                self.instance.recording().last_seq(&leg)
            }
        };

        // The dialog's confirmation BEFORE this message taught the leg anything:
        // what a tail that no longer matches was scripted for (`progress`).
        let confirmed_before = self.stacks.get(&leg).is_some_and(|stack| stack.confirmed());
        // Learn the dialog facts before matching: a step's inline checks may
        // read what this very message taught the leg.
        if let Some(stack) = self.stacks.get_mut(&leg) {
            match &message {
                SipMessage::Request(r) => stack.learn_request(r),
                SipMessage::Response(r) => stack.learn_response(r),
            }
        }
        // Only an INVITE final is ACKed, so only one kind is kept: a 200 to a
        // CANCEL is a final too, and ACKing it would answer the wrong
        // transaction.
        if let SipMessage::Response(r) = &message {
            if r.status() >= 200 && *r.cseq().method() == Method::Invite {
                self.note_final(&leg, r);
            }
        }
        self.sync_leg_state(&leg);

        // The leg's live expects, budget open or not: what a datagram may match
        // is the frontier, and a budget that has not opened yet withholds
        // nothing (§6.8).
        let gated: Vec<CompiledStep> = self
            .instance
            .cursor()
            .frontier()
            .iter()
            .filter_map(|id| self.instance.plan().step(id))
            .filter(|s| s.is_expect() && s.leg == leg)
            .cloned()
            .collect();
        if gated.is_empty() {
            // Nothing on this leg was waiting for anything, so nothing about it
            // became unreachable: the failure stands and the flow goes on
            // (§11.2).
            self.instance.fail(Failure::UnexpectedDatagram {
                leg: leg.clone(),
                arrived: inbound.arrived(),
                detail: None,
            });
            self.answer_unscripted(&leg, &message).await;
            return true;
        }
        let scope = self.instance.scope();
        let resolver = Resolver::new(self.instance.state(), &self.instance.config().identities);
        // The whole assertion first, over every armed step: a datagram that
        // satisfies it is that step's beyond doubt, so a step whose headers hold
        // always wins one whose headers merely could. Only then the reading that
        // takes a datagram whose HEADER SET diverges (`gate::body_content_holds`)
        // — the step names each difference and fails, rather than refusing the
        // message and abandoning the call several steps past the cause.
        // A late CANCEL's other final answers the step as its own status would
        // (§6.7d): read off this leg's wire, beside the structural gate.
        let ladder = if inbound.status.is_some()
            && Method::from_wire(&inbound.cseq_method) == Method::Cancel
        {
            self.instance.recording().legs().remove(&leg).unwrap_or_default()
        } else {
            Vec::new()
        };
        let late_cancel = |step: &CompiledStep| {
            let opener = self.instance.cursor().opening_send(&step.id);
            cancel::answers_late_cancel(step, &inbound, &ladder, opener)
        };
        let gates = |step: &CompiledStep| {
            gate::discriminates(step, &inbound).matches() || late_cancel(step)
        };
        let matched = gated
            .iter()
            .find(|step| {
                gates(step)
                    && self.rides_fork(step, &inbound).matches()
                    && gate::content_holds(step, &inbound, &scope, &resolver).matches()
            })
            .or_else(|| {
                gated.iter().find(|step| {
                    gates(step)
                        && self.rides_fork(step, &inbound).matches()
                        && gate::body_content_holds(step, &inbound, &scope).matches()
                        && self.takes_header_divergent(step, &inbound, actor, &leg)
                })
            });
        let tolerated = matched
            .filter(|step| !gate::discriminates(step, &inbound).matches())
            .and_then(|step| match (&step.discriminator, inbound.status) {
                (Discriminator::Response { status, .. }, Some(arrived)) => {
                    Some(cancel::note(*status, arrived))
                }
                _ => None,
            });
        let Some(step) = matched.cloned() else {
            return self
                .charge_unmatched(&leg, &inbound, &message, &gated, confirmed_before, seq)
                .await;
        };

        // A better candidate for the step that was holding one out: the held
        // datagram was the policy's all along, so it goes there now.
        if self.held.as_ref().is_some_and(|h| h.step == step.id) && !self.concede_held().await {
            return false;
        }
        if let Some(seq) = seq {
            self.instance.recording().attribute(&leg, seq, &step.id);
            if let Some(note) = &tolerated {
                self.instance.recording().renote(&leg, seq, note);
            }
        }
        // The step now owns these bytes: every later copy of them on this leg is
        // a repeat of THIS message, which is the unit §6.9 counts.
        self.repeats.claim(
            &step.id,
            &leg,
            LadderSide::Expect,
            step.retransmits,
            &step.retransmit_intervals_ms,
            &bytes,
            self.now_us(),
        );
        // An OBSERVED fork binds on CONSUMPTION: the step took this arrival, so
        // its id now names the dialog under the To-tag it carried — for every
        // later step naming the id, and for `${early:….tag}` (§6.1).
        if let Some(early) = &step.early {
            if self.early.tag(&leg, early).is_none() && self.learned.tag(&leg, early).is_none() {
                if let Some(tag) = inbound.to_tag.clone() {
                    self.learned.bind(&leg, early, &tag);
                    self.instance.mint_early(early, &leg, &tag);
                }
            }
        }
        self.instance.record_step(&step.id, outcome_of(&inbound));
        // Every header the step declared is READ, and each one that did not hold
        // is recorded by name (§9.1) — the frozen values the match was scoped
        // out of, and the ones it took the datagram in spite of.
        let findings = {
            let resolver = Resolver::new(self.instance.state(), &self.instance.config().identities);
            gate::header_findings(&step, &inbound, &resolver)
        };
        for finding in findings {
            self.instance.record(finding);
        }
        // A body assertion a lane-declared known bug took out of the match is
        // still READ, and what it found is recorded — the match was bought, and
        // the verdict says so.
        let waived = gate::waived_body_findings(&step, &inbound, &self.instance.scope());
        for (bug, finding) in waived {
            self.instance.record_waived(bug, finding);
        }
        // A finding is a fact about the verdict, never about the flow: the
        // message matched, so what comes after it is still composable and the
        // run goes on (§11.2) — gating fails the run, not the script.
        self.run_checks(&step, &inbound);
        self.complete(&step);
        true
    }

    /// Charge a datagram no armed expect on `leg` matched: to the expect of the
    /// transaction it rides (RFC 3261 §17.1.3), else to the step it came CLOSEST
    /// to. One whose discriminator matched and whose content did not says exactly
    /// what is wrong, where an unordered group's first member would describe a
    /// step the datagram was never for (`progress`).
    ///
    /// `false` when the arrival leaves the leg nothing it can still be satisfied
    /// by: the script is ended here (§11.2) and the run goes on to its close.
    async fn charge_unmatched(
        &mut self,
        leg: &str,
        inbound: &Inbound,
        message: &SipMessage,
        gated: &[CompiledStep],
        confirmed_before: bool,
        seq: Option<u64>,
    ) -> bool {
        let scope = self.instance.scope();
        let resolver = Resolver::new(self.instance.state(), &self.instance.config().identities);
        let ladder = self.instance.recording().legs().remove(leg).unwrap_or_default();
        let (closest, own_transaction, retired, released, ends) = {
            let cursor = self.instance.cursor();
            let items = &self.instance.plan().program().items;
            let armed: Vec<progress::Armed<'_>> = gated
                .iter()
                .map(|step| progress::Armed {
                    step,
                    opener: cursor.opening_send(&step.id),
                    retirable: items[step.loc.item].kind == ItemKind::Message,
                })
                .collect();
            let unmatched = progress::unmatched(inbound, &armed, &ladder, confirmed_before)
                .expect("the leg has an armed expect");
            (
                unmatched.charged.clone(),
                unmatched.own_transaction,
                unmatched.retired,
                unmatched.released,
                unmatched.ends,
            )
        };
        let reason = match gate::discriminates(&closest, inbound) {
            GateVerdict::Rejects(why) => why,
            GateVerdict::Matches => match self.rides_fork(&closest, inbound) {
                GateVerdict::Rejects(why) => why,
                GateVerdict::Matches => {
                    match gate::content_holds(&closest, inbound, &scope, &resolver) {
                        GateVerdict::Rejects(why) => why,
                        GateVerdict::Matches => "no armed expect matched".into(),
                    }
                }
            },
        };
        let reason = if own_transaction {
            format!("{reason}; on the {} transaction {} waits on", inbound.cseq_method, closest.id)
        } else {
            reason
        };
        let also_armed: Vec<&str> =
            gated.iter().map(|s| s.id.as_str()).filter(|id| *id != closest.id).collect();
        let reason = if also_armed.is_empty() {
            reason
        } else {
            format!("{reason}; also armed: {}", also_armed.join(", "))
        };
        let step = closest.id.clone();
        self.instance.fail(Failure::UnmatchedDatagram {
            step: step.clone(),
            leg: leg.to_string(),
            gated_on: (&closest.discriminator).into(),
            reason,
            arrived: inbound.arrived(),
        });
        // What the leg owes for this datagram is the scripted tail's where
        // the run goes on past a retired step; otherwise it is composed here,
        // as for any arrival the flow refused.
        if retired.is_empty() || ends {
            self.answer_unscripted(leg, message).await;
        }
        // A final ended the transaction the charged step waits on: the step
        // is retired, the tolerated absences on that transaction released,
        // and what stands behind them measures from now. No timing note and
        // no outcome: the step took nothing.
        let now = Instant::now();
        for id in &released {
            if self.instance.release_step(id) {
                self.completed_at.insert(id.clone(), now);
            }
        }
        for id in &retired {
            if self.instance.retire_step(id) {
                self.completed_at.insert(id.clone(), now);
            }
        }
        if !retired.is_empty() || !released.is_empty() {
            if let Some(seq) = seq {
                let note = format!(
                    "the final of the transaction {step} waits on: {} where the step names {}; {step} {}",
                    inbound.status.unwrap_or(0),
                    closest.discriminator,
                    if retired.is_empty() { "released" } else { "retired" }
                );
                self.instance.recording().renote(leg, seq, &note);
            }
            self.settle_blocks(now);
        }
        // The run goes on unless this arrival left the leg with nothing it
        // can still be satisfied by (§11.2).
        if !ends {
            return true;
        }
        self.end_script(Some(leg), Some(&step));
        false
    }
}
