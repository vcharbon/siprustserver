//! The model-invariant gate: a B2BUA turn that breaks a model invariant, or
//! notes a defect, lands all the same and leaves the violation in the
//! process's record ([`b2bua::rules::model_invariants::violations`]). The gate
//! reads that record from a [`Mark`] on, so a run fails on the first violation
//! it met, whatever the scenario did next, and runs that follow one another in
//! one process judge only their own. A test that provokes a violation on
//! purpose takes it ([`crate::B2buaSut::take_model_violations`]) and judges it
//! itself.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A point in the process's record of violations: a gate judges the ones
/// recorded after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark(usize);

impl Mark {
    /// Now: only the violations recorded from here on, the first of them
    /// with the call's dump.
    pub fn now() -> Mark {
        Mark(b2bua::rules::model_invariants::mark())
    }
}

/// A run's [`Mark`], shared by its SUT and its failure view, so a test that
/// takes the run's violations moves the mark for both.
#[derive(Debug, Clone)]
pub struct Since(Arc<AtomicUsize>);

impl Since {
    /// A run starting now.
    pub fn now() -> Since {
        Since(Arc::new(AtomicUsize::new(Mark::now().0)))
    }

    /// Where the run's judged violations start.
    pub fn mark(&self) -> Mark {
        Mark(self.0.load(Ordering::Relaxed))
    }

    /// Judge only the violations recorded from now on.
    fn restart(&self) {
        self.0.store(Mark::now().0, Ordering::Relaxed);
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

impl crate::B2buaSut {
    /// The violations this run kept so far, for a test that provokes one on
    /// purpose: taken, they no longer fail the run.
    pub fn take_model_violations(&mut self) -> Vec<String> {
        let since = self.since.mark().0;
        let kept = b2bua::rules::model_invariants::violations().into_iter().skip(since).collect();
        self.since.restart();
        kept
    }
}
