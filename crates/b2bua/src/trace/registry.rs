//! The process's live root spans and the gate that opens them (ADR-0026).
//!
//! ONE root span per call per process, so the registry is process-scoped by
//! definition: `traces()` hands out the singleton, and a runner installs a
//! configured one at startup before any call arrives.
//!
//! The registry holds the spans OUT of the serialized `Call` — a span is a
//! runtime object, not replicated state. What the `Call` carries is the
//! correlation triple (`trace_id`, `root_span_id`, `sampled`), which is exactly
//! what a takeover backup needs to open its own linked root.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use observe::{CallIdentity, CallSpan, SampleAdmission, TraceEvent};

/// The correlation triple stamped on a `Call` when a trace activates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceIds {
    /// The trace the call belongs to (stable across takeover).
    pub trace_id: String,
    /// This process's root span for the call.
    pub root_span_id: String,
}

/// The per-process trace gate + live root spans.
pub struct CallTraces {
    admission: Arc<SampleAdmission>,
    honors_header: bool,
    spans: Mutex<HashMap<String, CallSpan>>,
}

impl CallTraces {
    /// The production shape: admission read from the environment, and the
    /// `X-Trace-Sample` override honored only when `SIP_TRACE_HEADER` says so.
    /// Both are read ONCE here so no call path pays an environment lookup.
    pub fn from_env(now_ms: i64) -> Self {
        Self::new(SampleAdmission::from_env(now_ms), observe::trace_header_enabled())
    }

    /// A fully-specified gate (tests, and any runner that configures its own).
    pub fn new(admission: Arc<SampleAdmission>, honors_header: bool) -> Self {
        Self { admission, honors_header, spans: Mutex::new(HashMap::new()) }
    }

    /// Whether this process reads `X-Trace-Sample` at all. When false the header
    /// is never looked up — the deployment gate, not a per-request check.
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
        self.spans.lock().expect("trace registry mutex").len()
    }

    /// Run the admission chain and, on success, open the call's root span.
    /// `rate_override` is the header rate when this process honors one, or
    /// `Some(1.0)` for a decision-engine force-enable. `None` for the default
    /// draw. Idempotent: a call that already has a root span keeps it —
    /// sampling is monotonic, nothing re-draws and nothing un-samples.
    pub fn activate(
        &self,
        call_ref: &str,
        id: CallIdentity<'_>,
        rate_override: Option<f64>,
        now_ms: i64,
    ) -> Option<TraceIds> {
        if let Some(ids) = self.existing(call_ref) {
            return Some(ids);
        }
        let lease = self.admission.admit(rate_override, now_ms).ok()?;
        Some(self.insert(call_ref, CallSpan::open(lease, id)))
    }

    /// Open THIS node's root span for a call hydrated from a replica: the same
    /// trace, a fresh root span LINKED to the nominal's. Runs the same admission
    /// chain — a takeover must not blow past the active cap — and yields the
    /// backup's own ids to stamp back onto the call.
    pub fn activate_linked(
        &self,
        call_ref: &str,
        id: CallIdentity<'_>,
        trace_id: &str,
        nominal_root_span_id: Option<&str>,
        now_ms: i64,
    ) -> Option<TraceIds> {
        if let Some(ids) = self.existing(call_ref) {
            return Some(ids);
        }
        let lease = self.admission.admit(Some(1.0), now_ms).ok()?;
        let span = CallSpan::linked(lease, id, trace_id, nominal_root_span_id);
        Some(self.insert(call_ref, span))
    }

    /// Record one fact on a call's root span. No-op when the call has no span
    /// (it was never sampled, or its span is already closed).
    pub fn record(&self, call_ref: &str, event: TraceEvent<'_>) {
        if let Some(span) = self.spans.lock().expect("trace registry mutex").get(call_ref) {
            span.record(event);
        }
    }

    /// Record one outbound HTTP round trip as a child span of the call's root:
    /// the request body and the response body as two events with their own
    /// timestamps. No-op for a call with no root span.
    pub fn round_trip(
        &self,
        call_ref: &str,
        route: &'static str,
        request: RoundTripSide<'_>,
        response: RoundTripSide<'_>,
    ) {
        let spans = self.spans.lock().expect("trace registry mutex");
        let Some(root) = spans.get(call_ref) else {
            return;
        };
        let child = root.child(route);
        child.record(
            TraceEvent::new("http.request", request.at_ms, request.detail).with_body(request.body),
        );
        child.record(
            TraceEvent::new("http.response", response.at_ms, response.detail)
                .with_body(response.body),
        );
    }

    /// Close a call's root span, returning its active-trace slot to the gate.
    /// Idempotent — every teardown path may call it.
    pub fn close(&self, call_ref: &str) {
        self.spans.lock().expect("trace registry mutex").remove(call_ref);
    }

    fn existing(&self, call_ref: &str) -> Option<TraceIds> {
        self.spans.lock().expect("trace registry mutex").get(call_ref).map(|s| TraceIds {
            trace_id: s.trace_id().to_string(),
            root_span_id: s.span_id().to_string(),
        })
    }

    fn insert(&self, call_ref: &str, span: CallSpan) -> TraceIds {
        let ids = TraceIds {
            trace_id: span.trace_id().to_string(),
            root_span_id: span.span_id().to_string(),
        };
        self.spans.lock().expect("trace registry mutex").insert(call_ref.to_string(), span);
        ids
    }
}

