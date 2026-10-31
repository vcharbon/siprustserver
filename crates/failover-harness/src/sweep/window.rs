//! The window of a cut point: on the call's primary, in the uncut run, the
//! time from the last SIP message it sent before the sub-point to the first
//! it sent after it, or, when it sent nothing before, from the first message
//! of the call it received. A crash lands in that window in proportion to its
//! width. A cut point is narrow when it falls between two sends of one turn's
//! effects that leave less than [`NARROW`] apart: no turn starts between them,
//! and both fabrics carry every hop in at least 1 ms, so the two leave in one
//! go. A window as narrow between two turns is not: the fake clock gives no
//! time to what costs time in production — a decision engine's answer, two
//! inputs a scenario delivers together, a zero-delay timer. A narrow cell is a
//! corner of the method, run and judged like any other but reported apart
//! (ADR-0055 §11).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use sip_net_testkit::RecordedSipEntry;

use crate::turn_points::{Turn, TurnAt, TurnPoints};

/// The width under which a window between two sends of one turn is narrow.
pub const NARROW: Duration = Duration::from_millis(1);

/// A cut point's window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    /// Its width, `None` when the primary sent nothing after the sub-point.
    pub width: Option<Duration>,
    /// No turn of the primary starts between the two messages around it.
    pub one_turn: bool,
}

impl Window {
    /// Whether the cut point falls between two sends of one turn's effects
    /// less than [`NARROW`] apart.
    pub fn narrow(&self) -> bool {
        self.one_turn && self.width.is_some_and(|w| w < NARROW)
    }
}

/// Each cut point's window.
pub type Windows = BTreeMap<TurnAt, Window>;

/// The window of every sub-point of `call_ref`'s `turns`, from the messages
/// `entries` recorded and the sub-points `points` placed among them, on the
/// primary `ordinal` that sends from `primary`.
pub fn measure(
    points: &TurnPoints,
    call_ref: &str,
    turns: &[Turn],
    entries: &[RecordedSipEntry],
    (ordinal, primary): (&str, SocketAddr),
) -> Windows {
    let sent: Vec<(u64, u64)> =
        entries.iter().filter(|e| e.from == primary).map(|e| (e.seq, e.sent_ms)).collect();
    let call_id = call::parse_call_ref(call_ref).map(|r| r.call_id);
    let first_received = entries
        .iter()
        .filter(|e| e.to == primary && e.delivered)
        .find(|e| call_id.is_some() && sip_message::sniff::call_id(&e.raw) == call_id)
        .and_then(|e| Some((e.seq, e.received_ms?)));
    let starts = points.logged_stamps(ordinal);
    let mut windows = Windows::new();
    for (i, turn) in turns.iter().enumerate() {
        for &point in turn.points() {
            let at = TurnAt { turn: i + 1, point };
            let window = points
                .stamp_of(call_ref, at)
                .map(|stamp| around(&sent, first_received, &starts, stamp))
                .unwrap_or_default();
            windows.insert(at, window);
        }
    }
    windows
}

/// The window around `stamp`: from the last of `sent` (place, ms) before it,
/// else from `first_received`, to the first of `sent` after it, with whether
/// any of `starts` lies between the two.
fn around(
    sent: &[(u64, u64)],
    first_received: Option<(u64, u64)>,
    starts: &[u64],
    stamp: u64,
) -> Window {
    let before = sent
        .iter()
        .filter(|(seq, _)| *seq < stamp)
        .max_by_key(|(seq, _)| *seq)
        .copied()
        .or(first_received.filter(|(seq, _)| *seq < stamp));
    let after = sent.iter().filter(|(seq, _)| *seq > stamp).min_by_key(|(seq, _)| *seq).copied();
    let width = before
        .zip(after)
        .map(|((_, from), (_, to))| Duration::from_millis(to.saturating_sub(from)));
    let one_turn = before
        .zip(after)
        .is_some_and(|((from, _), (to, _))| !starts.iter().any(|s| from < *s && *s < to));
    Window { width, one_turn }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Option<Duration> {
        Some(Duration::from_millis(n))
    }

    #[test]
    fn a_window_spans_the_messages_around_the_sub_point() {
        let sent = [(1, 10), (4, 12), (9, 12), (12, 30)];
        let w = around(&sent, None, &[], 5);
        assert_eq!(w, Window { width: ms(0), one_turn: true }, "one turn's two sends");
        assert!(w.narrow());
        assert_eq!(around(&sent, None, &[], 2).width, ms(2));
        assert_eq!(around(&sent, None, &[], 10).width, ms(18));
        assert_eq!(around(&sent, None, &[], 13).width, None, "nothing sent after");
        assert!(!around(&sent, None, &[], 13).narrow());
    }

    #[test]
    fn a_turn_starting_between_the_two_sends_is_no_narrow_window() {
        let sent = [(4, 12), (9, 12)];
        let w = around(&sent, None, &[7], 5);
        assert_eq!(w, Window { width: ms(0), one_turn: false });
        assert!(!w.narrow(), "a decision or a timer that takes no time on the fake clock");
        assert!(!around(&sent, None, &[5], 5).narrow(), "the cut is the turn's own start");
    }

    #[test]
    fn with_nothing_sent_before_the_window_opens_at_the_first_message_received() {
        let sent = [(6, 15)];
        let w = around(&sent, Some((2, 9)), &[3], 4);
        assert_eq!(w, Window { width: ms(6), one_turn: false });
        assert_eq!(around(&sent, None, &[], 4).width, None);
    }
}
