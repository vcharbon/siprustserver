//! **Every fault primitive of the cluster harness bites on the fabric.**
//!
//! A verb that names a directed pair no live stream runs on creates an orphan
//! fault state and changes nothing; a scenario written on it then passes for
//! the wrong reason. Each verb carries a self-test whose assertion is on what
//! the fabric delivered — the captured `Sent`/`Received` frames of the
//! recording, paired by FIFO position — with the replica's state as the
//! secondary witness:
//!
//! - a partition applied **after** the pullers connected delivers nothing
//!   until the heal, then everything it held, in order;
//! - `cut(A, B)` closes the established wire from A's listener to B's puller,
//!   and B's replica of a later put stays stale until `reconnect`;
//! - `delay(A, B, ms)` lands a put on B no earlier than `sent + ms`;
//! - `stall(A, B)` then `resume` delivers everything, in order.
//!
//! Clock: the fabric transit is 1 ms, the harness pumps time in 100 ms
//! chunks. "No earlier than" reads the `at_ms` of the two captures of one
//! frame, never a sleep. The standalone recording carries no global
//! sequencer (`seq = 0`), so "since the fault" is the capture index at the
//! instant the verb ran.

use std::net::SocketAddr;
use std::time::Duration;

use ha_harness::{
    backup_is, cref, frame_summary, CapturedFrame, Direction, Frame, HaCluster, PartitionRole,
    ReplReport,
};

const BAK: PartitionRole = PartitionRole::Backup;

/// The delay the delay cell applies, in ms — chunks above the 100 ms pump
/// granularity, so "early" and "on time" cannot be confused.
const DELAY_MS: u64 = 3_000;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// One frame's two captures on the wire `src → dst`: what `src` handed to
/// `send` (with its capture index, the "since the fault" axis of a recording
/// without a sequencer), and — if it landed — what `dst` took out of `recv`.
struct Transit {
    sent_idx: usize,
    sent: CapturedFrame,
    received: Option<CapturedFrame>,
}

/// The transits of the wire `src → dst`, paired by FIFO position: the fabric
/// is ordered, so the receiver's sequence is a prefix of the sender's. A
/// receive that does not match the send at its position is a reordering, and
/// panics here.
fn transits(report: &ReplReport, src: SocketAddr, dst: SocketAddr) -> Vec<Transit> {
    let sent: Vec<(usize, &CapturedFrame)> = report
        .frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f.from == src && f.to == dst && f.dir == Direction::Sent)
        .collect();
    let received: Vec<&CapturedFrame> = report
        .frames
        .iter()
        .filter(|f| f.from == dst && f.to == src && f.dir == Direction::Received)
        .collect();
    assert!(
        received.len() <= sent.len(),
        "{src} → {dst}: {} frames received, only {} sent",
        received.len(),
        sent.len()
    );
    sent.iter()
        .enumerate()
        .map(|(i, (idx, s))| {
            let r = received.get(i).copied();
            if let Some(r) = r {
                assert_eq!(
                    r.frame,
                    s.frame,
                    "{src} → {dst}: frame #{i} was received out of order (sent {}, received {})",
                    frame_summary(&s.frame),
                    frame_summary(&r.frame)
                );
            }
            Transit { sent_idx: *idx, sent: (*s).clone(), received: r.cloned() }
        })
        .collect()
}

/// The client addresses `listener` serves a stream to — every puller that
/// opened one.
fn clients_of(report: &ReplReport, listener: SocketAddr) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = report
        .frames
        .iter()
        .filter(|f| f.from == listener && f.dir == Direction::Sent)
        .map(|f| f.to)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every frame received from `listener` on any stream it serves, captured at
/// index `since` or later.
fn received_from(report: &ReplReport, listener: SocketAddr, since: usize) -> Vec<CapturedFrame> {
    report
        .frames
        .iter()
        .skip(since)
        .filter(|f| f.dir == Direction::Received && f.to == listener)
        .cloned()
        .collect()
}

/// Every `Data` frame `listener` handed to `send`, captured at index `since`
/// or later.
fn data_sent_by(report: &ReplReport, listener: SocketAddr, since: usize) -> Vec<CapturedFrame> {
    report
        .frames
        .iter()
        .skip(since)
        .filter(|f| f.dir == Direction::Sent && f.from == listener)
        .filter(|f| matches!(f.frame, Frame::Data { .. }))
        .cloned()
        .collect()
}

