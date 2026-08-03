//! The proxy's live root spans and the gate that opens them (ADR-0026).
//!
//! One root span per call per process, keyed by `Call-ID` — the only
//! correlation key, since the proxy puts no span context on the wire. The map
//! is bounded by the admission chain's active-trace cap: every entry holds the
//! lease it was admitted with, so the cap counts open spans exactly.
//!
//! A span closes when the proxy observes the call's BYE final, or when
//! [`SPAN_IDLE_TTL_MS`] passes with no datagram naming it — the ceiling on a
//! span whose teardown this hop never sees (a caller that vanishes, a BYE that
//! takes another path).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use observe::{CallIdentity, CallSpan, SampleAdmission};

/// How long a root span survives with no datagram naming its call.
pub const SPAN_IDLE_TTL_MS: i64 = 900_000;

/// One traced call: its root span and the instant it goes stale.
struct Entry {
    span: CallSpan,
    expires_at_ms: i64,
}

/// The proxy's trace gate + live root spans.
pub struct ProxyTraces {
    admission: Arc<SampleAdmission>,
    honors_header: bool,
    idle_ttl_ms: i64,
    /// Is ANY call sampled right now? The per-packet path's only cost when the
    /// answer is no.
    any: AtomicBool,
    spans: Mutex<HashMap<String, Entry>>,
}

impl ProxyTraces {
    /// The production shape: admission read from the environment, and the
    /// `X-Trace-Sample` override honored only when `SIP_TRACE_HEADER` says so.
    /// Both are read ONCE here, so no packet path pays an environment lookup.
    pub fn from_env(now_ms: i64) -> Self {
        Self::new(SampleAdmission::from_env(now_ms), observe::trace_header_enabled())
    }

    /// A fully-specified gate (tests, and any runner that configures its own).
    pub fn new(admission: Arc<SampleAdmission>, honors_header: bool) -> Self {
        Self {
            admission,
            honors_header,
            idle_ttl_ms: SPAN_IDLE_TTL_MS,
            any: AtomicBool::new(false),
            spans: Mutex::new(HashMap::new()),
        }
    }

    /// Whether this process reads `X-Trace-Sample` at all — the deployment
    /// gate, not a per-request check.
    pub fn honors_header(&self) -> bool {
        self.honors_header
    }

    /// Whether this process exports traces at all. False makes every activation
    /// attempt inert against `trace_dropped_no_exporter_total`.
    pub fn exporter_configured(&self) -> bool {
        self.admission.exporter_configured()
    }

    /// Root spans currently open.
    pub fn active(&self) -> usize {
        self.spans.lock().expect("proxy trace registry mutex").len()
    }

    /// Is any call sampled? One relaxed load — the whole cost of the trace tier
    /// on a proxy that is not tracing anything.
    #[inline]
    pub fn any_sampled(&self) -> bool {
        self.any.load(Ordering::Relaxed)
    }

    /// Run the admission chain for an initial INVITE and, on success, open the
    /// call's root span. `rate_override` is the `X-Trace-Sample` rate when this
    /// process honors one, `None` for the configured rate. Idempotent: a call
    /// that already has a span keeps it — sampling is monotonic.
    pub fn activate(
        &self,
        call_id: &str,
        id: CallIdentity<'_>,
        rate_override: Option<f64>,
        now_ms: i64,
    ) -> bool {
        // Inert without a collector: no draw, no lock, no span — the whole
        // machinery costs one boolean read per new call.
        if !self.exporter_configured() {
            observe::counters::bump(&observe::counters::TRACE_DROPPED_NO_EXPORTER);
            return false;
        }
        // The rare path (an admitted INVITE) is where the stale entries of
        // calls whose teardown was never observed are reclaimed.
        self.expire(now_ms);
        let mut spans = self.spans.lock().expect("proxy trace registry mutex");
        if spans.contains_key(call_id) {
            return true;
        }
        let Ok(lease) = self.admission.admit(rate_override, now_ms) else {
            return false;
        };
        let entry = Entry { span: CallSpan::open(lease, id), expires_at_ms: now_ms + self.idle_ttl_ms };
        spans.insert(call_id.to_string(), entry);
        self.any.store(true, Ordering::Relaxed);
        true
    }

