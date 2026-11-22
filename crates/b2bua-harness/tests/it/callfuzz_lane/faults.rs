//! The network a fuzzed call crosses: an unreliable transport that loses and
//! holds back datagrams (RFC 3261 §18), each party's transaction layer riding
//! it out (§17). The faults stay inside what SIP recovers from: a hold of 2 s
//! at most (under 64·T1), only on a response or an ACK, never on a reliable
//! provisional, whose PRACK is owed only if it arrives before the final
//! (RFC 3262 §4). The fates are drawn from the call's seed in send order, so
//! a call replays with its faults.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use callfuzz::Rng;
use sip_net_testkit::{Fate, SendFate};

/// The salt of the fault draws: apart from the call's other draws.
const FAULT_SALT: u64 = 0xFA17_5A17_FA17_5A17;

/// The longest a held datagram waits past the transit, in ms.
const LATE_MAX_MS: u64 = 2_000;

/// The percent of datagrams lost and held back.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Faults {
    pub(super) drop: u64,
    pub(super) late: u64,
}

impl Faults {
    pub(super) fn any(&self) -> bool {
        self.drop + self.late > 0
    }
}

/// How many datagrams met each fault in one call.
#[derive(Debug, Default)]
pub(super) struct Tally {
    lost: AtomicU32,
    held: AtomicU32,
}

impl Tally {
    /// The tally as a draw note, empty when no datagram met a fault.
    pub(super) fn note(&self) -> String {
        let [lost, held] = [&self.lost, &self.held].map(|n| n.load(Ordering::Relaxed));
        if lost + held == 0 {
            return String::new();
        }
        format!(" net(lost {lost}, held {held})")
    }
}

/// Whether a hold keeps `raw` within what SIP recovers from: a response that
/// is not a reliable provisional, or an ACK.
fn holdable(raw: &[u8]) -> bool {
    match sip_message::sniff::resp_status(raw) {
        Some(status) => status >= 200 || sip_message::sniff::header_value(raw, "RSeq").is_none(),
        None => sip_message::sniff::req_method(raw).as_deref() == Some("ACK"),
    }
}

/// The fate of each datagram of the call of `seed`, counted in `tally`.
pub(super) fn fate_of(faults: Faults, seed: u64, tally: Arc<Tally>) -> SendFate {
    let rng = Mutex::new(Rng::new(seed ^ FAULT_SALT));
    Arc::new(move |_src, _dst, raw| {
        let mut rng = rng.lock().expect("fault draws");
        if rng.chance(faults.drop) {
            tally.lost.fetch_add(1, Ordering::Relaxed);
            return Fate::Lose;
        }
        if holdable(raw) && rng.chance(faults.late) {
            tally.held.fetch_add(1, Ordering::Relaxed);
            return Fate::Late(Duration::from_millis(1 + rng.below(LATE_MAX_MS)));
        }
        Fate::Deliver
    })
}