fn first_summary(frames: &[CapturedFrame]) -> String {
    frames
        .first()
        .map(|f| format!("{} → {}: {}", f.from, f.to, frame_summary(&f.frame)))
        .unwrap_or_default()
}

/// A two-node cluster at steady state with one converged call, so the
/// pullers' streams are established before any verb runs.
async fn steady_pair() -> (HaCluster, SocketAddr) {
    let mut cl = HaCluster::new(&["A", "B"]).await;
    cl.advance(ms(200)).await;
    let c0 = cref("A", "0");
    cl.put("A", &c0, b"seed".to_vec(), 1, 0, &backup_is("B")).await;
    cl.advance(ms(300)).await;
    assert_eq!(cl.node("B").get(BAK, "A", &c0).await.as_deref(), Some(&b"seed"[..]));
    let a = cl.node("A").addr();
    assert!(!clients_of(&cl.report(), a).is_empty(), "B's pullers are connected to A");
    (cl, a)
}

/// **A partition applied after the pullers connected delivers nothing until
/// the heal.** A puts while the pair is cut: B receives no frame from A's
/// listener and its replica stays at the pre-cut version; the heal releases
/// what the cut held, in order, and B converges.
#[tokio::test(start_paused = true)]
async fn partition_after_connect_delivers_nothing_until_heal() {
    let (mut cl, a) = steady_pair().await;
    let c = cref("A", "1");
    cl.put("A", &c, b"before".to_vec(), 1, 0, &backup_is("B")).await;
    cl.advance(ms(300)).await;
    assert_eq!(cl.node("B").get(BAK, "A", &c).await.as_deref(), Some(&b"before"[..]));

    let since = cl.report().frames.len();
    cl.partition("A", "B");
    cl.put("A", &c, b"during".to_vec(), 2, 0, &backup_is("B")).await;
    cl.advance(secs(2)).await;

    let report = cl.report();
    let staged = data_sent_by(&report, a, since);
    assert!(!staged.is_empty(), "A flushed the put behind the cut");
    let crossed = received_from(&report, a, since);
    assert!(
        crossed.is_empty(),
        "the partition delivered {} frame(s) from A's listener; the first: {}",
        crossed.len(),
        first_summary(&crossed),
    );
    assert_eq!(
        cl.node("B").get(BAK, "A", &c).await.as_deref(),
        Some(&b"before"[..]),
        "B's replica stays at the pre-cut version while the cut lasts",
    );

    cl.heal("A", "B");
    cl.advance(secs(2)).await;
    let report = cl.report();
    let mut landed = 0;
    for client in clients_of(&report, a) {
        for t in transits(&report, a, client) {
            if t.sent_idx >= since {
                assert!(
                    t.received.is_some(),
                    "a frame staged behind the cut never landed after the heal: {}",
                    frame_summary(&t.sent.frame)
                );
                landed += 1;
            }
        }
    }
    assert!(landed >= staged.len(), "every frame the cut held landed ({landed})");
    assert_eq!(
        cl.node("B").get(BAK, "A", &c).await.as_deref(),
        Some(&b"during"[..]),
        "B converges once the cut heals",
    );
}

/// **`cut(A, B)` closes the established wire from A to B.** A put A writes
/// after the cut is never received by B's puller — the replica stays stale —
/// until `reconnect(A, B)` lets a fresh stream carry it.
#[tokio::test(start_paused = true)]
async fn cut_closes_the_established_wire_until_reconnect() {
    let (mut cl, a) = steady_pair().await;
    let since = cl.report().frames.len();
    cl.cut("A", "B");
    cl.advance(ms(200)).await;
    let c = cref("A", "2");
    cl.put("A", &c, b"after-cut".to_vec(), 1, 0, &backup_is("B")).await;
    cl.advance(secs(2)).await;

    let report = cl.report();
    let crossed = received_from(&report, a, since);
    assert!(
        crossed.is_empty(),
        "the cut let {} frame(s) from A's listener through; the first: {}",
        crossed.len(),
        first_summary(&crossed),
    );
    assert!(
        cl.node("B").get(BAK, "A", &c).await.is_none(),
        "B's replica of the post-cut put stays absent while the wire is cut",
    );

    cl.reconnect("A", "B");
    cl.advance(secs(3)).await;
    assert_eq!(
        cl.node("B").get(BAK, "A", &c).await.as_deref(),
        Some(&b"after-cut"[..]),
        "the post-cut put lands once the pair reconnects",
    );
    assert!(
        !received_from(&cl.report(), a, since).is_empty(),
        "frames from A's listener reach B again after the reconnect",
    );
}