/// One side of an HTTP round trip as it is recorded.
#[derive(Debug, Clone, Copy)]
pub struct RoundTripSide<'a> {
    /// When this side happened, epoch ms.
    pub at_ms: i64,
    /// Method + route, or the outcome.
    pub detail: &'a str,
    /// The JSON body.
    pub body: &'a [u8],
}

/// The dialog identity a call's spans carry. The a-leg `Call-ID` is the only
/// cross-process correlation key — no span context ever rides the SIP wire.
pub fn call_identity(call: &call::Call) -> CallIdentity<'_> {
    CallIdentity {
        call_id: &call.a_leg.call_id,
        from_tag: &call.a_leg.from_tag,
        to_tag: call.a_leg.dialogs.first().map(|d| d.sip.local_tag.as_str()).unwrap_or(""),
    }
}

/// Open THIS node's root span for a call whose state was just hydrated from a
/// replica (takeover, reclaim, restore), and re-stamp the call with this node's
/// own root span id so a further takeover links to the span that actually
/// served the call. No-op for an unsampled call.
///
/// A refusal (the active cap, the bucket) leaves `sampled` alone: sampling is
/// monotonic and the trace exists at the node that opened it, so this node
/// simply records nothing for the call — and counts it
/// (`trace_adoption_refused_total`), because a mass takeover refuses a whole
/// population at once and an uncounted gap looks like a call that was never
/// traced.
pub fn adopt_replicated(call: &mut call::Call, now_ms: i64) {
    adopt_into(&traces(), call, now_ms);
}

/// [`adopt_replicated`] against an explicit registry — the seam the hydration
/// tests drive without touching the process singleton.
pub fn adopt_into(traces: &CallTraces, call: &mut call::Call, now_ms: i64) {
    if call.sampled != Some(true) {
        return;
    }
    let Some(trace_id) = call.trace_id.clone() else {
        return;
    };
    let call_ref = call.call_ref.clone();
    let nominal_root = call.root_span_id.clone();
    let Some(ids) = traces.activate_linked(
        &call_ref,
        call_identity(call),
        &trace_id,
        nominal_root.as_deref(),
        now_ms,
    ) else {
        observe::counters::bump(&observe::counters::TRACE_ADOPTION_REFUSED);
        return;
    };
    call.trace_id = Some(ids.trace_id);
    call.root_span_id = Some(ids.root_span_id);
}

static PROCESS_TRACES: OnceLock<RwLock<Arc<CallTraces>>> = OnceLock::new();

fn cell() -> &'static RwLock<Arc<CallTraces>> {
    PROCESS_TRACES.get_or_init(|| RwLock::new(Arc::new(CallTraces::from_env(0))))
}

/// The process's trace registry. Inert (no exporter → no draw, no span) unless
/// the process was started with an OTLP endpoint.
pub fn traces() -> Arc<CallTraces> {
    cell().read().expect("process trace registry lock").clone()
}

/// Replace the process's trace registry. A runner calls this once at startup,
/// before any call arrives; a test that drives the machinery installs its own
/// gate. Process-wide by construction — there is one root span per call per
/// process, so there is one registry.
pub fn install_process_traces(registry: Arc<CallTraces>) {
    *cell().write().expect("process trace registry lock") = registry;
}

#[cfg(test)]
mod tests {
    use super::*;
    use observe::{RateDraw, TokenBucket};

    fn gate(exporter: bool, rate: f64, max_active: usize) -> CallTraces {
        CallTraces::new(
            SampleAdmission::new(
                exporter,
                rate,
                max_active,
                RateDraw::seeded(7),
                TokenBucket::default_at(0),
            ),
            false,
        )
    }

