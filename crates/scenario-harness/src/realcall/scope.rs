//! Early-exit cleanup — the guarantee that a call which fails (or panics)
//! part-way through does **not** leak a dialog on the SUT.
//!
//! A leaked confirmed dialog would inflate `b2bua_active_calls`, hold a limiter
//! token, and contaminate the leak detector and endurance gates — exactly the
//! class of contamination the project's endurance work fights. So every call,
//! however it ends, CANCELs an INVITE that drew a provisional or BYEs its
//! confirmed dialog; an INVITE that drew none is owed no CANCEL (RFC 3261
//! §9.1) and is left to the SUT's own transaction timers.
//!
//! [`CallScope`] is owned by the runner **outside** the per-call `catch_unwind`
//! boundary, so it survives a scenario panic. As the scenario progresses it
//! registers a `Send + 'static` teardown snapshot: a [`CancelHandle`] once the
//! INVITE draws a provisional, upgraded to a [`Dialog`] clone once the call confirms (and
//! refreshed after each in-dialog request so the BYE CSeq stays valid), and
//! cleared to [`Terminated`](TeardownState::Terminated) once the scenario sends
//! its own BYE. After the scenario resolves (Ok / Err / panic) the runner calls
//! [`teardown`](CallScope::teardown), which acts on whatever was last registered.
//!
//! Each party's scope tears down the leg it holds: the caller's INVITE or
//! dialog, and an answering party's confirmed dialog, which a SUT that lost the
//! call would otherwise leave open (a 2xx never ACKed ends in a BYE,
//! §13.3.1.4). A canceled INVITE is given up: the party's transaction layer
//! still ACKs its non-2xx final (§17.1.1.3).

use std::sync::Mutex;

use crate::{CancelHandle, Dialog};

/// What teardown should do for a call, given how far it got. The dialog/cancel
/// payloads are boxed so the unit variants don't bloat the enum.
enum TeardownState {
    /// Nothing to tear down yet (no INVITE sent, or it drew no provisional —
    /// a CANCEL waits for one, RFC 3261 §9.1 — or already cleaned up).
    Idle,
    /// INVITE sent and answered provisionally, not yet confirmed → CANCEL
    /// (RFC 3261 §9.1).
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

    /// Register that the call's INVITE drew a provisional — teardown will
    /// CANCEL it (RFC 3261 §9.1).
    pub fn set_early(&self, handle: CancelHandle) {
        *self.inner.lock().unwrap() = TeardownState::Early(Box::new(handle));
    }

    /// Forget the early phase: the INVITE it named was answered and replaced by
    /// one that has drawn no provisional yet.
    pub fn clear_early(&self) {
        let mut g = self.inner.lock().unwrap();
        if matches!(*g, TeardownState::Early(_)) {
            *g = TeardownState::Idle;
        }
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
