//! What starts the self-gate's intake ramp (ADR-0069): the proxy turning fit
//! after an unfit spell in which it read no worker `Alive` or `Slow`. Both
//! proxies of a POP watch the same workers, so such a spell took the POP out
//! of rotation, and its traffic returns at once. A spell of another cause (a
//! stalled shard, the external face, an isolation that ended before any dead
//! verdict) left a peer proxy carrying the POP, and the start of the process is
//! no return: neither ramps.

use std::time::Duration;

use probe_http::{FitFn, Fitness};
use sip_proxy::self_gate::EluCpsGate;
use sip_proxy::supervise::{self, INTAKE_RAMP};
use tokio::task::JoinHandle;

use crate::readiness::Unfit;

/// How often the fitness is read: well inside the route guard's own reading.
const PERIOD: Duration = Duration::from_millis(250);

/// The fitness readings so far, as the ramp reads them.
#[derive(Debug, Default)]
pub struct RampTrigger {
    fit_once: bool,
    unfit: bool,
    no_worker_serving: bool,
}

impl RampTrigger {
    /// Read one fitness: `true` when it is the fit turn that starts a ramp.
    pub fn read(&mut self, fitness: Fitness) -> bool {
        match fitness {
            Fitness::Fit => {
                let ramp = self.fit_once && self.unfit && self.no_worker_serving;
                *self = Self { fit_once: true, ..Self::default() };
                ramp
            }
            Fitness::Unfit(reason) => {
                self.unfit = true;
                self.no_worker_serving |= reason == Unfit::NoWorkerAlive.reason();
                false
            }
        }
    }
}

/// Spawns the supervised watch: every 250 ms it reads `fit` and starts
/// `gate`'s ramp on the turn [`RampTrigger`] names. A restarted run starts
/// with no memory, so it ramps on no turn before it read the proxy fit.
pub fn spawn_intake_ramp(fit: FitFn, gate: EluCpsGate) -> JoinHandle<()> {
    supervise::spawn(INTAKE_RAMP, move || {
        let (fit, gate) = (fit.clone(), gate.clone());
        async move {
            #[expect(clippy::disallowed_methods, reason = "PERIOD is not zero")]
            let mut tick = tokio::time::interval(PERIOD);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut trigger = RampTrigger::default();
            loop {
                tick.tick().await;
                if trigger.read(fit()) {
                    gate.begin_ramp();
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reads(seq: &[Fitness]) -> Vec<bool> {
        let mut trigger = RampTrigger::default();
        seq.iter().map(|f| trigger.read(*f)).collect()
    }

    const NO_WORKER: Fitness = Fitness::Unfit("no-worker-alive");
    const ISOLATED: Fitness = Fitness::Unfit("isolated");

    #[test]
    fn a_fit_turn_after_a_spell_with_no_worker_serving_ramps_once() {
        let seq = [Fitness::Fit, ISOLATED, NO_WORKER, Fitness::Fit, Fitness::Fit];
        assert_eq!(reads(&seq), [false, false, false, true, false]);
    }

    #[test]
    fn neither_the_process_start_nor_a_spell_of_another_cause_ramps() {
        let start = [Fitness::Unfit("no-first-view"), NO_WORKER, Fitness::Fit];
        assert_eq!(reads(&start), [false, false, false], "the first fit turn is the start");
        for other in ["isolated", "shard-stalled", "external-face"] {
            let seq = [Fitness::Fit, Fitness::Unfit(other), Fitness::Fit];
            assert_eq!(reads(&seq), [false, false, false], "{other}");
        }
    }
}
