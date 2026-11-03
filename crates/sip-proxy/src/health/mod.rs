//! Worker health probing — the OPTIONS keepalive loop toward the B2BUA workers
//! ([`probe`]), the proxy's first worker view it latches ([`first_view`]),
//! the notice of each dead verdict to the other workers ([`dead_notice`]), and
//! whether any worker is reachable at all ([`reach`]).

pub mod dead_notice;
pub mod first_view;
pub mod probe;
pub mod reach;

pub use dead_notice::DeadNotifier;
pub use first_view::FirstView;
pub use probe::{HealthProbe, HealthProbeConfig};
pub use reach::WorkerReach;