    fn id() -> CallIdentity<'static> {
        CallIdentity { call_id: "c@h", from_tag: "ft", to_tag: "" }
    }

    #[test]
    fn without_an_exporter_no_span_is_ever_opened() {
        let traces = gate(false, 1.0, 100);
        assert!(traces.activate("w0|c|ft", id(), None, 0).is_none());
        assert_eq!(traces.active(), 0);
    }

    #[test]
    fn activation_is_idempotent_and_monotonic() {
        let traces = gate(true, 1.0, 100);
        let first = traces.activate("w0|c|ft", id(), None, 0).expect("admitted");
        // A second activation attempt — even one that would fail the draw —
        // keeps the call's existing span and ids.
        let again = traces.activate("w0|c|ft", id(), Some(0.0), 0).expect("still sampled");
        assert_eq!(first, again);
        assert_eq!(traces.active(), 1);
    }

    #[test]
    fn closing_frees_the_span_and_its_active_slot() {
        let traces = gate(true, 1.0, 2);
        traces.activate("a", id(), None, 0).expect("admitted");
        traces.activate("b", id(), None, 1000).expect("admitted");
        assert!(traces.activate("c", id(), None, 2000).is_none(), "the cap holds");
        traces.close("a");
        traces.close("a");
        assert_eq!(traces.active(), 1);
        assert!(traces.activate("c", id(), None, 3000).is_some(), "a closed span frees a slot");
    }

    #[test]
    fn a_takeover_span_reuses_the_trace_and_gets_its_own_root() {
        let nominal = gate(true, 1.0, 100);
        let nominal_ids = nominal.activate("w0|c|ft", id(), None, 0).expect("admitted");

        let backup = gate(true, 1.0, 100);
        let backup_ids = backup
            .activate_linked(
                "w0|c|ft",
                id(),
                &nominal_ids.trace_id,
                Some(&nominal_ids.root_span_id),
                0,
            )
            .expect("a takeover is admitted");
        assert_eq!(backup_ids.trace_id, nominal_ids.trace_id);
        assert_ne!(backup_ids.root_span_id, nominal_ids.root_span_id);
    }

    /// A minimal replicated call carrying the correlation triple a takeover
    /// hydration reads.
    fn replicated_call(trace_id: &str, root_span_id: &str, sampled: Option<bool>) -> call::Call {
        use sip_message::parser::custom::CustomParser;
        use sip_message::{SipMessage, SipParser};
        let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-adopt\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: adopt@10.0.0.9\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@10.0.0.9:5060>\r\n\
             Content-Length: 0\r\n\r\n";
        let invite = match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        };
        let mut c = crate::initial_invite::build_initial_call(
            &invite,
            std::net::SocketAddr::from(([10, 0, 0, 9], 5060)),
            &crate::config::B2buaConfig::default(),
            0,
        );
        c.trace_id = Some(trace_id.to_string());
        c.root_span_id = Some(root_span_id.to_string());
        c.sampled = sampled;
        c
    }

    #[test]
    fn a_hydrated_traced_call_opens_this_nodes_own_linked_root() {
        let nominal = gate(true, 1.0, 100);
        let nominal_ids = nominal.activate("ref", id(), None, 0).expect("admitted");

        let backup = gate(true, 1.0, 100);
        let mut call =
            replicated_call(&nominal_ids.trace_id, &nominal_ids.root_span_id, Some(true));
        let call_ref = call.call_ref.clone();
        adopt_into(&backup, &mut call, 0);

        assert_eq!(call.trace_id.as_deref(), Some(nominal_ids.trace_id.as_str()));
        assert_ne!(
            call.root_span_id.as_deref(),
            Some(nominal_ids.root_span_id.as_str()),
            "the backup serves the call under its OWN root span"
        );
        assert_eq!(backup.active(), 1);
        backup.close(&call_ref);
    }

    #[test]
    fn hydrating_an_unsampled_call_opens_nothing() {
        let backup = gate(true, 1.0, 100);
        let mut call = replicated_call(&"a".repeat(32), &"b".repeat(16), None);
        adopt_into(&backup, &mut call, 0);
        assert_eq!(backup.active(), 0);
        assert_eq!(call.root_span_id.as_deref(), Some("b".repeat(16).as_str()));
    }

    #[test]
    fn a_refused_adoption_is_counted() {
        // The cap is full, so the hydrated call gets no span here. It stays
        // sampled (monotonic) and keeps the nominal's root id — the only
        // evidence this node dropped its half of the story is the counter.
        let backup = gate(true, 1.0, 1);
        backup.activate("squatter", id(), None, 0).expect("the one slot");

        let before = observe::counters::get(&observe::counters::TRACE_ADOPTION_REFUSED);
        let mut call = replicated_call(&"a".repeat(32), &"b".repeat(16), Some(true));
        adopt_into(&backup, &mut call, 0);

        assert_eq!(backup.active(), 1, "the cap held");
        assert_eq!(call.root_span_id.as_deref(), Some("b".repeat(16).as_str()));
        assert_eq!(
            observe::counters::get(&observe::counters::TRACE_ADOPTION_REFUSED),
            before + 1,
            "a mass takeover's lost traces must not be invisible"
        );
    }

    #[test]
    fn recording_on_an_unsampled_call_is_a_no_op() {
        let (_guard, log) = observe::test_buffer();
        let traces = gate(true, 1.0, 100);
        traces.record("never-activated", TraceEvent::new("sip.in", 0, "alice"));
        assert!(log.lines().is_empty());
    }
}
