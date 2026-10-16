//! The reporter's live `/metrics` text: every series under the run's
//! `stream`, the per-call ones by `shape`.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use super::Reporter;
use crate::catalogue::DEFAULT_STREAM;
use crate::hist::Hist;
use crate::scenarios::ScenarioId;

impl Reporter {
    /// Render the live `/metrics` text in Prometheus exposition format. Series
    /// mirror the SIPp exporter's naming so existing dashboards/queries extend.
    pub fn render_prometheus(&self) -> String {
        use crate::catalogue as c;
        let g = self.inner.lock().unwrap();
        let st = g.stream.as_deref().unwrap_or(DEFAULT_STREAM);
        let mut out = String::new();
        // The case dimension is a report/sample refinement only — the Prometheus
        // series stay (shape, class)-keyed (stable dashboards, bounded series),
        // so aggregate over case here.
        let mut calls: BTreeMap<(ScenarioId, String), u64> = BTreeMap::new();
        for shape in &g.declared {
            for class in c::CLASS.values {
                calls.insert((shape, class.to_string()), 0);
            }
        }
        for ((shape, class, _case), n) in &g.counts {
            *calls.entry((*shape, class.clone())).or_default() += n;
        }
        c::CALLS.render_rows(
            &mut out,
            calls.into_iter().map(|((s, class), n)| ([st.to_string(), s.to_string(), class], n)),
        );
        c::BRANCHES.render_rows(
            &mut out,
            g.branches.iter().map(|((s, branch), n)| ([st, *s, *branch], *n)),
        );
        c::FAILED_STEPS.render_rows(
            &mut out,
            g.failed_steps
                .iter()
                .map(|((s, at), n)| ([st.to_string(), s.to_string(), at.to_string()], *n)),
        );
        let mut shed: BTreeMap<ScenarioId, u64> = g.declared.iter().map(|s| (*s, 0)).collect();
        shed.extend(g.shed.iter().map(|(s, n)| (*s, *n)));
        c::SHED.render_rows(&mut out, shed.into_iter().map(|(s, n)| ([st, s], n)));
        let one = |out: &mut String, family: &metric_catalogue::Family, n: u64| {
            family.render_rows(out, [([st], n)]);
        };
        one(&mut out, &c::INFLIGHT, self.inflight.load(Ordering::Relaxed));
        one(&mut out, &c::STARTED, self.started.load(Ordering::Relaxed));
        // 18x-delivery gate: a non-PRACK ringing provisional is best-effort, so a
        // miss is EXPECTED. `received/expected` should stay > 0.99; a systemic 18x
        // regression drops it well below and IS a bug (unlike one dropped 18x).
        one(&mut out, &c::RINGING_EXPECTED, self.ringing_expected.load(Ordering::Relaxed));
        one(&mut out, &c::RINGING_RECEIVED, self.ringing_received.load(Ordering::Relaxed));
        let empty = Hist::call();
        let mut e2e: BTreeMap<ScenarioId, &Hist> =
            g.declared.iter().map(|s| (*s, &empty)).collect();
        e2e.extend(g.e2e.iter().map(|(s, h)| (*s, h)));
        c::E2E_LATENCY_SECONDS.render_histogram_rows(
            &mut out,
            e2e.iter().map(|(s, h)| ([st, *s], h.histogram_value())),
        );
        c::CHECKPOINT_LATENCY_SECONDS.render_histogram_rows(
            &mut out,
            g.checkpoints.iter().map(|((s, name), h)| ([st, *s, *name], h.histogram_value())),
        );
        c::RTT_SECONDS.render_histogram_rows(
            &mut out,
            g.rtt
                .iter()
                .map(|((s, exchange), h)| ([st, *s, exchange.label()], h.histogram_value())),
        );
        out
    }

    /// The load stream every series names.
    pub fn stream(&self) -> String {
        self.inner.lock().unwrap().stream.clone().unwrap_or_else(|| DEFAULT_STREAM.to_string())
    }
}
