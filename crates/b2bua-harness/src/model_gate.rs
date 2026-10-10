//! The model-invariant gate: a B2BUA turn that breaks a model invariant
//! panics inside the dispatcher, which isolates the panic and lets the call
//! go on to the reaper. The gate reads the process's record of those
//! violations ([`b2bua::rules::model_invariants::violations`]) from a
//! [`Mark`] on, so a run fails on the first one it met, whatever the
//! scenario did next, and runs that follow one another in one process judge
//! only their own.

/// A point in the process's record of violations: a gate judges the ones
/// recorded after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark(usize);

impl Mark {
    /// Now: only the violations recorded from here on.
    pub fn now() -> Mark {
        Mark(b2bua::rules::model_invariants::violations().len())
    }
}

/// The first model-invariant violation recorded since `since`, if any.
pub fn first_violation(since: Mark) -> Option<String> {
    b2bua::rules::model_invariants::violations().into_iter().nth(since.0)
}

/// Panic with the first model-invariant violation recorded since `since`, if
/// any.
#[track_caller]
pub fn assert_no_model_violation(since: Mark) {
    if let Some(first) = first_violation(since) {
        panic!("{first}");
    }
}

/// [`assert_no_model_violation`] for a `Drop`: never panics while the thread
/// already unwinds, and names the violation on stderr instead.
pub fn gate_on_drop(since: Mark) {
    if std::thread::panicking() {
        if let Some(first) = first_violation(since) {
            let line = first.lines().next().unwrap_or_default();
            eprintln!("an earlier B2BUA turn broke a model invariant: {line}");
        }
        return;
    }
    assert_no_model_violation(since);
}
