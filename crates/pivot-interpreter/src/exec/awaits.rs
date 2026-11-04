//! The sends that await a lane event (§6.7g): refused up front where the lane
//! cannot report the event, released one per occurrence, failed at their
//! budget where the event never comes.

use std::time::Duration;

use pivot_schema::bundle::Failure;
use tokio::time::Instant;

use super::Runner;
use crate::plan::CompiledStep;

impl<'a, 'p> Runner<'a, 'p> {
    /// Refuse the run, before anything is dialled, when a send awaits an event
    /// the lane does not report: such a send could only ever time out.
    pub(super) fn refuse_unsupported_events(&mut self) -> bool {
        let refused: Vec<(String, String)> = self
            .instance
            .plan()
            .steps()
            .into_iter()
            .filter_map(|step| Some((step.id.clone(), step.awaits.clone()?)))
            .filter(|(_, event)| !self.lane.events.as_ref().is_some_and(|e| e.supports(event)))
            .collect();
        if refused.is_empty() {
            return true;
        }
        for (step, event) in refused {
            self.instance.fail(Failure::LaneEventUnsupported { step, event });
        }
        self.end_script(None, None);
        false
    }

    /// Whether `step`'s awaited event, if any, has an occurrence waiting.
    pub(super) fn event_released(&self, step: &CompiledStep) -> bool {
        match &step.awaits {
            None => true,
            Some(event) => self.lane.events.as_ref().is_some_and(|e| e.ready(event)),
        }
    }

    /// Hand `step` the occurrence that released it.
    pub(super) fn consume_event(&mut self, step: &CompiledStep) {
        if let (Some(event), Some(events)) = (&step.awaits, self.lane.events.as_mut()) {
            events.consume(event);
        }
    }

    /// When a held send gives up on its event: its dwell plus its budget.
    pub(super) fn event_deadline(&self, step: &CompiledStep) -> Option<Instant> {
        Some(self.dwell_deadline(step)? + Duration::from_millis(step.within_ms))
    }

    /// Fail every frontier send whose awaited event did not come inside its
    /// budget, and end the script there.
    pub(super) fn expire_awaits(&mut self) -> bool {
        let now = Instant::now();
        let expired: Vec<CompiledStep> = self
            .instance
            .cursor()
            .frontier()
            .iter()
            .filter_map(|id| self.instance.plan().step(id))
            .filter(|step| step.is_send() && !self.event_released(step))
            .filter(|step| self.event_deadline(step).is_some_and(|at| at <= now))
            .cloned()
            .collect();
        let Some(first) = expired.first().cloned() else { return true };
        for step in expired {
            self.instance.fail(Failure::LaneEventMissing {
                step: step.id.clone(),
                leg: step.leg.clone(),
                event: step.awaits.clone().unwrap_or_default(),
                within_ms: step.within_ms,
            });
        }
        self.end_script(Some(&first.leg), Some(&first.id));
        false
    }
}
