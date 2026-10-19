//! The calls-reaped oracle of a [`B2buaSut`]: every call created was reaped,
//! no per-call state survives, and the shell met no fault the test did not
//! declare.

use std::sync::atomic::{AtomicBool, Ordering};

use b2bua::metrics::shell::shell_faults;
use b2bua::metrics::B2buaMetrics;

use crate::B2buaSut;

/// The shell faults a run starts from, and whether its test declared some.
pub(crate) struct ShellFaults {
    since: u64,
    declared: AtomicBool,
}

impl ShellFaults {
    /// The process's shell faults now, none declared.
    pub(crate) fn from_now() -> Self {
        Self { since: shell_faults(), declared: AtomicBool::new(false) }
    }

    /// Assert, unless declared, that the store's sweep of `metrics` corrected
    /// nothing and no shell fault was met since the start.
    #[track_caller]
    fn assert_none(&self, metrics: &B2buaMetrics) {
        if self.declared.load(Ordering::Relaxed) {
            return;
        }
        assert_eq!(
            metrics.store_reconciled_total(),
            0,
            "store defect: the reaper sweep corrected {} store entr(ies)",
            metrics.store_reconciled_total()
        );
        let met = shell_faults().saturating_sub(self.since);
        assert_eq!(met, 0, "shell defect: {met} poisoned lock(s) recovered or task(s) restarted");
    }
}

impl B2buaSut {
    /// Declare that this test injects a store, lock or task fault on purpose:
    /// the calls-reaped oracle no longer asserts check 8.
    pub fn expect_shell_faults(&self) {
        self.shell.declared.store(true, Ordering::Relaxed);
    }

    /// Checks 1–5 and 8 of [`assert_fully_reaped`](Self::assert_fully_reaped): every
    /// call created was reaped and no per-call state survives, the limiter
    /// aside. For a caller that judges the calls at one instant and the
    /// limiter after a later settle, which a release still in flight needs.
    #[track_caller]
    pub fn assert_calls_reaped(&self) {
        let (creations, removals) = (self.metrics.creations_total(), self.metrics.removals_total());
        assert_eq!(
            creations, removals,
            "call leak: creations ({creations}) != removals ({removals}) — a call \
             was created but never reaped"
        );
        assert_eq!(
            self.active_calls(),
            0,
            "call leak: {} live call(s) left in the map",
            self.active_calls()
        );
        assert_eq!(
            self._core.unborn_calls(),
            0,
            "admission leak: {} admitted new call(s) still counted unborn",
            self._core.unborn_calls()
        );
        assert_eq!(
            self.lock_count(),
            0,
            "lock leak: {} stranded per-call serialization lock(s)",
            self.lock_count()
        );
        // 4. (ADR-0020) the reaper's last-touched ledger mirrors the call map;
        //    a residue is a stamp leak.
        assert_eq!(
            self._core.touched_count(),
            0,
            "stamp leak: {} stranded last-touched ledger entr(ies)",
            self._core.touched_count()
        );
        // 5. the setup-CANCEL marks (the decision-application drop guard) are
        //    cleared on every teardown path; a residue is a mark leak.
        assert_eq!(
            self._core.setup_cancelled_count(),
            0,
            "mark leak: {} stranded setup-CANCEL mark(s)",
            self._core.setup_cancelled_count()
        );
        // 8. the store's sweep corrected nothing, and no lock was poisoned nor
        //    a supervised task restarted: each is a defect unless declared.
        self.shell.assert_none(&self.metrics);
    }
}
