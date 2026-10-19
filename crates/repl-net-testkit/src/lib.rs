//! `repl-net-testkit` — the test support of the replication transport seam.
//!
//! Both types implement or wrap the [`ReplicationNetwork`] seam:
//! - [`SimulatedReplicationNetwork`] — in-process ordered delivery, the
//!   workhorse for the fake-clock tests. Mandatory, not a convenience: real
//!   `TcpStream` readiness does **not** obey `tokio::time::pause`, so the
//!   paused-clock scenarios *cannot* use real TCP. It moves encoded bytes, so
//!   the wire codec plays through every test path.
//! - [`RecordingReplicationNetwork`] — a decorator that tees every decoded
//!   frame into a capture sink for the report.
//!
//! The crate declares no purity tier (ADR-0051 §1): a production binary that
//! links it fails the closure check.

mod recording;
mod simulated;

// The seam's types and modules, imported under the paths this crate's modules read them by.
use repl_net::transport::{
    AcceptError, ConnectError, ListenError, ReplicationConnection, ReplicationListener,
    ReplicationNetwork, SendError,
};
use repl_net::{decode_frame, transport, Frame};
#[cfg(test)]
use repl_net::{Op, Partition, Watermark};

pub use recording::{CaptureSeq, CapturedFrame, Direction, RecordingReplicationNetwork};
pub use simulated::{Fault, NodeReplicationNetwork, SimulatedReplicationNetwork};
