//! The model-invariant gate: a B2BUA turn that breaks a model invariant
//! panics inside the dispatcher, which isolates the panic and lets the call
//! go on to the reaper. The gate reads the process's record of those
//! violations ([`b2bua::rules::model_invariants::violations`]) so the run
//! fails on the first one, whatever the scenario did next.

/// Panic with the first model-invariant violation this process met, if any.
#[track_caller]
pub fn assert_no_model_violation() {
    if let Some(first) = b2bua::rules::model_invariants::violations().into_iter().next() {
        panic!("{first}");
    }
}

/// [`assert_no_model_violation`] for a `Drop`: never panics while the thread
/// already unwinds, and names the violation on stderr instead.
pub fn gate_on_drop() {
    if std::thread::panicking() {
        if let Some(first) = b2bua::rules::model_invariants::violations().into_iter().next() {
            let line = first.lines().next().unwrap_or_default();
            eprintln!("an earlier B2BUA turn broke a model invariant: {line}");
        }
        return;
    }
    assert_no_model_violation();
}
