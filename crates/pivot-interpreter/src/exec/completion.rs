//! A step done: its checks, its timer reading, and the blocks and anchors it settles.

use pivot_schema::bundle::{Failure, TimingNote};
use tokio::time::Instant;

use super::Runner;
use crate::checks::{self, MessageObservables};
use crate::gate::Inbound;
use crate::plan::CompiledStep;
use crate::resolve::Resolver;
use crate::scope::Finding;
use crate::state::StepOutcome;

impl<'a, 'p> Runner<'a, 'p> {
    /// A matched step's inline checks (§9), each routed by its class (§9.1).
    ///
    /// Every finding lands on the verdict — gating fails the run, a class the
    /// lane does not gate on is informative — and the step completes either
    /// way: a check reads the message, it never makes the flow unfollowable.
    pub(super) fn run_checks(&mut self, step: &CompiledStep, inbound: &Inbound) {
        if step.checks.is_empty() {
            return;
        }
        let site = format!("step {:?}", step.id);
        let findings: Vec<Finding> = {
            let resolver = Resolver::new(self.instance.state(), &self.instance.config().identities);
            step.checks
                .iter()
                .filter_map(|check| {
                    checks::evaluate(&site, check, &MessageObservables(inbound), &resolver)
                        .map(|failure| Finding::new(check.class, failure))
                })
                .collect()
        };
        for finding in findings {
            self.instance.record(finding);
        }
    }

    /// Retire every expect on `send`'s leg waiting on the transaction `send`
    /// would have opened: a request never sent draws no response.
    pub(super) fn retire_answers_to(&mut self, send: &CompiledStep) {
        let answers: Vec<String> = self
            .instance
            .plan()
            .steps()
            .into_iter()
            .filter(|s| s.leg == send.leg && s.is_expect())
            .filter(|s| self.instance.cursor().opening_send(&s.id) == Some(send.id.as_str()))
            .map(|s| s.id.clone())
            .collect();
        for id in answers {
            self.instance.retire_step(&id);
        }
    }

    /// Mark a step done and remember when, for the dwells anchored on it.
    pub(super) fn complete(&mut self, step: &CompiledStep) {
        let now = Instant::now();
        self.measure_timer(step, now);
        self.completed_at.insert(step.id.clone(), now);
        for released in self.instance.complete_step(&step.id) {
            self.completed_at.insert(released, now);
        }
        self.settle_blocks(now);
    }

    /// Stamp every block that has just completed, for the dwells anchored on it.
    ///
    /// `alt` scoping leaves the block's own id as the only legal way to anchor
    /// on a branch that ran, so a dwell measured from one must have a time to
    /// measure from. Every path that can retire a step calls this: a block whose
    /// last member is a RELEASED tolerated absence completes exactly as one
    /// whose last member arrived.
    pub(super) fn settle_blocks(&mut self, now: Instant) {
        let settled: Vec<String> = self
            .instance
            .plan()
            .program()
            .items
            .iter()
            .filter(|item| !self.completed_at.contains_key(&item.id))
            .filter(|item| self.instance.cursor().node_complete(&item.id))
            .map(|item| item.id.clone())
            .collect();
        for id in settled {
            self.completed_at.insert(id, now);
        }
    }

    /// A timer-anchored arrival, measured against what the document declares
    /// (§9.2).
    ///
    /// It reads EXPECT steps alone: a `timer_linked` dwell on an expect is what
    /// the SYSTEM's own timer measured, while the same dwell on a send is one
    /// this interpreter itself slept and has nothing to observe. The reading is
    /// recorded whatever it says, and it FAILS the run when the run's stated
    /// tolerance does not cover the difference — a window absorbs a delta, never
    /// an absence and never an order.
    pub(super) fn measure_timer(&mut self, step: &CompiledStep, now: Instant) {
        if !step.is_expect() || !step.delay.timer_linked {
            return;
        }
        let Some(anchor) = self.anchor_at(&step.delay.from) else { return };
        let declared_ms = step.delay.ms;
        let observed_ms = now.saturating_duration_since(anchor).as_millis() as u64;
        let tolerance_ms = self.instance.config().timing_tolerance_ms;
        let absorbed = self.instance.config().absorbs_timing(declared_ms, observed_ms);
        self.instance.note_timing(TimingNote {
            step: step.id.clone(),
            leg: step.leg.clone(),
            declared_ms,
            observed_ms,
            delta_ms: observed_ms as i64 - declared_ms as i64,
            tolerance_ms,
        });
        if !absorbed {
            self.instance.record(Finding::gating(Failure::TimingOutOfTolerance {
                step: step.id.clone(),
                leg: step.leg.clone(),
                declared_ms,
                observed_ms,
                tolerance_ms,
            }));
        }
    }
}

pub(super) fn outcome_of(inbound: &Inbound) -> StepOutcome {
    StepOutcome {
        status: inbound.status,
        cseq: Some(inbound.cseq),
        cseq_method: Some(inbound.cseq_method.clone()),
        rseq: inbound.rseq,
        method: inbound.method.clone(),
        headers: inbound.headers.clone(),
    }
}