/// **`delay(A, B, ms)` lands a put no earlier than `ms` later.** Every frame
/// A's listener sends after the verb is received by B's puller at
/// `sent + ms` or later; at every pump chunk before that instant nothing has
/// landed and B's replica is absent.
#[tokio::test(start_paused = true)]
async fn delay_lands_a_put_no_earlier_than_the_delay() {
    let (mut cl, a) = steady_pair().await;
    let since = cl.report().frames.len();
    cl.delay("A", "B", DELAY_MS);
    let c = cref("A", "3");
    cl.put("A", &c, b"delayed".to_vec(), 1, 0, &backup_is("B")).await;
    cl.advance(ms(100)).await;
    let sent_at = data_sent_by(&cl.report(), a, since)
        .iter()
        .map(|f| f.at_ms)
        .min()
        .expect("A flushed the put after the delay was applied");
    let lands_at = sent_at + DELAY_MS as i64;

    while cl.now_ms() + 200 < lands_at {
        cl.advance(ms(100)).await;
        assert!(cl.now_ms() < lands_at, "the probe stays before the delivery instant");
        let early = received_from(&cl.report(), a, since);
        assert!(
            early.is_empty(),
            "at {} ms (put sent at {sent_at} ms, delay {DELAY_MS} ms) {} frame(s) from A's \
             listener already landed; the first: {}",
            cl.now_ms(),
            early.len(),
            first_summary(&early),
        );
        assert!(
            cl.node("B").get(BAK, "A", &c).await.is_none(),
            "at {} ms B's replica is still absent",
            cl.now_ms()
        );
    }

    cl.advance(secs(1)).await;
    let report = cl.report();
    let mut checked = 0;
    for client in clients_of(&report, a) {
        for t in transits(&report, a, client) {
            let Some(r) = &t.received else { continue };
            if t.sent_idx < since {
                continue;
            }
            assert!(
                r.at_ms >= t.sent.at_ms + DELAY_MS as i64,
                "a frame on {a} → {client} landed {} ms after it was sent; the delay is \
                 {DELAY_MS} ms: {}",
                r.at_ms - t.sent.at_ms,
                frame_summary(&t.sent.frame),
            );
            checked += 1;
        }
    }
    assert!(checked >= 1, "at least one delayed frame landed ({checked} checked)");
    assert_eq!(
        cl.node("B").get(BAK, "A", &c).await.as_deref(),
        Some(&b"delayed"[..]),
        "the delayed put landed",
    );
}

/// **`stall(A, B)` then `resume` delivers everything, in order.** Three puts
/// A writes under the stall reach B's puller only after the resume, in the
/// order they were sent, and B holds all three.
#[tokio::test(start_paused = true)]
async fn stall_then_resume_delivers_everything_in_order() {
    let (mut cl, a) = steady_pair().await;
    let since = cl.report().frames.len();
    cl.stall("A", "B");
    let refs: Vec<String> = (1..=3).map(|i| cref("A", &format!("s{i}"))).collect();
    for (i, c) in refs.iter().enumerate() {
        cl.put("A", c, format!("v{i}").into_bytes(), 1, 0, &backup_is("B")).await;
    }
    cl.advance(secs(2)).await;

    let report = cl.report();
    let staged = data_sent_by(&report, a, since);
    assert_eq!(staged.len(), 3, "A flushed the three puts under the stall");
    let crossed = received_from(&report, a, since);
    assert!(
        crossed.is_empty(),
        "the stall let {} frame(s) from A's listener through; the first: {}",
        crossed.len(),
        first_summary(&crossed),
    );
    for c in &refs {
        assert!(cl.node("B").get(BAK, "A", c).await.is_none(), "{c} is held back by the stall");
    }

    cl.resume("A", "B");
    cl.advance(secs(1)).await;
    let report = cl.report();
    let mut landed = 0;
    for client in clients_of(&report, a) {
        for t in transits(&report, a, client) {
            if t.sent_idx >= since {
                assert!(
                    t.received.is_some(),
                    "a frame held by the stall never landed after the resume: {}",
                    frame_summary(&t.sent.frame)
                );
                landed += 1;
            }
        }
    }
    assert!(landed >= 3, "every held frame landed ({landed})");
    for (i, c) in refs.iter().enumerate() {
        assert_eq!(
            cl.node("B").get(BAK, "A", c).await.as_deref(),
            Some(format!("v{i}").as_bytes()),
            "{c} landed on the resume",
        );
    }
}
