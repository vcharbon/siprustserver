//! The §5.1 arbitration between an armed expect and a `background` policy for one datagram.

use tokio::time::Instant;

use super::Runner;
use crate::gate::{self, Inbound};
use crate::plan::CompiledStep;
use crate::resolve::Resolver;

/// How long a step holds a datagram out for a better candidate.
///
/// A BACKSTOP, not a wait: the hold normally ends on the next arrival, and this
/// is only reached when the leg goes silent — which is itself the answer, that
/// nothing better was coming. So it is sized to outlast one relay hop on any
/// lane the interpreter drives (a simulated fabric's 100 ms, a real lane's
/// single-digit milliseconds) while staying far inside the smallest expect
/// budget a document states, since a step that reaches the backstop completes
/// this much later than it otherwise would.
pub(super) const HOLD_OUT_MS: u64 = 500;

/// The recording note a held datagram carries while the hold-out is open.
pub(super) const HELD_NOTE: &str =
    "held: the armed expect froze headers this datagram carries none of, so it waits to see \
     whether a better candidate for that step lands in the same instant";

/// The note it carries once the hold-out closed with nothing better.
pub(super) const HELD_TAKEN: &str =
    "held out for a better candidate; none landed, so the step took this one";

impl<'a, 'p> Runner<'a, 'p> {
    /// Whether `step` may take a datagram whose frozen HEADER SET did not hold.
    ///
    /// Arbitration, never tolerance: the relaxed reading stands unless something
    /// ELSE on this leg could be the datagram's owner. Only a `background`
    /// policy can be — a policy answers a METHOD outside the flow (§5.1) and
    /// never a response — and the one thing on the wire that tells the two
    /// readings apart is whether the datagram carries any of the vocabulary the
    /// step froze ([`gate::declared_headers_carried`]). Nothing contests a
    /// datagram whose discriminator, leg and fork are already the step's.
    pub(super) fn takes_header_divergent(
        &self,
        step: &CompiledStep,
        inbound: &Inbound,
        actor: &str,
        leg: &str,
    ) -> bool {
        let Some(method) = inbound.method.as_deref() else { return true };
        if self.background_policy(actor, method, Some(leg)).is_none() {
            return true;
        }
        let (declared, carried) = gate::declared_headers_carried(step, inbound);
        declared == 0 || carried > 0
    }

    /// The step, if any, that holds THIS datagram out for a better candidate.
    ///
    /// Every clause is load-bearing. A background policy must answer the method,
    /// or there is no second reading to hold out for. The step must be one an
    /// armed expect would MATCH, so the hold-out never changes what a datagram
    /// no step wanted does. And the step must have frozen at least one header
    /// while the datagram carries none of them: a step that froze nothing has
    /// nothing to tell the two readings apart with, and one whose headers are
    /// partly there is looking at the message it is about.
    ///
    /// Only ONE datagram is ever held, and only for [`HOLD_OUT_MS`]: the hold
    /// is a tie-break between two arrivals of one instant, never a wait.
    pub(super) fn holds_out_for_better(
        &self,
        leg: Option<&str>,
        actor: &str,
        inbound: &Inbound,
    ) -> Option<String> {
        if self.held.is_some() {
            return None;
        }
        let leg = leg?;
        let method = inbound.method.as_deref()?;
        self.background_policy(actor, method, Some(leg))?;
        let scope = self.instance.scope();
        let resolver = Resolver::new(self.instance.state(), &self.instance.config().identities);
        let now = Instant::now();
        self.instance.cursor().frontier().iter().find_map(|id| {
            let step = self.instance.plan().step(id)?;
            let armed = self.armed_at.get(id).is_some_and(|opened| *opened <= now);
            if !armed || !step.is_expect() || step.leg != leg {
                return None;
            }
            if !gate::discriminates(step, inbound).matches()
                || !self.rides_fork(step, inbound).matches()
                || !gate::content_holds(step, inbound, &scope, &resolver).matches()
            {
                return None;
            }
            let (declared, carried) = gate::declared_headers_carried(step, inbound);
            (declared > 0 && carried == 0).then(|| step.id.clone())
        })
    }

    /// Give the held datagram to the background policy it was held against: the
    /// step took a better candidate, so this one was the policy's all along.
    pub(super) async fn concede_held(&mut self) -> bool {
        let Some(held) = self.held.take() else { return true };
        let Some(method) = held.inbound.method.as_deref() else { return true };
        let Some((index, policy)) = self.background_policy(&held.actor, method, Some(&held.leg))
        else {
            return true;
        };
        if let Some(seq) = held.seq {
            self.instance.recording().renote(&held.leg, seq, "background policy");
        }
        self.instance.note_background_answered(index);
        if !self.answer_background(&held.actor, &held.leg, &held.message, policy.status).await {
            self.end_script(Some(&held.leg), None);
            return false;
        }
        true
    }

    /// Nothing better landed inside the hold-out: the datagram goes to the flow
    /// exactly as it would have without the hold.
    pub(super) async fn release_held(&mut self) -> bool {
        let Some(held) = self.held.take() else { return true };
        self.deliver(
            &held.actor.clone(),
            held.inbound,
            held.message,
            held.bytes,
            held.repeat,
            held.seq,
        )
        .await
    }
}
