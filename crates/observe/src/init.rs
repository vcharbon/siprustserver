//! Production subscriber installation.
//!
//! One call at the top of a runner's `main` installs the process subscriber and
//! returns the guard that must outlive every logging site. The guard drains the
//! log writer and flushes the tracer provider on drop, so a SIGTERM-drained
//! runner loses neither its last lines nor its in-flight spans.

use std::sync::atomic::{AtomicBool, Ordering};

use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry};

use crate::{otlp, writer};

/// Installed once per process; a second call is inert rather than a panic.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Levels emitted when `RUST_LOG` says nothing. Lifecycle logging is `info`.
const DEFAULT_FILTER: &str = "info";

/// Holds the process subscriber's resources. Drop flushes and shuts down.
pub struct ObserveGuard {
    writer: Option<writer::LogWriterGuard>,
    provider: Option<SdkTracerProvider>,
}

impl ObserveGuard {
    /// The inert guard: nothing was installed, nothing to flush.
    fn inert() -> Self {
        Self { writer: None, provider: None }
    }

    /// Whether this process exports spans over OTLP.
    pub fn exports_traces(&self) -> bool {
        self.provider.is_some()
    }
}

impl Drop for ObserveGuard {
    fn drop(&mut self) {
        // Spans first: their export may itself log.
        if let Some(provider) = self.provider.take() {
            let _ = provider.force_flush();
            let _ = provider.shutdown();
        }
        drop(self.writer.take());
    }
}

/// Install the process subscriber for `service_name` and return its guard.
///
/// Layers, in order:
/// 1. a compact single-line `key=value` fmt layer over the bounded lossy stdout
///    writer — lifecycle logs, always on, never back-pressuring;
/// 2. the OTLP span layer, present only when `OTEL_EXPORTER_OTLP_ENDPOINT` is
///    set (see [`otlp::provider_from_env`]).
///
/// Hold the returned guard until the process exits.
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
        .with_writer(make_writer);

    let provider = otlp::provider_from_env(service_name);
    let otlp_layer = provider.as_ref().map(otlp::layer);
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
