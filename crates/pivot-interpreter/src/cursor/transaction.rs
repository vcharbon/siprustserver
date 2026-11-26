//! The answers list order does not hold back because their transaction says
//! when they come: an answer to a transaction this leg opened (§6.7c), the
//! answers a CANCEL this leg received draws (§6.7e), and the ACK a final this
//! leg took is owed and the answer a non-INVITE request it took is owed
//! (§6.7h).

use sip_message::Method;

use super::{Cursor, StepStatus};
use crate::plan::{CompiledStep, Discriminator};

impl<'p> Cursor<'p> {
    /// Whether every step of an item answers a transaction THIS leg has already
    /// opened (§6.7c): an `expect` naming its transaction (`status` plus
    /// `cseq-method`), whose method no REQUIRED expect armed here answers and
    /// whose discriminator no armed tolerated absence shares (one
    /// discriminator armed twice would let either step take the other's
    /// datagram), and whose request this leg sent at a completed step.
    /// RFC 3261 §17 orders no two answers on two open transactions, so such a
    /// step arms beside whatever stands in front of it; within one transaction
    /// list order binds.
    pub(super) fn answers_open_transaction(&self, frontier: &[String], armed: &[String]) -> bool {
        !frontier.is_empty()
            && frontier.iter().all(|id| {
                let Some(step) = self.plan.step(id) else { return false };
                let Discriminator::Response { cseq_method: Some(method), .. } = &step.discriminator
                else {
                    return false;
                };
                let method = Method::from_wire(method);
                step.is_expect()
                    && !armed.iter().any(|prev| {
                        self.plan.step(prev).is_some_and(|s| {
                            answers(s, &method).is_some()
                                && (!s.optional_expect() || s.discriminator == step.discriminator)
                        })
                    })
                    && self.transaction_open(step, &method)
            })
    }

    /// Whether the transaction `step` answers is open at `step`'s place in its
    /// leg: the leg's last send of `method` before it has completed, and no
    /// answer to that send stands between the two — a pending REQUIRED one,
    /// which would be the transaction's earlier turn, or a settled final,
    /// which ended it. A pending tolerated absence is no earlier turn: the
    /// document allows it never to come.
    fn transaction_open(&self, step: &CompiledStep, method: &Method) -> bool {
        let Some((opened, behind)) = self.opener_along(step, method) else { return false };
        if self.steps.get(&opened.id).copied() != Some(StepStatus::Complete) {
            return false;
        }
        behind.iter().all(|s| match (answers(s, method), self.steps.get(&s.id)) {
            (Some(_), Some(StepStatus::Pending)) => s.optional_expect(),
            (Some(_), Some(StepStatus::Retired)) => false,
            (Some(status), Some(StepStatus::Complete)) => status < 200,
            _ => true,
        })
    }

