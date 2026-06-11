//! RFC 3261 §17 transaction timer constants. Verbatim port of the source
//! `TransactionLayer.ts` constants — behaviour rides `tokio::time` (via the
//! [`tokio_util::time::DelayQueue`] driver), so a single
//! `tokio::time::advance` moves all of these together in tests.

use std::time::Duration;

/// RTT estimate (ms). Base retransmit interval (Timer A / Timer E).
pub const T1: u64 = 500;
/// Max retransmit interval for non-INVITE (ms) — Timer E caps here.
pub const T2: u64 = 4000;

/// INVITE client transaction timeout (Timer B = 64·T1 = 32 s). Applies to
/// *in-dialog* re-INVITEs and (as Timer F) every non-INVITE — fast failure
/// detection for in-dialog traffic.
pub const TIMER_B: u64 = 64 * T1;
/// Non-INVITE client transaction timeout (Timer F = 64·T1 = 32 s).
pub const TIMER_F: u64 = 64 * T1;

/// INITIAL (out-of-dialog) INVITE client-transaction timeout — a call-setup
/// backstop, NOT the 32 s Timer B. RFC 3261 §17.1.1.2 scopes Timer B to the
/// Calling state; a ringing callee may legitimately take minutes, and the upper
/// layer's no-answer timer owns that deadline. We keep a hard expiry but place it
/// *below* the 180 s (3-minute) Timer-C mark and *above* any deployment no-answer
/// timeout, so the no-answer always fires first (clean CANCEL→487) and the
/// 3-minute timer never beats us — only this backstop, if no-answer is unset.
pub const INVITE_INITIAL_TIMEOUT: u64 = 158_000;
/// INVITE server txn cleanup after a final response (Timer H, RFC 3261 §17.2.1).
pub const TIMER_H: u64 = 64 * T1;
/// Non-INVITE server txn cleanup after a final response (Timer J, §17.2.2).
pub const TIMER_J: u64 = 64 * T1;
/// INVITE *client* txn Completed-state hold after ACKing a non-2xx final (Timer D,
/// §17.1.1.2). ≥ 32 s for unreliable transports — long enough to re-ACK + absorb
/// retransmitted finals after a lost ACK rather than re-surfacing them.
pub const TIMER_D: u64 = 64 * T1;

/// Safety-net sweep cadence (ms).
pub const TXN_SWEEP_INTERVAL: u64 = 10_000;
/// Safety-net max txn age — just above Timer H/J (32 s) so the sweep only ever
/// catches transactions a missing-cleanup bug would otherwise leak.
pub const TXN_MAX_AGE: u64 = 35_000;

pub(crate) const fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}
