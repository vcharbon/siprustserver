//! The instants at which the parties' INVITE transactions resend over UDP
//! (RFC 3261 §17), for a test that plays a party's retransmissions by hand.

use std::time::Duration;

use sip_retransmit::timers::{self, ms, T1, T2};

/// The INVITE client's bound with no response heard, 64·T1 (§17.1.1.2).
pub const TIMER_B: Duration = ms(timers::TIMER_B);

/// The INVITE server's bound on the ACK of its non-2xx final, 64·T1
/// (§17.2.1).
pub const TIMER_H: Duration = ms(timers::TIMER_H);

/// The instants, after its first send, at which an INVITE client resends the
/// INVITE on Timer A (§17.1.1.2): T1, the interval doubling, each before
/// Timer B.
pub fn timer_a_copies() -> Vec<Duration> {
    copies(|interval| interval * 2, timers::TIMER_B)
}

/// The instants, after its first send, at which an INVITE server resends its
/// non-2xx final on Timer G (§17.2.1): T1, the interval doubling up to T2,
/// each before Timer H.
pub fn timer_g_copies() -> Vec<Duration> {
    copies(|interval| (interval * 2).min(T2), timers::TIMER_H)
}

/// The instants `T1`, then each `next` interval later, while under `bound` ms.
fn copies(next: impl Fn(u64) -> u64, bound: u64) -> Vec<Duration> {
    let mut out = Vec::new();
    let mut interval = T1;
    let mut at = T1;
    while at < bound {
        out.push(ms(at));
        interval = next(interval);
        at += interval;
    }
    out
}
