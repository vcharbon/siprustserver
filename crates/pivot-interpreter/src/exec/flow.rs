//! The flow loop: readiness, dwells, expect budgets, and the wait between them.

use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use pivot_schema::bundle::Failure;
use pivot_schema::flow::Anchor;
use scenario_harness::Inbound as HarnessInbound;
use sip_message::SipMessage;
use tokio::time::Instant;

use super::stall::Stall;
use super::Runner;
use crate::plan::CompiledStep;

impl<'a, 'p> Runner<'a, 'p> {
    /// The flow loop. `false` once a failure aborts it.
    pub(super) async fn run_flow(&mut self) -> bool {
        // A run must never hang. Every turn of this loop either moves the flow
        // or advances the clock; if neither happens for a long stretch the loop
        // cannot make progress, and saying so beats spinning forever.
        let mut stall = Stall::new(self.wall_ceiling());
        loop {
            if let Some(failure) = stall.check("the flow", || self.instance.cursor().frontier()) {
                self.instance.fail(failure);
                self.end_script(None, None);
                return false;
            }
            if !self.refuse_injects() {
                return false;
            }
            // A rung due goes out BEFORE the turn's steps: its instant was
            // booked first, and the peer paces its own answer off it.
            if !self.drain_repeats().await {
                return false;
            }
            if !self.emit_ready().await {
                return false;
            }
            if self.instance.cursor().is_done() {
                // A hold-out the flow outlived belongs to the policy: no step is
                // left to take it, and the settle must not meet an unanswered
                // request the run is still carrying.
                return self.concede_held().await;
            }
            self.arm_expects();
            let Some(wake) = self.next_wake() else {
                // Nothing to send and nothing to wait for, yet the flow is not
                // done: the run cannot progress and says so.
                let pending = self.instance.cursor().pending();
                self.instance.fail(Failure::FlowIncomplete { pending });
                self.end_script(None, None);
                return false;
            };
            match self.wait(wake).await {
                Wake::Timer => {
                    // The hold-out closes BEFORE any budget does: the datagram
                    // it holds is what an armed expect on this leg was waiting
                    // for, and expiring that expect first would fail the run on
                    // a message the run is still carrying.
                    if self.held.as_ref().is_some_and(|h| h.release_at <= Instant::now())
                        && !self.release_held().await
                    {
                        return false;
                    }
                    if !self.expire_expects() {
                        return false;
                    }
                }
                Wake::Message { actor, message } => {
                    if !self.dispatch(&actor, *message).await {
                        return false;
                    }
                }
                Wake::Closed { actor, detail } => {
                    self.instance.fail(Failure::TransportClosed { actor, detail });
                    // The close still runs: only THIS actor's transport is gone,
                    // and what another leg holds open is still endable.
                    self.end_script(None, None);
                    return false;
                }
            }
        }
    }

    /// An `inject` the lane cannot perform stops the run by name. `inject` is
    /// shape-only in this program: no interpreter executes an action.
    pub(super) fn refuse_injects(&mut self) -> bool {
        let open = self.instance.cursor().open_injects();
        if open.is_empty() {
            return true;
        }
        let first = open.first().map(|(node, _)| node.to_string());
        for (node, action) in open {
            self.instance.fail(Failure::InjectorMissing {
                node: node.to_string(),
                action: action.to_string(),
            });
        }
        let leg = first
            .as_deref()
            .and_then(|node| self.instance.plan().step(node).map(|s| s.leg.clone()));
        self.end_script(leg.as_deref(), first.as_deref());
        false
    }

    /// Emit every send whose leg is at it, whose `after` holds and whose dwell
    /// has elapsed.
    pub(super) async fn emit_ready(&mut self) -> bool {
        loop {
            let Some(step) = self.next_send() else { return true };
            if !self.emit(&step, None).await {
                // A send the run cannot compose, pace or put on the wire is a
                // script it cannot keep following (§11.2): the close ends what
                // the legs still hold.
                self.end_script(Some(&step.leg), Some(&step.id));
                return false;
            }
        }
    }

    /// The next send that is ready NOW.
    pub(super) fn next_send(&self) -> Option<CompiledStep> {
        let now = Instant::now();
        self.instance
            .cursor()
            .frontier()
            .iter()
            .filter_map(|id| self.instance.plan().step(id))
            .find(|step| step.is_send() && self.dwell_deadline(step).is_some_and(|at| at <= now))
            .cloned()
    }

    /// When a `delay`'s anchor happened, or `None` while it has not.
    ///
    /// A duration measured from a node that has not happened is not a duration,
    /// and defaulting it to the run's start would place it before the message it
    /// is anchored on. An anchor that is RELEASED (a tolerated absence) settles
    /// at its release, and a BLOCK settles when the block completes, so neither
    /// an `optional` nor an `alt` anchor wedges what is anchored on it.
    pub(super) fn anchor_at(&self, anchor: &Anchor) -> Option<Instant> {
        match anchor {
            Anchor::Trigger => Some(self.started),
            Anchor::Step(id) => self.completed_at.get(id).copied(),
        }
    }

    /// When a step's dwell expires: its anchor's own time plus `delay.ms`.
    ///
    /// On a SEND this is when the message goes out; on an EXPECT it is when the
    /// budget opens (see [`Self::arm_expects`]).
    ///
    /// A compressible dwell on a virtual clock still rides — virtual time costs
    /// nothing and the dwell carries the causal order — while a `timer_linked`
    /// one is what a system timer measures and is never shortened.
    pub(super) fn dwell_deadline(&self, step: &CompiledStep) -> Option<Instant> {
        Some(self.anchor_at(&step.delay.from)? + Duration::from_millis(step.delay.ms))
    }

