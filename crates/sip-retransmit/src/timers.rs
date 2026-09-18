//! RFC 3261 §17 transaction timer constants — the one home for them, paced
//! against by transaction and dialog-level ladders alike (ADR-0032).
//! Behaviour rides `tokio::time` (via the [`tokio_util::time::DelayQueue`]
//! driver in `sip-txn`), so a single `tokio::time::advance` moves all of these
//! together in tests.

use std::time::Duration;

/// RTT estimate (ms). Base retransmit interval (Timer A / Timer E).
pub const T1: u64 = 500;

/// Max retransmit interval for non-INVITE (ms) — Timer E caps here.
pub const T2: u64 = 4000;

/// Maximum time a message stays in the network (ms, RFC 3261 §17.1.2.2) — the
/// bound on how late a retransmitted ACK can still arrive (Timer I).
pub const T4: u64 = 5000;

/// INVITE client transaction timeout (Timer B = 64·T1 = 32 s): the bound of an
/// INVITE — initial or in-dialog — that has drawn no response at all (RFC 3261
/// §17.1.1.2 scopes it to Calling), and (as Timer F) of every non-INVITE —
/// fast failure detection for a hop that answers nothing.
pub const TIMER_B: u64 = 64 * T1;

/// Non-INVITE client transaction timeout (Timer F = 64·T1 = 32 s).
pub const TIMER_F: u64 = 64 * T1;

/// DEFAULT for the INVITE transaction bound
/// (`sip_txn::TransactionConfig::invite_initial_timeout_ms`) — the pre-final
/// backstop of an INVITE in Proceeding, NOT the 32 s Timer B.
/// RFC 3261 §17.1.1.2 scopes Timer B to the Calling state, so this bound is
/// armed at the FIRST PROVISIONAL and never before it: a ringing callee may
/// legitimately take minutes and the upper layer's no-answer timer owns that
/// deadline, while an INVITE nothing ever answered stays on Timer B. The bound
/// is a hard expiry that must sit *above* every deployment setup/no-answer
/// timeout, so the app deadline always fires first (clean CANCEL→487) and only
/// this backstop remains when none is set. The default stays below the 180 s
/// Timer-C mark; a telephony deployment raises the config field (up to ~600 s)
/// rather than this const.
pub const INVITE_INITIAL_TIMEOUT: u64 = 158_000;

/// INVITE server txn cleanup after a final response (Timer H, RFC 3261 §17.2.1).
pub const TIMER_H: u64 = 64 * T1;

/// Non-INVITE server txn cleanup after a final response (Timer J, §17.2.2).
pub const TIMER_J: u64 = 64 * T1;

/// INVITE *server* txn Confirmed-state hold after the ACK for a non-2xx final
/// (Timer I, RFC 3261 §17.2.1): T4 on UDP, the transport this layer rides —
/// long enough to absorb the ACK's retransmissions and to refuse a second
/// final on the branch. `TIMER_I_RELIABLE` is its value on a reliable
/// transport, where no ACK retransmission can follow.
pub const TIMER_I: u64 = T4;

/// Timer I on a reliable transport (RFC 3261 §17.2.1): zero, the transaction
/// leaves at once.
pub const TIMER_I_RELIABLE: u64 = 0;

/// INVITE *client* txn Completed-state hold after ACKing a non-2xx final (Timer D,
/// §17.1.1.2). ≥ 32 s for unreliable transports — long enough to re-ACK + absorb
/// retransmitted finals after a lost ACK rather than re-surfacing them.
pub const TIMER_D: u64 = 64 * T1;

/// INVITE *server* txn hold in Accepted after a 2xx (Timer L, RFC 6026 §7.1):
/// the window in which the ACK may still arrive and a re-INVITE on the same
/// dialog is a §14.1 violation rather than a new transaction. Equal to the
/// §13.3.1.4 retransmission bound, which is what makes it the give-up of
/// [`crate::Class::Final2xx`].
pub const TIMER_L: u64 = 64 * T1;

/// INVITE *client* txn hold in Accepted after a 2xx the layer ACKed on the
/// transaction's own behalf (Timer M, RFC 6026 §7.2): the §13.3.1.4 window in
/// which the answerer may still repeat the 2xx, each repeat re-drawing the ACK
/// (RFC 3261 §13.2.2.4). Equal to that retransmission bound.
pub const TIMER_M: u64 = 64 * T1;

/// DEFAULT for the held-CANCEL grace window
/// (`sip_txn::TransactionConfig::cancel_hold_grace_ms`) — how long a CANCEL for
/// a response-less INVITE client txn waits for the branch's first provisional
/// (RFC 3261 §9.1) before being sent regardless (ADR-0028). 2·T1: long enough
/// for the original INVITE plus one Timer-A retransmit to reach a UAS and draw
/// its 100, so the grace-expiry send is a rare fallback, not the common path.
/// The RFC-audit acceptance floor (`rfc_rules::rules::cancel::CANCEL_GRACE_FLOOR_US`,
/// the `cancel-after-1xx` rule) sits just below this value — keep them in step.
pub const CANCEL_HOLD_GRACE: u64 = 2 * T1;

/// Safety-net sweep cadence (ms).
pub const TXN_SWEEP_INTERVAL: u64 = 10_000;

/// Safety-net max txn age — just above Timer H/J (32 s) so the sweep only ever
/// catches transactions a missing-cleanup bug would otherwise leak.
pub const TXN_MAX_AGE: u64 = 35_000;

/// The constants above are milliseconds; this is the one place they become a
/// `Duration`, so no caller re-states the unit.
pub const fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}
