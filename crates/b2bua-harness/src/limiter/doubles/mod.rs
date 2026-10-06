//! Scripted call limiters for tests: standalone
//! [`CallLimiter`](b2bua::limiter::CallLimiter)s that answer every request
//! one way, and decorators over any inner limiter, composed by nesting
//! (`spy(refuse_on("cap", fail_open()))`).
//!
//! No double states a health answer, so the worker runs it without a circuit
//! breaker whatever it wraps. A decorator forwards `report_to` and every
//! request it does not script to what it wraps, and states the admit budget
//! of what it wraps unless its contract says otherwise.

mod admit_faults;
mod answers;
mod forward;
mod in_process;
mod refusals;
mod releases;
mod spy;

pub use admit_faults::{
    answer_nth_admit_late, delay_admits_after, delay_nth_admit, never_answer_admit,
    unavailable_on_nth,
};
pub use answers::{admit_all, answers_released, fail_open};
pub use forward::without_breaker;
pub use in_process::in_process;
pub use refusals::{refuse_all, refuse_on};
pub use releases::{drop_releases, gate_releases, panic_on_first_release, ReleaseGate};
pub use spy::{spy, SpiedAdmit, Spy};
