//! The per-call root span and the facts recorded on it (ADR-0026).
//!
//! ONE root span per call per process, opened only for an admitted call and
//! closed at the call's terminal state or reap. Detail rides as span EVENTS
//! carrying `at_ms` — when the fact HAPPENED — so a backfilled activation
//! records the INVITE, the auto-100 and the decision round trip at their
//! original times rather than at activation time. Child spans exist only for
//! outbound HTTP round trips.
//!
//! Every span and event here is emitted under [`TRACE_TARGET`], the trace
//! plane's only target — that is what keeps a call's raw wire bytes off the
//! lifecycle stdout stream (ADR-0026).
//!
//! Correlation ids are minted by [`crate::trace_ids`] and carried as span
//! attributes, so a domain crate populates `Call.trace_id` / `Call.root_span_id`
//! and links a takeover span without the OpenTelemetry dependency tree. On
//! takeover the backup's own root span carries `link.trace_id` / `link.span_id`
//! naming the nominal's root — a link, never a parent: the nominal's span is
//! closed or lost by definition, so claiming it as a parent would claim a live
//! span that never closes.

use tracing::Span;

use crate::admission::TraceLease;
use crate::attr::{cap_bytes, ATTR_CAP_BYTES};
use crate::plane::TRACE_TARGET;
use crate::trace_ids::{is_valid_id, new_span_id, new_trace_id, SPAN_ID_HEX, TRACE_ID_HEX};

/// The dialog identity every span on a call carries.
#[derive(Debug, Clone, Copy)]
pub struct CallIdentity<'a> {
    /// The a-leg `Call-ID` — the ONLY cross-process correlation key (no span
    /// context is ever put on the SIP wire).
    pub call_id: &'a str,
    /// The a-leg From tag.
    pub from_tag: &'a str,
    /// The a-leg To tag, empty until the B2BUA answers.
    pub to_tag: &'a str,
}

/// One fact recorded on a span. `at_ms` is the fact's own epoch-ms timestamp,
/// which is NOT the emission time on the backfill path.
#[derive(Debug, Clone, Copy)]
pub struct TraceEvent<'a> {
    /// What kind of fact this is (`sip.in`, `rule.transition`, `http.request`…).
    pub kind: &'static str,
    /// When the fact happened, epoch ms.
    pub at_ms: i64,
    /// Short structured description — peer, rule id, outcome.
    pub detail: &'a str,
    /// The raw payload (wire bytes, JSON body), capped at 16 KiB on record.
    pub body: &'a [u8],
}

impl<'a> TraceEvent<'a> {
    /// A fact with no payload.
    pub fn new(kind: &'static str, at_ms: i64, detail: &'a str) -> Self {
        Self { kind, at_ms, detail, body: b"" }
    }

    /// The same fact carrying a payload.
    pub fn with_body(mut self, body: &'a [u8]) -> Self {
        self.body = body;
        self
    }
}

/// A call's root span plus its active-trace slot. Dropping it closes the span
/// and returns the slot to the admission gate.
pub struct CallSpan {
    span: Span,
    trace_id: String,
    span_id: String,
    _lease: TraceLease,
}

impl CallSpan {
    /// Open a root span for a newly admitted call.
    pub fn open(lease: TraceLease, id: CallIdentity<'_>) -> Self {
        Self::build(lease, id, new_trace_id(), None)
    }

    /// Open THIS process's root span for a call taken over from another node:
    /// the replicated `trace_id` is reused so both processes' spans belong to
    /// one trace, and `linked_span_id` is recorded as a link to the nominal's
    /// root. An unusable replicated id falls back to a fresh trace — a
    /// malformed correlation is worse than an unlinked one.
    pub fn linked(
        lease: TraceLease,
        id: CallIdentity<'_>,
        trace_id: &str,
        linked_span_id: Option<&str>,
    ) -> Self {
        let trace_id = if is_valid_id(trace_id, TRACE_ID_HEX) {
            trace_id.to_string()
        } else {
            new_trace_id()
        };
        let link = linked_span_id.filter(|s| is_valid_id(s, SPAN_ID_HEX));
        Self::build(lease, id, trace_id, link)
    }

    fn build(
        lease: TraceLease,
        id: CallIdentity<'_>,
        trace_id: String,
        link_span_id: Option<&str>,
    ) -> Self {
        let span_id = new_span_id();
        let span = tracing::info_span!(
            target: TRACE_TARGET,
            "sip.call",
            trace_id = %trace_id,
            span_id = %span_id,
            sip.call_id = %id.call_id,
            sip.from_tag = %id.from_tag,
            sip.to_tag = %id.to_tag,
            link.trace_id = link_span_id.map(|_| trace_id.as_str()).unwrap_or(""),
            link.span_id = link_span_id.unwrap_or(""),
        );
        Self { span, trace_id, span_id, _lease: lease }
    }

    /// The trace this call belongs to — replicated on `Call.trace_id`.
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// This process's root span id — replicated on `Call.root_span_id` and the
    /// link target a takeover backup records.
    pub fn span_id(&self) -> &str {
        &self.span_id
    }

    /// Record one fact on the root span.
    pub fn record(&self, event: TraceEvent<'_>) {
        emit(&self.span, event);
    }

    /// Open a child span for one outbound HTTP round trip. Request and response
    /// bodies are recorded on it as events; nothing else gets a child span.
    pub fn child(&self, name: &'static str) -> ChildSpan {
        ChildSpan {
            span: tracing::info_span!(
                target: TRACE_TARGET,
                parent: &self.span,
                "sip.call.http",
                trace_id = %self.trace_id,
                http.route = name,
            ),
        }
    }
}

