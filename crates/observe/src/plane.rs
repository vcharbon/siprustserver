//! Which of the two observability planes an event belongs to (ADR-0026).
//!
//! The planes are independent: lifecycle logs are traffic-INDEPENDENT and go to
//! stdout, per-call traces are traffic-proportional and go to the collector. The
//! separation is carried by the `tracing` TARGET — every per-call span and event
//! is emitted under [`TRACE_TARGET`], nothing else is — so the stdout fmt layer
//! filters the trace plane out per-layer while the OTLP layer still receives it.
//! A shared level filter could not do this: muting the trace plane on stdout
//! would starve the exporter.

/// The `tracing` target every per-call span and event carries, and NOTHING
/// else. Stdout is the trace plane's non-subscriber.
pub const TRACE_TARGET: &str = "sip::trace";

/// Whether `target` names the per-call trace plane.
#[inline]
pub fn is_trace_plane(target: &str) -> bool {
    target == TRACE_TARGET
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_trace_target_is_the_trace_plane() {
        assert!(is_trace_plane(TRACE_TARGET));
        assert!(!is_trace_plane("b2bua::router::process"));
        assert!(!is_trace_plane("sip::trace::x"), "the target is exact, not a prefix");
    }
}
