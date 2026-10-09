//! One call's SIP round trips on their way to the reporter: the sink the
//! call's mux endpoints feed, and the fold into the reporter when the call
//! ends.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::mux::{Exchange, RttSink};
use crate::report::Reporter;
use crate::scenarios::ScenarioId;

/// The samples one call keeps at most; an exchange measured past it is not
/// reported.
const MAX_SAMPLES: usize = 1024;

/// The round trips of one `scenario` call: held by the call while it runs
/// (the mux dispatch path never touches the reporter) and folded into the
/// reporter once, when it ends.
pub(crate) struct CallRtts {
    reporter: Arc<Reporter>,
    scenario: ScenarioId,
    samples: Arc<Mutex<Vec<(Exchange, Duration)>>>,
}

impl CallRtts {
    pub(crate) fn new(reporter: Arc<Reporter>, scenario: ScenarioId) -> Self {
        Self { reporter, scenario, samples: Arc::default() }
    }

    /// The sink the call's mux network hands every measured exchange to.
    pub(crate) fn sink(&self) -> RttSink {
        let samples = self.samples.clone();
        Arc::new(move |exchange, rtt| {
            let mut g = samples.lock().unwrap();
            if g.len() < MAX_SAMPLES {
                g.push((exchange, rtt));
            }
        })
    }

    /// The call ended: fold its round trips into the reporter.
    pub(crate) fn finish(self) {
        let samples = std::mem::take(&mut *self.samples.lock().unwrap());
        self.reporter.record_rtts(self.scenario, &samples);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::report::ReporterCfg;

    /// A sample reaches the call's sink without waiting on the reporter's lock
    /// (which a report snapshot holds), and lands in the reporter once the
    /// call ends.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn a_round_trip_sample_never_waits_on_the_reporter() {
        let reporter =
            Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
        let rtts = CallRtts::new(reporter.clone(), "basic_call");
        let sink = rtts.sink();
        let (tx, rx) = mpsc::channel();
        let held = reporter.hold_lock_for_test();
        let feeder = std::thread::spawn(move || {
            sink(Exchange::Invite100, Duration::from_millis(2));
            tx.send(()).unwrap();
        });
        let fed = rx.recv_timeout(Duration::from_millis(500));
        drop(held);
        feeder.join().unwrap();
        assert!(fed.is_ok(), "the sample waited on the reporter's lock");
        rtts.finish();
        let prom = reporter.render_prometheus();
        assert!(
            prom.contains(
                "loadgen_rtt_seconds_count{scenario=\"basic_call\",exchange=\"invite_100\"} 1\n"
            ),
            "{prom}"
        );
    }
}
