//! Early-exit cleanup — the guarantee that a call which fails (or panics)
//! part-way through does **not** leak a dialog on the SUT.
//!
//! A leaked confirmed dialog would inflate `b2bua_active_calls`, hold a limiter
//! token, and contaminate the leak detector and endurance gates — exactly the
//! class of contamination the project's endurance work fights. So every call,
//! however it ends, must CANCEL its early dialog or BYE its confirmed one.
//!
//! [`CallScope`] is owned by the runner **outside** the per-call `catch_unwind`
//! boundary, so it survives a scenario panic. As the scenario progresses it
//! registers a `Send + 'static` teardown snapshot: a [`CancelHandle`] once the
//! INVITE is sent, upgraded to a [`Dialog`] clone once the call confirms (and
//! refreshed after each in-dialog request so the BYE CSeq stays valid), and
//! cleared to [`Terminated`](TeardownState::Terminated) once the scenario sends
//! its own BYE. After the scenario resolves (Ok / Err / panic) the runner calls
//! [`teardown`](CallScope::teardown), which acts on whatever was last registered.
//!
//! Teardown targets the **a-leg** (toward the cluster/SUT): that BYE/CANCEL is
//! what releases the SUT's call state and limiter token. The in-process
//! bob/charlie legs are simply dropped — once the a-leg is torn down the B2BUA
//! terminates the call and cleans up its b-leg on its own.

use std::sync::Mutex;

use crate::{CancelHandle, Dialog};

/// What teardown should do for a call, given how far it got. The dialog/cancel
/// payloads are boxed so the unit variants don't bloat the enum.
enum TeardownState {
    /// Nothing to tear down yet (no INVITE sent, or already cleaned up).
    Idle,
    /// INVITE sent, not yet confirmed → CANCEL (RFC 3261 §9.1).
    Early(Box<CancelHandle>),
    /// Dialog confirmed → BYE (RFC 3261 §15).
    Confirmed(Box<Dialog>),
    /// The scenario tore the call down itself (its own BYE) → nothing to do.
    Terminated,
}

/// Per-call teardown registry. `Send + Sync` (a plain `Mutex` over Send state),
/// cheap to construct, owned by the runner across the `catch_unwind` boundary.
pub struct CallScope {
    inner: Mutex<TeardownState>,
}

impl CallScope {
    pub fn new() -> Self {
        Self { inner: Mutex::new(TeardownState::Idle) }
    }

    /// Register that the call is in its early phase — teardown will CANCEL.
    pub fn set_early(&self, handle: CancelHandle) {
        *self.inner.lock().unwrap() = TeardownState::Early(Box::new(handle));
    }

    /// Register/refresh the confirmed dialog — teardown will BYE. Call again
    /// after each in-dialog request so the teardown BYE's CSeq stays ahead of
    /// what the scenario already sent.
    pub fn set_confirmed(&self, dialog: Dialog) {
        *self.inner.lock().unwrap() = TeardownState::Confirmed(Box::new(dialog));
    }

    /// Mark the call already torn down by the scenario (its own BYE succeeded);
    /// teardown becomes a no-op.
    pub fn mark_terminated(&self) {
        *self.inner.lock().unwrap() = TeardownState::Terminated;
    }

    /// Best-effort cleanup of whatever is registered. Idempotent: leaves the
    /// scope `Terminated`. Never panics (the helpers swallow transport errors),
    /// so it is safe to run after a caught scenario panic. The lock is released
    /// before any `.await`.
    pub async fn teardown(&self) {
        let state = {
            let mut g = self.inner.lock().unwrap();
            std::mem::replace(&mut *g, TeardownState::Terminated)
        };
        match state {
            TeardownState::Early(handle) => handle.cancel_best_effort().await,
            TeardownState::Confirmed(mut dialog) => dialog.bye_best_effort().await,
            TeardownState::Idle | TeardownState::Terminated => {}
        }
    }
}

impl Default for CallScope {
    fn default() -> Self {
        Self::new()
    }
}
