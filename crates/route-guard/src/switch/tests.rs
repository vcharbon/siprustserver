//! The switch over scripted readings, one every 100 ms on the paused clock.

use super::*;

const TIMING: Timing =
    Timing { dead: Duration::from_millis(1_000), hold: Duration::from_millis(3_000) };
const POLL: Duration = Duration::from_millis(100);

fn unfit(reason: &str) -> Reading {
    Reading::Unfit(reason.to_string())
}

fn gone() -> Reading {
    Reading::NoAnswer("connection refused".to_string())
}

/// A switch started on a fit proxy, its first refresh written.
fn running(origin: Instant) -> Switch {
    let (mut s, write, cause) = Switch::start(TIMING, origin, &Reading::Fit);
    assert_eq!((write, cause), (Write::Refresh, None));
    assert!(!s.refreshed(origin), "the start's refresh lets out no withdrawn node");
    s
}

/// Feeds `reading` every [`POLL`] from `from` for `count` polls, writing
/// every refresh asked: the instants of the refreshes and of each withdrawal.
fn feed(
    s: &mut Switch,
    from: Instant,
    count: u32,
    reading: &Reading,
) -> (Vec<Duration>, Vec<(Duration, Cause)>, Vec<Duration>) {
    let (mut refreshes, mut withdrawals, mut restores) = (vec![], vec![], vec![]);
    for n in 1..=count {
        let now = from + POLL * n;
        let step = s.on_reading(now, reading);
        if let Some(cause) = step.withdrawn {
            withdrawals.push((now - from, cause));
        }
        if step.write == Some(Write::Refresh) {
            refreshes.push(now - from);
            if s.refreshed(now) {
                restores.push(now - from);
            }
        }
    }
    (refreshes, withdrawals, restores)
}

#[test]
fn a_guard_started_on_a_proxy_not_fit_withdraws_its_node_at_once() {
    let now = Instant::now();
    for first in [unfit("isolated"), gone()] {
        let (s, write, cause) = Switch::start(TIMING, now, &first);
        assert_eq!((write, cause), (Write::Withdraw, Some(Cause::Start)), "{first:?}");
        assert!(s.withdrawn());
    }
}

#[test]
fn a_fit_proxy_is_refreshed_every_quarter_of_the_dead_bound() {
    let origin = Instant::now();
    let mut s = running(origin);
    let (refreshes, withdrawals, _) = feed(&mut s, origin, 10, &Reading::Fit);
    let ms = |v: &[u64]| v.iter().map(|m| Duration::from_millis(*m)).collect::<Vec<_>>();
    assert_eq!(refreshes, ms(&[300, 600, 900]));
    assert!(withdrawals.is_empty() && !s.withdrawn());
}

/// The second unfit reading since the last fit one withdraws the node, once;
/// a single unfit reading between fit ones withdraws nothing.
#[test]
fn a_second_unfit_reading_withdraws_the_node_and_a_lone_one_does_not() {
    let origin = Instant::now();
    let mut s = running(origin);
    assert_eq!(s.on_reading(origin + POLL, &unfit("isolated")), Step::default());
    assert_eq!(s.on_reading(origin + POLL * 2, &Reading::Fit).withdrawn, None);
    assert_eq!(s.on_reading(origin + POLL * 3, &unfit("isolated")), Step::default());
    let step = s.on_reading(origin + POLL * 4, &unfit("isolated"));
    assert_eq!(step.write, Some(Write::Withdraw));
    assert_eq!(step.withdrawn, Some(Cause::Unfit("isolated".to_string())));
    let again = s.on_reading(origin + POLL * 5, &unfit("isolated"));
    assert_eq!(again, Step::default(), "an unfit reading of a withdrawn node writes nothing");
}

/// No fit answer: nothing is written, the kernel's timeout withdraws the
/// node the dead bound after the last refresh, and the switch reads it so
/// from the first reading at or past that instant.
#[test]
fn no_fit_answer_lets_the_last_refresh_run_out() {
    let origin = Instant::now();
    let mut s = running(origin);
    let (refreshes, withdrawals, _) = feed(&mut s, origin, 15, &gone());
    assert!(refreshes.is_empty());
    assert_eq!(withdrawals, vec![(TIMING.dead, Cause::Expired)]);
}

/// A withdrawn node is let out again once the proxy read fit for the whole
/// hold; a reading not fit inside the hold starts it again.
#[test]
fn a_withdrawn_node_is_let_out_after_the_proxy_read_fit_for_the_hold() {
    let origin = Instant::now();
    let mut s = running(origin);
    s.on_reading(origin + POLL, &unfit("external-face"));
    s.on_reading(origin + POLL * 2, &unfit("external-face"));
    assert!(s.withdrawn());
    let start = origin + POLL * 2;
    let (refreshes, _, _) = feed(&mut s, start, 20, &Reading::Fit);
    assert!(refreshes.is_empty(), "2 s of fitness is inside the hold");
    let blip = start + POLL * 21;
    assert_eq!(s.on_reading(blip, &gone()), Step::default());
    let (refreshes, withdrawals, restores) = feed(&mut s, blip, 31, &Reading::Fit);
    assert!(withdrawals.is_empty());
    assert_eq!(restores, vec![TIMING.hold + POLL], "the hold counts from the first fit reading");
    assert_eq!(refreshes.first(), restores.first());
    assert!(!s.withdrawn());
}

/// Refreshes that cannot be written while the proxy reads fit: the last one
/// written runs out and the node reads withdrawn; the proxy fit since the
/// start, the next refresh written once that lasted the hold lets it out.
#[test]
fn refreshes_left_unwritten_run_out_and_a_later_written_one_restores() {
    let origin = Instant::now();
    let mut s = running(origin);
    let mut withdrawn_at = None;
    for n in 1..30 {
        let now = origin + POLL * n;
        let step = s.on_reading(now, &Reading::Fit);
        if let Some(cause) = step.withdrawn {
            withdrawn_at = Some((now - origin, cause));
        }
        if s.withdrawn() {
            assert_eq!(step.write, None, "{:?}: a withdrawn node waits out the hold", now - origin);
        }
    }
    assert_eq!(withdrawn_at, Some((TIMING.dead, Cause::Expired)));
    let now = origin + TIMING.hold;
    assert_eq!(s.on_reading(now, &Reading::Fit).write, Some(Write::Refresh));
    assert!(s.refreshed(now), "the written refresh lets the node out again");
}
