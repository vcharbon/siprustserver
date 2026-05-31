//! Routing strategies — pluggable implementations of the [`crate::strategy`]
//! `RoutingStrategy` seam.
//!
//! - [`rendezvous`] — the pure HRW helper both strategies build on.
//! - `forward_all` — the dev strategy (static target, unsigned cookie).
//! - `load_balancer` — the production strategy (HRW + signed cookie + routing
//!   matrix over worker health).

pub mod forward_all;
pub mod load_balancer;
pub mod rendezvous;

pub use forward_all::ForwardAllStrategy;
pub use load_balancer::{LoadBalancerConfig, LoadBalancerStrategy};