    /// Open the budget of every frontier expect whose dwell is known and whose
    /// budget is not open yet.
    ///
    /// It opens at the LATER of the leg reaching the step and the step's own
    /// dwell — the very `anchor + delay.ms` a send sleeps. The dwell gates when
    /// the budget STARTS, never the match: an expect whose budget has not opened
    /// is as matchable as one whose has, it just cannot expire. Opening on the
    /// frontier alone would spend the budget across the dwell itself, failing a
    /// faithful run for a message the flow has not yet been asked to provoke.
    pub(super) fn arm_expects(&mut self) {
        let now = Instant::now();
        let opening: Vec<(String, Instant)> = self
            .instance
            .cursor()
            .frontier()
            .into_iter()
            .filter_map(|id| {
                let step = self.instance.plan().step(&id)?;
                if !step.is_expect() {
                    return None;
                }
                let opens = now.max(self.dwell_deadline(step)?);
                Some((id, opens))
            })
            .collect();
        for (id, opens) in opening {
            self.armed_at.entry(id).or_insert(opens);
        }
    }

    /// The next moment the loop must wake: a dwell, or an expect's budget.
    ///
    /// A step whose anchor has not settled has no deadline yet — for either op,
    /// what wakes the run is the anchor's own arrival.
    pub(super) fn next_wake(&self) -> Option<Instant> {
        let mut soonest: Option<Instant> = None;
        for id in self.instance.cursor().frontier() {
            let Some(step) = self.instance.plan().step(&id) else { continue };
            let at = if step.is_send() {
                match self.dwell_deadline(step) {
                    Some(at) => at,
                    None => continue,
                }
            } else {
                match self.armed_at.get(&id) {
                    Some(opened) => *opened + Duration::from_millis(step.within_ms),
                    None => continue,
                }
            };
            soonest = Some(soonest.map_or(at, |s: Instant| s.min(at)));
        }
        // A rung owed is a wake owed: the flow may have nothing else pending
        // and the ladder must still go out on time.
        if let Some(at) = self.next_repeat() {
            soonest = Some(soonest.map_or(at, |s: Instant| s.min(at)));
        }
        // A hold-out that nothing better closed must still close: the datagram
        // it holds is one an armed expect would otherwise have taken.
        if let Some(at) = self.held.as_ref().map(|h| h.release_at) {
            soonest = Some(soonest.map_or(at, |s: Instant| s.min(at)));
        }
        soonest
    }

    /// Wait for the first of: a datagram on any bound socket, or `wake`.
    pub(super) async fn wait(&mut self, wake: Instant) -> Wake {
        let mut pumps = FuturesUnordered::new();
        // One receive future per distinct UA stack: two actors on one socket
        // share a queue, and pumping it twice would race the same datagram.
        let mut seen = std::collections::BTreeSet::new();
        for (actor, agent) in &self.lane.agents {
            if !seen.insert(agent.stack_id()) {
                continue;
            }
            let actor = actor.clone();
            let agent = agent.clone();
            pumps.push(async move {
                let inbound = agent.recv_any().await;
                (actor, inbound)
            });
        }
        tokio::select! {
            biased;
            Some((actor, inbound)) = pumps.next() => match inbound {
                Ok(HarnessInbound::Request(txn)) => Wake::Message {
                    actor,
                    message: Box::new(SipMessage::Request(txn.request().clone())),
                },
                Ok(HarnessInbound::Response(response)) => Wake::Message {
                    actor,
                    message: Box::new(SipMessage::Response(*response)),
                },
                // A per-recv timeout is "nothing arrived yet", not an error: the
                // loop's own deadlines decide when waiting has gone on too long.
                Err(scenario_harness::StepError::Timeout { .. }) => Wake::Timer,
                Err(e) => Wake::Closed { actor, detail: e.to_string() },
            },
            () = tokio::time::sleep_until(wake) => Wake::Timer,
        }
    }

    /// A budget that ran out. A required expect FAILS and ENDS THE SCRIPT — the
    /// message it waits for is missing, so the run cannot compose what comes
    /// after it (§11.2); an `optional` one is released, because the budget is how
    /// long the document tolerates the absence it already declared — and a
    /// release is the only rule that does not wedge the steps behind it forever.
    pub(super) fn expire_expects(&mut self) -> bool {
        let now = Instant::now();
        let expired: Vec<CompiledStep> = self
            .instance
            .cursor()
            .frontier()
            .iter()
            .filter_map(|id| self.instance.plan().step(id).map(|s| (id.clone(), s)))
            .filter(|(id, step)| {
                step.is_expect()
                    && self.armed_at.get(id).is_some_and(|opened| {
                        now >= *opened + Duration::from_millis(step.within_ms)
                    })
            })
            .map(|(_, step)| step.clone())
            .collect();
        let mut failed = false;
        for step in expired {
            if step.optional_expect() {
                if self.instance.release_step(&step.id) {
                    self.completed_at.insert(step.id.clone(), now);
                    self.settle_blocks(now);
                }
                continue;
            }
            self.instance.fail(Failure::ExpectTimedOut {
                step: step.id.clone(),
                leg: step.leg.clone(),
                gated_on: step.discriminator.to_string(),
                within_ms: step.within_ms,
            });
            self.end_script(Some(&step.leg), Some(&step.id));
            failed = true;
        }
        !failed
    }
}

/// What woke the run loop.
pub(super) enum Wake {
    /// A deadline, or a per-receive timeout that means "nothing yet".
    Timer,
    /// A datagram at one actor. Boxed: a parsed message dwarfs the other
    /// variants, and this enum is created once per loop turn.
    Message { actor: String, message: Box<SipMessage> },
    /// A socket closed or errored under the run, in the transport's own words.
    Closed { actor: String, detail: String },
}
