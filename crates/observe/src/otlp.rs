//! The OTLP span-export layer.
//!
//! Built ONLY when `OTEL_EXPORTER_OTLP_ENDPOINT` is set: with no endpoint the
//! process installs no tracer provider at all, so no span is ever constructed
//! and the export tree stays idle. Spans leave over OTLP http/protobuf through
//! a batch processor, so a slow or absent collector never blocks a SIP task.
//! That processor owns a dedicated OS thread outside the tokio runtime, so the
//! HTTP client under it is the blocking one — see the workspace `[workspace
//! .dependencies]` note on `opentelemetry-otlp`.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;

/// The instrumentation-scope name every root span is created under.
const SCOPE: &str = "sip-observe";

/// The configured tracer provider, or `None` when this process exports nothing.
/// An endpoint that is set but unusable also yields `None` — a broken collector
/// URL degrades to "no traces", never to a crash-looping runner.
pub fn provider_from_env(service_name: &str) -> Option<SdkTracerProvider> {
    let endpoint = std::env::var(crate::OTLP_ENDPOINT_ENV).ok()?;
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return None;
    }
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| {
            tracing::warn!(
                endpoint = endpoint,
                error = %e,
                "OTLP exporter refused to build; this process exports no traces"
            )
        })
        .ok()?;
    Some(
        SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(Resource::builder().with_service_name(service_name.to_string()).build())
            .build(),
    )
}

/// The `tracing` layer feeding `provider`'s tracer.
pub fn layer<S>(provider: &SdkTracerProvider) -> tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_opentelemetry::layer().with_tracer(provider.tracer(SCOPE))
}