    /// The send that opened the transaction an expect waits on: the leg's last
    /// `send` of the expect's `cseq-method` standing before it in leg order.
    /// `None` on a step naming no transaction, and on one whose leg sent no
    /// such request before it (a relay of another leg's origination).
    pub fn opening_send(&self, step: &str) -> Option<&'p str> {
        let step = self.plan.step(step)?;
        let Discriminator::Response { cseq_method: Some(method), .. } = &step.discriminator else {
            return None;
        };
        let (opened, _) = self.opener_along(step, &Method::from_wire(method))?;
        Some(opened.id.as_str())
    }

    /// Whether retiring `step`, an expect on a transaction its leg opened, arms
    /// another expect waiting on that same transaction: whether a final the
    /// step was charged with can still be taken by a step of its own.
    pub fn retiring_arms_its_transaction(&self, step: &str) -> bool {
        let Some(compiled) = self.plan.step(step) else { return false };
        let Some(opener) = self.opening_send(step) else { return false };
        let Discriminator::Response { cseq_method: Some(method), .. } = &compiled.discriminator
        else {
            return false;
        };
        let method = Method::from_wire(method);
        let mut after = self.clone();
        if !after.retire(step) {
            return false;
        }
        after.frontier().iter().filter_map(|id| self.plan.step(id)).any(|s| {
            s.leg == compiled.leg
                && answers(s, &method).is_some()
                && after.opening_send(&s.id) == Some(opener)
        })
    }

    /// The last send of `method` before `step` on its leg, and every step of
    /// the leg standing between the two, in leg order.
    fn opener_along(
        &self,
        step: &CompiledStep,
        method: &Method,
    ) -> Option<(&'p CompiledStep, Vec<&'p CompiledStep>)> {
        let key = self.leg_key(&step.id);
        let mut along: Vec<&'p CompiledStep> = self
            .plan
            .steps()
            .into_iter()
            .filter(|s| s.leg == step.leg && self.leg_key(&s.id) < key)
            .collect();
        along.sort_by_key(|s| self.leg_key(&s.id));
        let opened = along.iter().rposition(|s| {
            s.is_send()
                && matches!(&s.discriminator, Discriminator::Request { method: m }
                    if Method::from_wire(m) == *method)
        })?;
        let behind = along.split_off(opened + 1);
        Some((along[opened], behind))
    }

    /// Whether every step of an item is an answer a CANCEL this leg RECEIVED
    /// draws (§6.7e): the `200` to that CANCEL and the INVITE's non-2xx final
    /// (RFC 3261 §9.2). The transaction layer owes both at once, so neither
    /// waits behind an arrival of another transaction the leg stands on; the
    /// CANCEL must have completed, and each send still fires at its own dwell.
    pub(super) fn answers_received_cancel(&self, frontier: &[String]) -> bool {
        !frontier.is_empty()
            && frontier.iter().all(|id| {
                let Some(step) = self.plan.step(id) else { return false };
                let Discriminator::Response { status, cseq_method: Some(method) } =
                    &step.discriminator
                else {
                    return false;
                };
                let owed = match Method::from_wire(method) {
                    Method::Cancel => *status == 200,
                    Method::Invite => *status >= 300,
                    _ => false,
                };
                step.is_send() && owed && self.cancel_received_before(step)
            })
    }

    /// Whether every step of an item is owed by what this leg TOOK (§6.7h):
    /// the ACK to a final it took ([`Self::acked_final`]), or the answer to a
    /// non-INVITE request it took ([`Self::answered_request`]). What it is
    /// owed for has arrived, so it waits on no arrival of another transaction
    /// the leg still awaits (RFC 3261 §13.2.2.4 for a 2xx, §17.1.1.3 for a
    /// non-2xx, §17.2.2 for a non-INVITE request).
    pub(super) fn owed_for_what_the_leg_took(&self, frontier: &[String]) -> bool {
        !frontier.is_empty()
            && frontier.iter().all(|id| {
                self.plan.step(id).is_some_and(|s| {
                    self.acked_final(s).is_some() || self.answered_request(s).is_some()
                })
            })
    }

    /// The non-INVITE request `step`, a `send` of a response to it, answers,
    /// where an expect of the leg has taken it: the leg's last `expect` of a
    /// request of that method before `step`, completed. `None` on any other
    /// step, an INVITE's answer included: RFC 3262 §3 orders it behind the
    /// PRACK it waits on, so it keeps list order.
    pub fn answered_request(&self, step: &CompiledStep) -> Option<&'p CompiledStep> {
        let Discriminator::Response { cseq_method: Some(method), .. } = &step.discriminator else {
            return None;
        };
        let method = Method::from_wire(method);
        if !step.is_send() || method == Method::Invite {
            return None;
        }
        let key = self.leg_key(&step.id);
        let request = self
            .plan
            .steps()
            .into_iter()
            .filter(|s| s.leg == step.leg && self.leg_key(&s.id) < key && s.is_expect())
            .filter(|s| {
                matches!(&s.discriminator, Discriminator::Request { method: m }
                    if Method::from_wire(m) == method)
            })
            .max_by_key(|s| self.leg_key(&s.id))?;
        (self.steps.get(&request.id).copied() == Some(StepStatus::Complete)).then_some(request)
    }

    /// The INVITE final `step`, a `send` of an ACK, acknowledges, where an
    /// expect of the leg has taken it: the leg's last `expect` of an INVITE
    /// final before `step`, completed, with no INVITE or ACK sent between the
    /// two. `None` on any other step.
    pub fn acked_final(&self, step: &CompiledStep) -> Option<&'p CompiledStep> {
        let is_request = |s: &CompiledStep, wanted: Method| {
            matches!(&s.discriminator, Discriminator::Request { method }
                if Method::from_wire(method) == wanted)
        };
        if !step.is_send() || !is_request(step, Method::Ack) {
            return None;
        }
        let key = self.leg_key(&step.id);
        let mut along: Vec<&'p CompiledStep> = self
            .plan
            .steps()
            .into_iter()
            .filter(|s| s.leg == step.leg && self.leg_key(&s.id) < key)
            .collect();
        along.sort_by_key(|s| self.leg_key(&s.id));
        let last = along.into_iter().rev().find(|s| {
            (s.is_send() && (is_request(s, Method::Invite) || is_request(s, Method::Ack)))
                || answers(s, &Method::Invite).is_some_and(|status| status >= 200)
        })?;
        (last.is_expect() && self.steps.get(&last.id).copied() == Some(StepStatus::Complete))
            .then_some(last)
    }

    /// When a pending `step` the leg sends past what it awaits (§6.7h) goes
    /// out, where a required step before it on its leg is still pending:
    /// the ACK to a non-2xx final on that final's arrival, the ACK to a 2xx
    /// and the answer to a request at the document's offset.
    pub fn held_send(&self, step: &str) -> Option<HeldTiming<'p>> {
        let step = self.plan.step(step)?;
        if self.steps.get(&step.id).copied() != Some(StepStatus::Pending) {
            return None;
        }
        let timing = match self.acked_final(step) {
            Some(refusal) if is_refusal(refusal) => HeldTiming::OnArrivalOf(refusal),
            Some(_) => HeldTiming::AtCapturedOffset,
            None => self.answered_request(step).map(|_| HeldTiming::AtCapturedOffset)?,
        };
        let key = self.leg_key(&step.id);
        self.plan
            .steps()
            .into_iter()
            .any(|s| {
                s.leg == step.leg
                    && self.leg_key(&s.id) < key
                    && !s.optional_expect()
                    && self.steps.get(&s.id).copied() == Some(StepStatus::Pending)
            })
            .then_some(timing)
    }

    /// Whether the leg's last `expect` of a CANCEL standing before `step` has
    /// completed.
    fn cancel_received_before(&self, step: &CompiledStep) -> bool {
        let key = self.leg_key(&step.id);
        self.plan
            .steps()
            .into_iter()
            .filter(|s| s.leg == step.leg && self.leg_key(&s.id) < key && s.is_expect())
            .filter(|s| {
                matches!(&s.discriminator, Discriminator::Request { method }
                    if Method::from_wire(method) == Method::Cancel)
            })
            .max_by_key(|s| self.leg_key(&s.id))
            .is_some_and(|s| self.steps.get(&s.id).copied() == Some(StepStatus::Complete))
    }
}

/// When a send held past what its leg awaits goes out ([`Cursor::held_send`]).
#[derive(Debug, Clone, Copy)]
pub enum HeldTiming<'p> {
    /// When that step's datagram arrived: the ACK to a non-2xx final is its
    /// client transaction's own (RFC 3261 §17.1.1.3).
    OnArrivalOf(&'p CompiledStep),
    /// At the document's offset from what it answers, read up its anchor
    /// chain to the nearest anchor that has happened.
    AtCapturedOffset,
}

/// Whether `step` takes an INVITE final above 2xx.
fn is_refusal(step: &CompiledStep) -> bool {
    matches!(step.discriminator, Discriminator::Response { status, .. } if status >= 300)
}

/// The status an expect waits for on a response to `method`, where it does.
fn answers(step: &CompiledStep, method: &Method) -> Option<u16> {
    match &step.discriminator {
        Discriminator::Response { status, cseq_method: Some(m) }
            if step.is_expect() && Method::from_wire(m) == *method =>
        {
            Some(*status)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
