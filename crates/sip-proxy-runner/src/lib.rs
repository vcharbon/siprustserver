//! The front proxy runner's library half: its boot configuration, its worker
//! pool, its health sampler, its intake shed, its readiness, the state its
//! recv shards share, and what its `/metrics` body holds.

pub mod config;
pub mod discovery;
pub mod health_sampler;
pub mod intake_shed;
pub mod metrics_body;
pub mod readiness;
pub mod shards;