/// One HTTP round trip's span. Dropping it closes the round trip.
pub struct ChildSpan {
    span: Span,
}

impl ChildSpan {
    /// Record one fact (a request or response body) on the round trip.
    pub fn record(&self, event: TraceEvent<'_>) {
        emit(&self.span, event);
    }
}

/// Emit one event under `span`, capping the payload and marking a capped one so
/// a prefix is never mistaken for the whole value.
fn emit(span: &Span, event: TraceEvent<'_>) {
    let (body, body_truncated) = cap_bytes(event.body);
    let (detail, detail_truncated) = crate::attr::cap_str(event.detail);
    tracing::info!(
        target: TRACE_TARGET,
        parent: span,
        kind = event.kind,
        at_ms = event.at_ms,
        detail = %detail,
        body = %String::from_utf8_lossy(body),
        truncated = body_truncated || detail_truncated,
        "trace"
    );
}

/// The payload size beyond which a recorded body is capped.
pub const BODY_CAP_BYTES: usize = ATTR_CAP_BYTES;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::SampleAdmission;
    use crate::rate_draw::RateDraw;
    use crate::test_buffer::test_buffer;
    use crate::token_bucket::TokenBucket;

    fn lease() -> TraceLease {
        SampleAdmission::new(true, 1.0, 10, RateDraw::seeded(1), TokenBucket::default_at(0))
            .admit(None, 0)
            .expect("the gate is wide open in this fixture")
    }

    fn identity() -> CallIdentity<'static> {
        CallIdentity { call_id: "c1@host", from_tag: "ft", to_tag: "" }
    }

    #[test]
    fn a_root_span_carries_usable_correlation_ids() {
        let span = CallSpan::open(lease(), identity());
        assert!(is_valid_id(span.trace_id(), TRACE_ID_HEX));
        assert!(is_valid_id(span.span_id(), SPAN_ID_HEX));
    }

    #[test]
    fn events_record_the_facts_own_timestamp() {
        let (_guard, log) = test_buffer();
        let span = CallSpan::open(lease(), identity());
        span.record(TraceEvent::new("sip.in", 1234, "alice").with_body(b"INVITE sip:bob"));

        let events = log.matching("kind=sip.in");
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("at_ms=1234"));
        assert!(events[0].contains("INVITE sip:bob"));
        assert!(events[0].contains("truncated=false"));
    }

    #[test]
    fn every_recorded_fact_rides_the_trace_plane_target() {
        let (_guard, log) = test_buffer();
        let span = CallSpan::open(lease(), identity());
        span.record(TraceEvent::new("sip.in", 1, "alice").with_body(b"INVITE sip:bob"));
        span.child("/call/new").record(TraceEvent::new("http.request", 2, "POST"));

        let events = log.snapshot();
        assert_eq!(events.len(), 2);
        assert!(
            events.iter().all(|e| crate::plane::is_trace_plane(&e.target)),
            "stdout filters the trace plane out by target: {:?}",
            events.iter().map(|e| e.target.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_oversized_body_is_capped_and_marked() {
        let (_guard, log) = test_buffer();
        let span = CallSpan::open(lease(), identity());
        let body = vec![b'x'; BODY_CAP_BYTES + 100];
        span.record(TraceEvent::new("sip.in", 0, "alice").with_body(&body));

        let events = log.matching("kind=sip.in");
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("truncated=true"));
        let recorded = &events[0].fields.iter().find(|(k, _)| k == "body").expect("body field").1;
        assert_eq!(recorded.len(), BODY_CAP_BYTES);
    }

    #[test]
    fn a_takeover_span_reuses_the_trace_and_links_the_nominal_root() {
        let nominal = CallSpan::open(lease(), identity());
        let backup = CallSpan::linked(
            lease(),
            identity(),
            nominal.trace_id(),
            Some(nominal.span_id()),
        );
        assert_eq!(backup.trace_id(), nominal.trace_id(), "one trace spans both processes");
        assert_ne!(backup.span_id(), nominal.span_id(), "the backup opens its OWN root");
    }

    #[test]
    fn an_unusable_replicated_trace_id_falls_back_to_a_fresh_trace() {
        let backup = CallSpan::linked(lease(), identity(), "not-a-trace-id", Some("nope"));
        assert!(is_valid_id(backup.trace_id(), TRACE_ID_HEX));
    }

    #[test]
    fn the_active_slot_returns_when_the_span_closes() {
        let gate =
            SampleAdmission::new(true, 1.0, 10, RateDraw::seeded(1), TokenBucket::default_at(0));
        let span = CallSpan::open(gate.admit(None, 0).expect("admitted"), identity());
        assert_eq!(gate.active(), 1);
        drop(span);
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn a_child_span_records_a_round_trips_bodies() {
        let (_guard, log) = test_buffer();
        let span = CallSpan::open(lease(), identity());
        let child = span.child("/call/new");
        child.record(TraceEvent::new("http.request", 10, "POST").with_body(b"{\"call_id\":\"c1\"}"));
        child.record(TraceEvent::new("http.response", 20, "200").with_body(b"{\"route\":{}}"));

        assert_eq!(log.matching("kind=http.request").len(), 1);
        assert_eq!(log.matching("kind=http.response").len(), 1);
    }
}
