//! The answers list order does not hold back because their transaction says
//! when they come: an answer to a transaction this leg opened (§6.7c), and the
//! answers a CANCEL this leg received draws (§6.7e).

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
