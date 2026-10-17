//! Worker health probing — the OPTIONS keepalive loop toward the B2BUA workers
//! ([`probe`]) and the proxy's first worker view it latches ([`first_view`]).

pub mod first_view;
pub mod probe;

pub use first_view::FirstView;
pub use probe::{HealthProbe, HealthProbeConfig};
