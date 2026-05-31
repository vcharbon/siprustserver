//! RFC 3261 §17 transaction timer constants. Verbatim port of the source
//! `TransactionLayer.ts` constants — behaviour rides `tokio::time` (via the
//! [`tokio_util::time::DelayQueue`] driver), so a single
//! `tokio::time::advance` moves all of these together in tests.

use std::time::Duration;

/// RTT estimate (ms). Base retransmit interval (Timer A / Timer E).
pub const T1: u64 = 500;
/// Max retransmit interval for non-INVITE (ms) — Timer E caps here.
pub const T2: u64 = 4000;

/// INVITE client transaction timeout (Timer B = 64·T1 = 32 s).
pub const TIMER_B: u64 = 64 * T1;
/// Non-INVITE client transaction timeout (Timer F = 64·T1 = 32 s).
pub const TIMER_F: u64 = 64 * T1;
/// INVITE server txn cleanup after a final response (Timer H, RFC 3261 §17.2.1).
pub const TIMER_H: u64 = 64 * T1;
/// Non-INVITE server txn cleanup after a final response (Timer J, §17.2.2).
pub const TIMER_J: u64 = 64 * T1;

/// Safety-net sweep cadence (ms).
pub const TXN_SWEEP_INTERVAL: u64 = 10_000;
/// Safety-net max txn age — just above Timer H/J (32 s) so the sweep only ever
/// catches transactions a missing-cleanup bug would otherwise leak.
pub const TXN_MAX_AGE: u64 = 35_000;

pub(crate) const fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}
