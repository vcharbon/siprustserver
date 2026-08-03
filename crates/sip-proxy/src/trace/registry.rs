//! The proxy's live root spans and the gate that opens them (ADR-0026).
//!
//! One root span per call per process, keyed by `Call-ID` — the only
//! correlation key, since the proxy puts no span context on the wire. The map
//! is bounded by the admission chain's active-trace cap: every entry holds the
//! lease it was admitted with, so the cap counts open spans exactly.
//!
//! A span closes when the proxy observes the call's last fact on this hop — a
//! BYE final, or the ACK relayed on a remembered non-2xx INVITE final — or when
//! [`SPAN_IDLE_TTL_MS`] passes with no datagram naming it, the ceiling on a span
//! whose teardown this hop never sees (a caller that vanishes, a BYE that takes
//! another path).
//!
//! **One registry serves every SO_REUSEPORT shard**, so its lock is on the path
//! of every shard's every datagram once anything is sampled. It is therefore an
//! `RwLock`: recording — which is what a chatty traced call spends its time on —
//! runs under the READ lock, and the per-shard map check of an untraced call
//! runs in parallel with it. Only the three rare mutations (open, close, sweep)
//! take the write lock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, RwLock};

use observe::{CallIdentity, CallSpan, SampleAdmission};

/// How long a root span survives with no datagram naming its call.
pub const SPAN_IDLE_TTL_MS: i64 = 900_000;

/// Sweeps per idle TTL. A stale span is therefore reclaimed within
/// `idle_ttl / SWEEP_DIVISOR` of going stale, and the sweep's cost is amortised
/// over that window instead of being paid per admitted INVITE.
const SWEEP_DIVISOR: i64 = 16;

/// What an activation attempt did to a call's root span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// This attempt opened the span. Its caller owns the call's first record:
    /// nothing can have recorded a datagram for a call that had no span.
    Opened,
    /// The call was already traced and keeps the span it has — sampling is
    /// monotonic. Its datagrams are already recorded by the per-packet seam, so
    /// an opener's backfill here would record them TWICE.
    AlreadyOpen,
    /// The admission chain refused, or this process exports nothing.
    Refused,
}

impl Activation {
    /// Whether the call holds a root span once this attempt is done.
    pub fn is_traced(self) -> bool {
        !matches!(self, Activation::Refused)
    }
}

/// One traced call: its root span and the instant it goes stale. The deadline
/// is atomic so refreshing it — the per-datagram write — needs only the read
/// lock.
struct Entry {
    span: CallSpan,
    expires_at_ms: AtomicI64,
}