    /// Record on a traced call's root span, refreshing its idle deadline, and
    /// report whether the call IS traced. The guard every emission site runs
    /// through: one relaxed load when nothing is sampled, one map miss for an
    /// unsampled call while others are traced. `f` is reached only for a call
    /// that IS traced, so a detail string is formatted only when it will be
    /// recorded. The returned flag is the same answer without a second lookup,
    /// for a caller that must know before it spends anything on the call.
    #[inline]
    pub fn with_span(&self, call_id: &str, at_ms: i64, f: impl FnOnce(&CallSpan)) -> bool {
        if !self.any_sampled() {
            return false;
        }
        let mut spans = self.spans.lock().expect("proxy trace registry mutex");
        let Some(entry) = spans.get_mut(call_id) else {
            return false;
        };
        entry.expires_at_ms = at_ms + self.idle_ttl_ms;
        f(&entry.span);
        true
    }

    /// Close a call's root span, returning its active-trace slot. Idempotent —
    /// every teardown path may call it.
    pub fn close(&self, call_id: &str) {
        if !self.any_sampled() {
            return;
        }
        let mut spans = self.spans.lock().expect("proxy trace registry mutex");
        spans.remove(call_id);
        self.any.store(!spans.is_empty(), Ordering::Relaxed);
    }

    /// Drop every span whose call went quiet for [`SPAN_IDLE_TTL_MS`].
    fn expire(&self, now_ms: i64) {
        if !self.any_sampled() {
            return;
        }
        let mut spans = self.spans.lock().expect("proxy trace registry mutex");
        spans.retain(|_, e| e.expires_at_ms > now_ms);
        self.any.store(!spans.is_empty(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use observe::{RateDraw, TokenBucket, TraceEvent};

    fn gate(exporter: bool, max_active: usize) -> ProxyTraces {
        ProxyTraces::new(
            SampleAdmission::new(exporter, 1.0, max_active, RateDraw::seeded(3), TokenBucket::default_at(0)),
            false,
        )
    }

    fn id() -> CallIdentity<'static> {
        CallIdentity { call_id: "c@h", from_tag: "ft", to_tag: "" }
    }

    #[test]
    fn without_an_exporter_no_span_is_ever_opened() {
        let before = observe::counters::get(&observe::counters::TRACE_DROPPED_NO_EXPORTER);
        let traces = gate(false, 100);
        assert!(!traces.activate("c@h", id(), None, 0));
        assert_eq!(traces.active(), 0);
        assert!(!traces.any_sampled(), "the per-packet flag stays down");
        assert!(
            observe::counters::get(&observe::counters::TRACE_DROPPED_NO_EXPORTER) > before,
            "the refusal is counted, never logged",
        );
    }

    #[test]
    fn activation_is_idempotent_and_raises_the_sampled_flag() {
        let traces = gate(true, 100);
        assert!(traces.activate("c@h", id(), None, 0));
        assert!(traces.any_sampled());
        assert!(traces.activate("c@h", id(), Some(0.0), 0), "a traced call stays traced");
        assert_eq!(traces.active(), 1);
    }

    #[test]
    fn closing_frees_the_span_its_slot_and_the_flag() {
        let traces = gate(true, 1);
        assert!(traces.activate("a@h", id(), None, 0));
        assert!(!traces.activate("b@h", id(), None, 1000), "the active cap holds");
        traces.close("a@h");
        traces.close("a@h");
        assert_eq!(traces.active(), 0);
        assert!(!traces.any_sampled());
        assert!(traces.activate("b@h", id(), None, 2000), "a closed span frees a slot");
    }

    #[test]
    fn a_call_that_goes_quiet_expires_at_the_idle_ttl() {
        let traces = gate(true, 100);
        traces.activate("a@h", id(), None, 0);
        // Traffic at the deadline keeps the call alive; the next activation
        // sweeps only what has actually gone quiet.
        traces.with_span("a@h", SPAN_IDLE_TTL_MS, |_| {});
        traces.activate("b@h", id(), None, SPAN_IDLE_TTL_MS + 1);
        assert_eq!(traces.active(), 2, "a call still sending is not stale");
        traces.activate("c@h", id(), None, 2 * SPAN_IDLE_TTL_MS + 2);
        assert_eq!(traces.active(), 1, "both quiet calls were reclaimed");
        assert!(traces.any_sampled(), "the freshly activated call keeps the flag up");
    }

    #[test]
    fn recording_on_an_untraced_call_is_a_no_op() {
        let (_guard, log) = observe::test_buffer();
        let traces = gate(true, 100);
        traces.activate("a@h", id(), None, 0);
        assert!(
            !traces.with_span("other@h", 1, |span| span.record(TraceEvent::new("sip.in", 1, "x"))),
            "an untraced call reports untraced even while another call is traced",
        );
        assert!(log.lines().is_empty(), "no span names that call");
        assert!(traces.with_span("a@h", 2, |span| span.record(TraceEvent::new("sip.in", 2, "x"))));
        assert_eq!(log.matching("kind=sip.in").len(), 1);
    }
}
