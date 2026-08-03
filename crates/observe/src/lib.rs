//! Process observability foundation — ADR-0026.
//!
//! Two independent planes, deliberately not coupled:
//!
//! - **Lifecycle logs** always go to stdout as compact single-line `key=value`
//!   records through a bounded, lossy, non-blocking writer ([`writer`]). They
//!   are traffic-independent: a SIP task never blocks on a log line and an
//!   overloaded writer drops lines against a counter instead of applying back
//!   pressure.
//!   Only the lifecycle plane reaches stdout: the fmt layer filters
//!   [`TRACE_TARGET`] out per-layer, so a traced call's wire bytes never
//!   become log lines and never displace a lifecycle line in the writer queue.
//! - **Per-call traces** are exported over OTLP and exist ONLY when
//!   `OTEL_EXPORTER_OTLP_ENDPOINT` names a collector. With no endpoint the
//!   whole sampling machinery is inert: [`SampleAdmission::admit`] refuses
//!   every call against `counters::TRACE_DROPPED_NO_EXPORTER`, so no span is
//!   ever created.
//!
//! Emission sites guard themselves — `if call.sampled { … }` — so an unsampled
//! call evaluates no format arguments and allocates nothing. Subscriber-side
//! filtering is never the mechanism.
//!
//! Lifecycle logs are traffic-INDEPENDENT by construction: a per-call event
//! class is aggregated through [`WaveSet`] into a rising-edge line, a ~5 s
//! periodic summary and a falling-edge totals line, so a 5000-call failover
//! prints a handful of lines instead of 5000.
//!
//! Domain crates depend on `tracing` and the thin helpers here; the
//! OpenTelemetry dependency tree terminates in this crate.

mod admission;
mod attr;
mod call_span;
pub mod counters;
mod init;
mod node;
#[cfg(feature = "otlp")]
mod otlp;
mod plane;
mod rate_draw;
mod test_buffer;
mod token_bucket;
pub mod trace_ids;
mod wave;
mod wave_set;
mod writer;

pub use admission::{Denied, SampleAdmission, TraceLease, DEFAULT_MAX_ACTIVE};
pub use attr::{cap_bytes, cap_str, ATTR_CAP_BYTES, TRUNCATED_FIELD};
pub use call_span::{CallIdentity, CallSpan, ChildSpan, TraceEvent, BODY_CAP_BYTES};
pub use init::{init_production, ObserveGuard};
pub use node::{node, set_node_identity};
pub use plane::{is_trace_plane, TRACE_TARGET};
pub use rate_draw::RateDraw;
pub use test_buffer::{
    current_test_buffer, test_buffer, CapturedEvent, CapturedSpan, TestLogGuard, TestLogHandle,
};
pub use token_bucket::TokenBucket;
pub use wave::{
    Edge, Tally, Wave, WaveReport, DEFAULT_IDLE_CLOSE_AFTER, DEFAULT_SUMMARY_EVERY, MAX_COUNTERS,
};
pub use wave_set::{WaveSet, MAX_KEYS};

/// The env var whose presence enables OTLP span export for this process.
pub const OTLP_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// The env var that must be set for the `X-Trace-Sample` request header to be
/// honored (lab/endurance only — the header is untrusted on a serving edge).
pub const TRACE_HEADER_ENV: &str = "SIP_TRACE_HEADER";

/// The default per-call sampling rate when no header override and no engine
/// force-enable apply.
pub const DEFAULT_SAMPLE_RATE: f64 = 1e-4;

/// Set when a named endpoint yielded no exporter, so the answer below stops
/// promising an export this process cannot perform.
static EXPORT_UNUSABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record that the configured endpoint produced no tracer provider. Called by
/// [`init_production`], which runs before any gate reads the answer.
#[cfg(feature = "otlp")]
pub(crate) fn mark_export_unusable() {
    EXPORT_UNUSABLE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether this process exports traces at all: `OTEL_EXPORTER_OTLP_ENDPOINT` is
/// set to a non-empty value AND its exporter built. An endpoint that named a
/// collector but produced no provider reads the same as an unset one, so the
/// sampling machinery stays inert instead of opening root spans nothing
/// collects. Read once at startup and carried thereafter.
pub fn exporter_configured() -> bool {
    !EXPORT_UNUSABLE.load(std::sync::atomic::Ordering::Relaxed)
        && std::env::var(OTLP_ENDPOINT_ENV).map(|v| !v.trim().is_empty()).unwrap_or(false)
}

/// Whether the `X-Trace-Sample` header override is honored by this process.
pub fn trace_header_enabled() -> bool {
    std::env::var(TRACE_HEADER_ENV)
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        })
        .unwrap_or(false)
}
