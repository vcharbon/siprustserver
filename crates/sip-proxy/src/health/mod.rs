//! Worker health probing — the OPTIONS keepalive loop toward the B2BUA workers
//! ([`probe`]), the proxy's first worker view it latches ([`first_view`]), and
//! the notice of each dead verdict to the other workers ([`dead_notice`]).

pub mod dead_notice;
pub mod first_view;
pub mod probe;

pub use dead_notice::DeadNotifier;
pub use first_view::FirstView;
pub use probe::{HealthProbe, HealthProbeConfig};
