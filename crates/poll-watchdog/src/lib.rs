//! The poll watchdog (ADR-0057 §6). Rust cannot preempt a thread and tokio
//! cancels only at an await, so a poll that never returns holds its thread
//! for ever. A runtime built with [`Watchdog::install`] keeps, per thread
//! that polls tasks, a slot whose sequence number is odd inside a poll and
//! even between polls; the watchdog thread reads the slots and each thread's
//! CPU time once a second ([`criterion`]). A poll that stays on one thread's
//! CPU past the threshold is logged, published to [`Watchdog::stuck`], and
//! the process ends with [`EXIT_CODE`] once the host's exit bound has
//! passed, even when the stuck thread is the runtime's only worker.

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

pub mod criterion;
pub mod poison;
pub mod schedstat;
mod slot;
mod watchdog;

pub use watchdog::{CallOf, Stuck, Watchdog, DEFAULT_EXIT_AFTER, EXIT_CODE, READ_PERIOD};
