//! The window of a cut point: on the call's primary, in the uncut run, the
//! time from the last SIP message it sent before the sub-point to the first
//! it sent after it. A crash lands in that window in proportion to its
//! width. Both fabrics carry every hop in at least 1 ms, so a window under
//! [`NARROW`] is one instant of the node: the two sends leave in one go, and
//! a cell cut between them is a corner of the method, run and judged like
//! any other but reported apart (ADR-0055 §11).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use sip_net_testkit::RecordedSipEntry;

use crate::turn_points::{Turn, TurnAt, TurnPoints};

/// The width under which a cut point's window is narrow.
pub const NARROW: Duration = Duration::from_millis(1);

/// Each cut point's window, `None` where the primary sent nothing before the
/// sub-point or nothing after it.
pub type Windows = BTreeMap<TurnAt, Option<Duration>>;

/// Whether `window` is narrower than [`NARROW`].
pub fn narrow(window: Option<Duration>) -> bool {
    window.is_some_and(|w| w < NARROW)
}

/// The window of every sub-point of `call_ref`'s `turns`, from the messages
/// `entries` recorded and the sub-points `points` placed among them, on the
/// primary that sends from `primary`.
pub fn measure(
    points: &TurnPoints,
    call_ref: &str,
    turns: &[Turn],
    entries: &[RecordedSipEntry],
    primary: SocketAddr,
) -> Windows {
    let sent: Vec<(u64, u64)> =
        entries.iter().filter(|e| e.from == primary).map(|e| (e.seq, e.sent_ms)).collect();
    let mut windows = Windows::new();
    for (i, turn) in turns.iter().enumerate() {
        for &point in turn.points() {
            let at = TurnAt { turn: i + 1, point };
            let window = points.stamp_of(call_ref, at).and_then(|stamp| around(&sent, stamp));
            windows.insert(at, window);
        }
    }
    windows
}

/// The time between the last of `sent` (place, ms) before `stamp` and the
/// first after it.
fn around(sent: &[(u64, u64)], stamp: u64) -> Option<Duration> {
    let before = sent.iter().filter(|(seq, _)| *seq < stamp).map(|(_, ms)| *ms).max()?;
    let after = sent.iter().filter(|(seq, _)| *seq > stamp).map(|(_, ms)| *ms).min()?;
    Some(Duration::from_millis(after.saturating_sub(before)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_spans_the_sends_around_the_sub_point() {
        let sent = [(1, 10), (4, 12), (9, 12), (12, 30)];
        assert_eq!(around(&sent, 5), Some(Duration::ZERO), "two sends of one instant");
        assert_eq!(around(&sent, 2), Some(Duration::from_millis(2)));
        assert_eq!(around(&sent, 10), Some(Duration::from_millis(18)));
        assert_eq!(around(&sent, 0), None, "nothing sent before");
        assert_eq!(around(&sent, 13), None, "nothing sent after");
        assert!(narrow(around(&sent, 5)));
        assert!(!narrow(around(&sent, 2)));
        assert!(!narrow(None));
    }
}
