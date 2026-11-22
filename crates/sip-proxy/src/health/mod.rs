//! Worker health probing — the OPTIONS keepalive loop toward the B2BUA workers
//! ([`probe`]), the proxy's first worker view it latches ([`first_view`]),
//! the notice of each dead verdict to the other workers ([`dead_notice`]),
//! whether any worker is reachable at all ([`reach`]), and what the workers
//! last advertised ([`capabilities`]).

pub mod capabilities;
pub mod dead_notice;
pub mod first_view;
pub mod probe;
pub mod reach;
mod stated_host;

pub use capabilities::WorkerCapabilities;
pub use dead_notice::DeadNotifier;
pub use first_view::FirstView;
pub use probe::{HealthProbe, HealthProbeConfig};
pub use reach::WorkerReach;
