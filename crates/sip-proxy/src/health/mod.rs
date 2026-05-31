//! Worker health probing — the OPTIONS keepalive loop toward the B2BUA workers.
//! See [`probe`].

pub mod probe;

pub use probe::{HealthProbe, HealthProbeConfig};
