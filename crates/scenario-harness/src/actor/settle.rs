//! The **settle barrier** — the acknowledgement-gated verdict. A call is OK
//! only once every in-dialog request is acknowledged (each REFER-progress
//! NOTIFY, each re-INVITE ACK, each BYE 200); we do NOT excuse a loss by
//! suppressing an audit finding — we require the protocol to actually recover
//! it via re-emission.
//!
//! The ceiling is `64·T1 = 32 s` — RFC 3261 Timer B/F/H — the protocol's own
//! bound on re-emission, so ALL recovery (the peer's and the SUT's own
//! Timer-E/G retransmits) has completed in every case. It is not a knob.
//!
//! **No `SettleDriver` trait (B2/B4).** Both lanes use plain `tokio::time`.
//! Under `#[tokio::test(start_paused)]` tokio auto-advances to the earliest
//! pending timer *only while the reactors are idle*, so a re-emit that lands
//! mid-window wakes the reactors first and the ledger closes before the ceiling
//! is reached; on the load lane the same `sleep` is a real wall-clock wait. The
//! loop re-checks every `T1` and never leaps the ceiling in one step (clock
//! rule 2). In the no-loss case the ledger is already closed, so `wait` returns
//! on the first poll — the ceiling only bites when a drop actually occurred.

use std::time::Duration;

use tokio::time::Instant;

use super::state::ObservedState;

/// RFC 3261 T1 (500 ms) — the settle poll cadence; `64·T1` is the ceiling.
pub const T1: Duration = Duration::from_millis(500);

/// The verdict of the settle barrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleVerdict {
    /// Every obligation was acknowledged within the ceiling.
    Ok,
    /// The ceiling elapsed with obligations still open — names each one.
    Fail(Vec<String>),
}

/// Holds the verdict until the ledger closes, bounded by `ceiling`.
pub struct SettleBarrier {
    pub ceiling: Duration,
}

impl SettleBarrier {
    /// The default `64·T1 = 32 s` ceiling.
    pub fn default_ceiling() -> Self {
        Self { ceiling: 64 * T1 }
    }

    /// Wait for the ledger to close, re-checking every `T1`. Returns `Ok` the
    /// instant it closes (immediately in the no-loss case), or `Fail` naming the
    /// still-open obligations once the ceiling elapses.
    pub async fn wait(&self, obs: &ObservedState) -> SettleVerdict {
        let deadline = Instant::now() + self.ceiling;
        loop {
            if obs.ledger_closed() {
                return SettleVerdict::Ok;
            }
            if Instant::now() >= deadline {
                return SettleVerdict::Fail(obs.describe_open());
            }
            // One T1 step (real on the load lane; auto-advanced under a paused
            // clock while the reactors are idle). Never a single 32 s leap.
            tokio::time::sleep(T1).await;
        }
    }
}
