//! The front proxy runner's library half: its boot configuration, its worker
//! pool, its readiness, and what its `/metrics` body holds.

pub mod config;
pub mod discovery;
pub mod metrics_body;
pub mod readiness;
