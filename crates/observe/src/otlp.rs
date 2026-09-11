//! The OTLP span-export layer.
//!
//! Built ONLY when `OTEL_EXPORTER_OTLP_ENDPOINT` is set: with no endpoint the
//! process installs no tracer provider at all, so no span is ever constructed
//! and the export tree stays idle. Spans leave over OTLP http/protobuf through
//! a batch processor, so a slow or absent collector never blocks a SIP task.
//! That processor owns a dedicated OS thread outside the tokio runtime, so the
//! HTTP client under it is the blocking one — see the workspace `[workspace
//! .dependencies]` note on `opentelemetry-otlp`.
//!
//! The endpoint is a BASE url, per the OTLP/HTTP spec: an operator sets the same
//! value any OpenTelemetry SDK takes, and this module appends the trace-signal
//! path (`<base>/v1/traces`) and hands the exporter the resolved url. Resolving
//! it here — rather than leaving it to the exporter's env fallback — is what
//! makes an unusable endpoint fail loudly at build time instead of silently
//! redirecting every batch to the SDK's `localhost:4318` default.
//!
//! Failure is REPORTED, not logged here: resolution runs before a subscriber
//! exists, so this module returns [`ExportSetup::Unusable`] and
//! [`crate::init_production`] emits the warning once the subscriber is
//! installed.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;

/// The instrumentation-scope name every root span is created under.
const SCOPE: &str = "sip-observe";

/// The OTLP/HTTP trace-signal path appended to the configured base url.
const TRACES_PATH: &str = "/v1/traces";

/// The trace-signal url for OTLP/HTTP base `base`, or `None` when `base` is
/// blank and so names no endpoint — the same emptiness rule
/// [`crate::exporter_configured`] applies, so the two never disagree about
/// whether this process exports.
fn signal_endpoint(base: &str) -> Option<String> {
    let base = base.trim();
    (!base.is_empty()).then(|| format!("{}{TRACES_PATH}", base.trim_end_matches('/')))
}

/// What resolving the environment says about this process's span export.
pub enum ExportSetup {
    /// No endpoint is named: this process exports nothing, by configuration.
    Disabled,
    /// The endpoint resolved and its exporter built.
    Provider(SdkTracerProvider),
    /// An endpoint is named but its exporter refused to build: this process
    /// exports nothing, against the operator's intent. Carries what the
    /// installer needs to say so — a broken collector url degrades to "no
    /// traces", never to a crash-looping runner and never to a silently
    /// redirected export.
    Unusable { endpoint: String, error: String },
}

/// This process's export setup, read from `OTEL_EXPORTER_OTLP_ENDPOINT`.
pub fn export_from_env(service_name: &str) -> ExportSetup {
    match std::env::var(crate::OTLP_ENDPOINT_ENV).ok().as_deref().and_then(signal_endpoint) {
        Some(endpoint) => build(&endpoint, service_name),
        None => ExportSetup::Disabled,
    }
}

/// The export setup for an already-resolved trace-signal `endpoint`.
fn build(endpoint: &str, service_name: &str) -> ExportSetup {
    match SpanExporter::builder().with_http().with_endpoint(endpoint).build() {
        Ok(exporter) => ExportSetup::Provider(
            SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(
                    Resource::builder().with_service_name(service_name.to_string()).build(),
                )
                .build(),
        ),
        Err(error) => {
            ExportSetup::Unusable { endpoint: endpoint.to_string(), error: error.to_string() }
        }
    }
}

/// The `tracing` layer feeding `provider`'s tracer.
pub fn layer<S>(
    provider: &SdkTracerProvider,
) -> tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_opentelemetry::layer().with_tracer(provider.tracer(SCOPE))
}

#[cfg(test)]
mod tests {
    use super::{build, signal_endpoint, ExportSetup};

    #[test]
    fn an_unusable_endpoint_is_reported_not_logged() {
        match build("not a url/v1/traces", "svc") {
            ExportSetup::Unusable { endpoint, error } => {
                assert_eq!(endpoint, "not a url/v1/traces");
                assert!(!error.is_empty(), "the installer needs a cause to log");
            }
            _ => panic!("an endpoint no url parser accepts must not yield a provider"),
        }
    }

    #[test]
    fn base_url_gains_the_trace_signal_path() {
        assert_eq!(
            signal_endpoint("http://vt:10428/insert/opentelemetry").as_deref(),
            Some("http://vt:10428/insert/opentelemetry/v1/traces")
        );
    }

    #[test]
    fn a_trailing_slash_does_not_double_up() {
        assert_eq!(
            signal_endpoint("http://vt:10428/insert/opentelemetry/").as_deref(),
            Some("http://vt:10428/insert/opentelemetry/v1/traces")
        );
    }

    #[test]
    fn surrounding_whitespace_is_not_part_of_the_endpoint() {
        assert_eq!(
            signal_endpoint("  http://vt:10428  ").as_deref(),
            Some("http://vt:10428/v1/traces")
        );
    }

    #[test]
    fn a_blank_base_names_no_endpoint() {
        assert_eq!(signal_endpoint(""), None);
        assert_eq!(signal_endpoint("   "), None);
    }
}
