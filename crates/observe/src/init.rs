//! Production subscriber installation.
//!
//! One call at the top of a runner's `main` installs the process subscriber and
//! returns the guard that must outlive every logging site. The guard drains the
//! log writer and flushes the tracer provider on drop, so a SIGTERM-drained
//! runner loses neither its last lines nor its in-flight spans.

use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "otlp")]
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::filter::{filter_fn, FilterFn};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry};

use crate::plane::is_trace_plane;
use crate::writer;

/// Installed once per process; a second call is inert rather than a panic.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Levels emitted when `RUST_LOG` says nothing. Lifecycle logging is `info`.
const DEFAULT_FILTER: &str = "info";

/// Holds the process subscriber's resources. Drop flushes and shuts down.
pub struct ObserveGuard {
    writer: Option<writer::LogWriterGuard>,
    #[cfg(feature = "otlp")]
    provider: Option<SdkTracerProvider>,
}

impl ObserveGuard {
    /// The inert guard: nothing was installed, nothing to flush.
    fn inert() -> Self {
        Self {
            writer: None,
            #[cfg(feature = "otlp")]
            provider: None,
        }
    }

    /// Whether this process exports spans over OTLP.
    pub fn exports_traces(&self) -> bool {
        #[cfg(feature = "otlp")]
        {
            self.provider.is_some()
        }
        #[cfg(not(feature = "otlp"))]
        {
            false
        }
    }
}

impl Drop for ObserveGuard {
    fn drop(&mut self) {
        // Spans first: their export may itself log.
        #[cfg(feature = "otlp")]
        if let Some(provider) = self.provider.take() {
            let _ = provider.force_flush();
            let _ = provider.shutdown();
        }
        drop(self.writer.take());
    }
}

/// The stdout plane's per-layer filter: everything EXCEPT the per-call trace
/// plane. Per-layer, so the OTLP layer still receives what stdout refuses —
/// a shared level/target filter would starve the exporter along with stdout.
fn lifecycle_only() -> FilterFn<fn(&tracing::Metadata<'_>) -> bool> {
    filter_fn(|meta: &tracing::Metadata<'_>| !is_trace_plane(meta.target()))
}

/// Install the process subscriber for `service_name` and return its guard.
///
/// Layers, in order:
/// 1. a compact single-line `key=value` fmt layer over the bounded lossy stdout
///    writer — lifecycle logs, always on, never back-pressuring, and carrying
///    the lifecycle plane ONLY: per-call trace spans and events are filtered
///    out per-layer, so traced traffic never turns stdout per-call;
/// 2. the OTLP span layer, present only when this build carries the `otlp`
///    feature (the runners) AND `OTEL_EXPORTER_OTLP_ENDPOINT` is set (see
///    `otlp::provider_from_env`). A domain crate depending on `observe` for the
///    lifecycle helpers therefore never compiles the export tree.
///
/// Hold the returned guard until the process exits.
#[cfg_attr(not(feature = "otlp"), allow(unused_variables))]
pub fn init_production(service_name: &str) -> ObserveGuard {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return ObserveGuard::inert();
    }
    let (make_writer, writer_guard) = writer::spawn();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_writer(make_writer)
        .with_filter(lifecycle_only());

    #[cfg(feature = "otlp")]
    {
        let provider = crate::otlp::provider_from_env(service_name);
        let otlp_layer = provider.as_ref().map(crate::otlp::layer);
        let registry = Registry::default().with(filter).with(fmt_layer).with(otlp_layer);
        if tracing::subscriber::set_global_default(registry).is_err() {
            // Another subscriber owns this process (an embedding host): keep its
            // choice and tear our own resources down.
            drop(writer_guard);
            if let Some(p) = provider {
                let _ = p.shutdown();
            }
            return ObserveGuard::inert();
        }
        ObserveGuard { writer: Some(writer_guard), provider }
    }
    #[cfg(not(feature = "otlp"))]
    {
        let registry = Registry::default().with(filter).with(fmt_layer);
        if tracing::subscriber::set_global_default(registry).is_err() {
            drop(writer_guard);
            return ObserveGuard::inert();
        }
        ObserveGuard { writer: Some(writer_guard) }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::admission::SampleAdmission;
    use crate::call_span::{CallIdentity, CallSpan, TraceEvent};
    use crate::rate_draw::RateDraw;
    use crate::token_bucket::TokenBucket;

    /// A `MakeWriter` collecting what the fmt layer renders.
    #[derive(Clone, Default)]
    struct Collected(Arc<Mutex<Vec<u8>>>);

    impl Collected {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("collect buffer")).into_owned()
        }
    }

    impl std::io::Write for Collected {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("collect buffer").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Collected {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn stdout_carries_the_lifecycle_plane_and_never_a_traced_calls_wire_bytes() {
        let out = Collected::default();
        let fmt_layer = tracing_subscriber::fmt::layer()
            .compact()
            .with_ansi(false)
            .with_writer(out.clone())
            .with_filter(lifecycle_only());
        let _guard = tracing::subscriber::set_default(Registry::default().with(fmt_layer));

        tracing::info!(node = "w-0", peer = "w-1", "takeover complete");

        let lease = SampleAdmission::new(true, 1.0, 10, RateDraw::seeded(1), TokenBucket::default_at(0))
            .admit(None, 0)
            .expect("the gate is wide open in this fixture");
        let span = CallSpan::open(
            lease,
            CallIdentity { call_id: "c1@host", from_tag: "ft", to_tag: "" },
        );
        span.record(TraceEvent::new("sip.in", 0, "alice").with_body(b"INVITE sip:bob SIP/2.0"));
        span.child("/call/new").record(TraceEvent::new("http.request", 1, "POST"));

        let text = out.text();
        assert!(text.contains("takeover complete"), "the lifecycle plane reaches stdout: {text}");
        assert!(
            !text.contains("INVITE sip:bob"),
            "a traced call's wire bytes must never reach stdout: {text}"
        );
        assert!(!text.contains("kind=sip.in"), "no per-call info line ever: {text}");
        assert!(!text.contains("http.request"), "…including the HTTP round trips: {text}");
    }
}
