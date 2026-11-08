//! The crate's named lock (ADR-0051 §7): every [`crate::queue::PacketQueue`]
//! takes its state under `packet_queue`, and a poisoned one is recovered,
//! logged and counted under it.

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;

const PACKET_QUEUE: &str = "packet_queue";

/// Every lock name, in counter order.
pub const LOCKS: [&str; 1] = [PACKET_QUEUE];

/// The recoveries of the crate's locks in this process.
pub static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock a queue's state `m`.
pub(crate) fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    RECOVERIES.mutex(m, PACKET_QUEUE)
}
