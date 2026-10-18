use super::*;
use repl_net::{Frame, Watermark};
use repl_net_testkit::CapturedFrame;

fn addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}").parse().unwrap()
}

/// Capture a frame `a → b` at `at_ms`. `dir` records on whichever endpoint
/// observed it: a `Sent` records on the sender (`from=a`), a `Received` on the
/// receiver — and because each side records its OWN local addr as `from`, a
/// receipt of `a → b` is captured with `from=b, to=a` (orientation swapped),
/// exactly as the real recorder does. The helper takes the LOGICAL direction
/// (`a` is always the sender) and swaps for `Received` so tests read naturally.
fn cap(at_ms: i64, a: u16, b: u16, dir: Direction, frame: Frame) -> CapturedFrame {
    let (from, to) = match dir {
        Direction::Sent => (addr(a), addr(b)),
        Direction::Received => (addr(b), addr(a)),
    };
    CapturedFrame { at_ms, seq: 0, from, to, dir, frame }
}

fn noop() -> Frame {
    Frame::Noop { at: Watermark::new(1, 2) }
}

#[test]
fn sent_with_a_received_twin_is_delivered_without_one_is_lost() {
    // b1(9401) sends two identical Noops to b2(9402): the first is delivered
    // (b2 recv'd it), the second is lost (b2 had crashed — no recv twin).
    let frames = vec![
        cap(100, 9401, 9402, Direction::Sent, noop()),
        cap(101, 9401, 9402, Direction::Received, noop()), // twin of the first
        cap(700, 9401, 9402, Direction::Sent, noop()),     // no twin → lost
    ];
    let delivered = delivered_sent(&frames);
    assert!(delivered.contains(&0), "first send had a recv twin");
    assert!(!delivered.contains(&2), "second send was lost (no recv twin)");
}

#[test]
fn a_later_redelivery_does_not_rescue_an_earlier_dropped_send() {
    // The crux of the kill report: the SAME (b1→b2, frame) is sent at t=170
    // (lost, peer dead) then re-sent at t=230 after reboot (delivered). The
    // single receipt at t=231 must pair with the t=230 send — NOT retroactively
    // mark the t=170 drop as delivered.
    let frames = vec![
        cap(170, 9401, 9402, Direction::Sent, noop()), // dropped (peer dead)
        cap(230, 9401, 9402, Direction::Sent, noop()), // re-sent post-reboot
        cap(231, 9401, 9402, Direction::Received, noop()), // pairs with t=230
    ];
    let delivered = delivered_sent(&frames);
    assert!(!delivered.contains(&0), "the t=170 drop stays lost");
    assert!(delivered.contains(&1), "the t=230 re-send was delivered");
}

#[test]
fn group_loss_count_equals_sent_minus_received() {
    // Six identical Noops sent, four received → exactly two marked lost
    // (which two among identical frames is immaterial; the COUNT is the claim).
    let mut frames = Vec::new();
    for i in 0..6 {
        frames.push(cap(100 + i * 10, 9401, 9402, Direction::Sent, noop()));
    }
    for i in 0..4 {
        frames.push(cap(105 + i * 10, 9401, 9402, Direction::Received, noop()));
    }
    let delivered = delivered_sent(&frames);
    let lost = (0..6).filter(|i| !delivered.contains(i)).count();
    assert_eq!(lost, 2, "6 sent − 4 received = 2 lost");
}