/// The proxy's trace gate + live root spans.
pub struct ProxyTraces {
    admission: Arc<SampleAdmission>,
    honors_header: bool,
    idle_ttl_ms: i64,
    /// The earliest time a stale-span sweep may run again.
    next_sweep_at_ms: AtomicI64,
    /// Is ANY call sampled right now? The per-packet path's only cost when the
    /// answer is no.
    any: AtomicBool,
    spans: RwLock<HashMap<String, Entry>>,
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
            next_sweep_at_ms: AtomicI64::new(i64::MIN),
            any: AtomicBool::new(false),
            spans: RwLock::new(HashMap::new()),
        }
    }

    /// Override the idle TTL (and with it the sweep cadence, which is derived
    /// from it). Production keeps [`SPAN_IDLE_TTL_MS`].
    pub fn with_idle_ttl(mut self, idle_ttl_ms: i64) -> Self {
        self.idle_ttl_ms = idle_ttl_ms;
        self
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
        self.spans.read().expect("proxy trace registry lock").len()
    }

    /// Is any call sampled? One relaxed load — the whole cost of the trace tier
    /// on a proxy that is not tracing anything.
    #[inline]
    pub fn any_sampled(&self) -> bool {
        self.any.load(Ordering::Relaxed)
    }

    /// Run the admission chain for an initial INVITE and, on success, open the
    /// call's root span. `rate_override` is the `X-Trace-Sample` rate when this
    /// process honors one, `None` for the configured rate.
    ///
    /// The returned [`Activation`] tells an opener apart from a call that was
    /// ALREADY traced — a digest-auth retry, or a retransmit whose memo has
    /// been evicted, reaches this seam a second time. Sampling is monotonic, so
    /// such a call keeps its span; the distinction exists because only an
    /// opener may record the datagram that opened it (the per-packet seam
    /// already recorded it for every other case).
    pub fn activate(
        &self,
        call_id: &str,
        id: CallIdentity<'_>,
        rate_override: Option<f64>,
        now_ms: i64,
    ) -> Activation {
        // Inert without a collector: no draw, no lock, no span — the whole
        // machinery costs one boolean read per new call.
        if !self.exporter_configured() {
            observe::counters::bump(&observe::counters::TRACE_DROPPED_NO_EXPORTER);
            return Activation::Refused;
        }
        // Stale entries are reclaimed BEFORE the draw, so a span whose teardown
        // this hop never saw frees its cap slot for the call now asking for one.
        self.sweep_if_due(now_ms);
        let mut spans = self.spans.write().expect("proxy trace registry lock");
        if spans.contains_key(call_id) {
            return Activation::AlreadyOpen;
        }
        let Ok(lease) = self.admission.admit(rate_override, now_ms) else {
            return Activation::Refused;
        };
        let entry = Entry {
            span: CallSpan::open(lease, id),
            expires_at_ms: AtomicI64::new(now_ms + self.idle_ttl_ms),
        };
        spans.insert(call_id.to_string(), entry);
        self.any.store(true, Ordering::Relaxed);
        Activation::Opened
    }

    /// Record on a traced call's root span, refreshing its idle deadline, and
    /// report whether the call IS traced. The guard every emission site runs
    /// through: one relaxed load when nothing is sampled, one map miss for an
    /// unsampled call while others are traced. `f` is reached only for a call
    /// that IS traced, so a detail string is formatted only when it will be
    /// recorded. The returned flag is the same answer without a second lookup,
    /// for a caller that must know before it spends anything on the call.
    ///
    /// Everything here — the lookup, the deadline refresh, and `f`'s own
    /// recording, which `tracing` performs synchronously — runs under the READ
    /// lock, so one chatty traced call never queues the other shards behind it.
    #[inline]
    pub fn with_span(&self, call_id: &str, at_ms: i64, f: impl FnOnce(&CallSpan)) -> bool {
        if !self.any_sampled() {
            return false;
        }
        let spans = self.spans.read().expect("proxy trace registry lock");
        let Some(entry) = spans.get(call_id) else {
            return false;
        };
        entry.expires_at_ms.store(at_ms + self.idle_ttl_ms, Ordering::Relaxed);
        f(&entry.span);
        true
    }

    /// Close a call's root span, returning its active-trace slot. Idempotent —
    /// every teardown path may call it.
    pub fn close(&self, call_id: &str) {
        if !self.any_sampled() {
            return;
        }
        let mut spans = self.spans.write().expect("proxy trace registry lock");
        spans.remove(call_id);
        self.any.store(!spans.is_empty(), Ordering::Relaxed);
    }

    /// Sweep stale spans, at most once per `idle_ttl / SWEEP_DIVISOR`.
    ///
    /// The sweep is a full scan under the write lock and its caller is every
    /// admitted INVITE — call rate, not span rate — so running it unconditionally
    /// would put a whole-map comparison per new call in front of every shard.
    /// Off-schedule this is one relaxed load. The cost of the schedule is that a
    /// stale span holds its cap slot for up to one sweep interval past its TTL.
    fn sweep_if_due(&self, now_ms: i64) {
        if !self.any_sampled() || now_ms < self.next_sweep_at_ms.load(Ordering::Relaxed) {
            return;
        }
        self.next_sweep_at_ms
            .store(now_ms + (self.idle_ttl_ms / SWEEP_DIVISOR).max(1), Ordering::Relaxed);
        let mut spans = self.spans.write().expect("proxy trace registry lock");
        spans.retain(|_, e| e.expires_at_ms.load(Ordering::Relaxed) > now_ms);
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
        assert_eq!(traces.activate("c@h", id(), None, 0), Activation::Refused);
        assert_eq!(traces.active(), 0);
        assert!(!traces.any_sampled(), "the per-packet flag stays down");
        assert!(
            observe::counters::get(&observe::counters::TRACE_DROPPED_NO_EXPORTER) > before,
            "the refusal is counted, never logged",
        );
    }

    #[test]
    fn a_second_activation_reports_the_span_it_did_not_open() {
        let traces = gate(true, 100);
        assert_eq!(traces.activate("c@h", id(), None, 0), Activation::Opened);
        assert!(traces.any_sampled());
        // Sampling is monotonic: the call keeps its span even when the second
        // attempt would draw at 0. But the caller must not treat this as an
        // open — the datagram is already recorded by the per-packet seam.
        assert_eq!(
            traces.activate("c@h", id(), Some(0.0), 0),
            Activation::AlreadyOpen,
            "a traced call stays traced, and says so distinguishably",
        );
        assert_eq!(traces.active(), 1);
    }

    #[test]
    fn closing_frees_the_span_its_slot_and_the_flag() {
        let traces = gate(true, 1);
        assert_eq!(traces.activate("a@h", id(), None, 0), Activation::Opened);
        assert_eq!(traces.activate("b@h", id(), None, 1000), Activation::Refused, "the active cap holds");
        traces.close("a@h");
        traces.close("a@h");
        assert_eq!(traces.active(), 0);
        assert!(!traces.any_sampled());
        assert_eq!(traces.activate("b@h", id(), None, 2000), Activation::Opened, "a closed span frees a slot");
    }

    #[test]
    fn a_call_that_goes_quiet_expires_at_the_idle_ttl() {
        let traces = gate(true, 100);
        traces.activate("a@h", id(), None, 0);
        // Traffic at the deadline keeps the call alive; the sweep reclaims only
        // what has actually gone quiet.
        traces.with_span("a@h", SPAN_IDLE_TTL_MS, |_| {});
        traces.activate("b@h", id(), None, SPAN_IDLE_TTL_MS + 1);
        assert_eq!(traces.active(), 2, "a call still sending is not stale");
        traces.activate("c@h", id(), None, 2 * SPAN_IDLE_TTL_MS + 2);
        assert_eq!(traces.active(), 1, "both quiet calls were reclaimed");
        assert!(traces.any_sampled(), "the freshly activated call keeps the flag up");
    }

    // The sweep costs a full scan under the write lock and its caller is every
    // admitted INVITE, so it is on a schedule: at most one per idle_ttl/16. A
    // stale span therefore lingers until the next slot comes due.
    #[test]
    fn the_stale_span_sweep_runs_on_a_schedule_not_per_activation() {
        const TTL: i64 = 1600;
        let traces = gate(true, 100).with_idle_ttl(TTL);
        // Nothing is sampled yet, so this one takes no sweep at all.
        traces.activate("a@h", id(), None, 0);
        // First sweep: `a` is not stale yet (it expires at TTL). The next slot
        // is now TTL - 50 + 100.
        traces.activate("b@h", id(), None, TTL - 50);
        assert_eq!(traces.active(), 2);

        // `a` went stale at TTL, but the schedule's next slot is TTL + 50.
        traces.activate("c@h", id(), None, TTL + 1);
        assert_eq!(traces.active(), 3, "a stale span lingers until the sweep is due");

        traces.activate("d@h", id(), None, TTL + 50);
        assert_eq!(traces.active(), 3, "the due sweep reclaimed `a`, and `d` took its place");
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
