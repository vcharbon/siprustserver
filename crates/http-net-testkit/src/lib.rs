//! `http-net-testkit` — the test support of the unary HTTP transport seam.
//!
//! Every type here implements or serves the [`HttpTransport`] seam:
//! - [`SimulatedHttpNetwork`] — an in-memory routing table keyed by
//!   `SocketAddr`. A `request` looks up the bound service, applies the per-`dst`
//!   [`Fault`] (pause / cutoff / error), waits the transit delay, and invokes
//!   the **real** handler in-process. Mandatory for the paused-clock tests:
//!   real sockets do not obey `tokio::time::pause`, so deterministic scenarios
//!   cannot use real HTTP. This is the workhorse.
//! - [`RecordingHttpNetwork`] — a decorator that records both sides of every
//!   exchange onto the `layer-harness` `Recorder` ([`HTTP_TAG`]), so HTTP joins
//!   the run's ladder; [`to_http_entries`] reads the channel back.
//! - [`scripted::ScriptedHttpService`] — an [`HttpService`] that serves the
//!   exchanges a scenario states as a program and answers anything else `500`,
//!   recorded as a finding. Its faults include closing the connection without
//!   a response, through [`HttpService::answer`] and [`HttpAnswer`].
//!
//! The crate declares no purity tier (ADR-0051 §1): a production binary that
//! links it fails the closure check.

mod entries;
mod recording;
pub mod scripted;
mod simulated;

// The seam's types and modules, imported under the paths this crate's modules read them by.
use http_net::{
    peer, settings, BindError, HttpAnswer, HttpError, HttpRequest, HttpResponse, HttpServerHandle,
    HttpService, HttpTransport,
};

pub use entries::{to_http_entries, CapturedExchange, ExchangeOutcome, RecordedHttpEntry};
pub use recording::{HttpNetworkEvent, HttpOutcome, RecordingHttpNetwork, HTTP_TAG};
pub use simulated::{Behaviour, Fault, SimulatedHttpNetwork};
