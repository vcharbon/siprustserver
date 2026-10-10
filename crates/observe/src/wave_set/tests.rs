use super::*;
use crate::wave::Edge;

/// Captured lines, so the tests assert on the aggregation rather than on a
/// subscriber.
#[derive(Default)]
struct Captured(Mutex<Vec<(String, Edge, u64)>>);

fn capturing() -> (Arc<WaveSet>, Arc<Captured>) {
    let seen = Arc::new(Captured::default());
    let sink = seen.clone();
    let set = WaveSet::new(move |key, report| {
        sink.0.lock().unwrap().push((key.to_string(), report.edge, report.tally.total()));
    });
    (set, seen)
}

/// The headline contract: a 5000-event burst on one key produces a handful
/// of lines — rising, periodic summaries, falling — never one per event.
#[tokio::test(start_paused = true)]
async fn a_five_thousand_event_burst_produces_a_handful_of_lines() {
    let (set, seen) = capturing();

    for _ in 0..5_000 {
        set.record("w0", "hydrated", 1);
    }
    // Let the episode's driver task arm its timer BEFORE moving the clock
    // (it was spawned, not yet polled — advancing first would arm it from
    // the later instant), then advance exactly to the idle window that
    // closes the episode.
    tokio::task::yield_now().await;
    tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
    tokio::task::yield_now().await;

    let lines = seen.0.lock().unwrap().clone();
    assert!(lines.len() <= 4, "5000 events must not print 5000 lines: {lines:?}");
    assert_eq!(lines.first().map(|l| l.1), Some(Edge::Rising));
    let last = lines.last().expect("an episode always closes");
    assert_eq!(last.1, Edge::Falling);
    assert_eq!(last.2, 5_000, "the falling edge carries the episode totals");
    assert!(!set.is_active());
}

/// Keys are independent episodes: one peer's burst does not fold into
/// another's totals, and each closes on its own quiet.
#[tokio::test(start_paused = true)]
async fn keys_are_independent_episodes() {
    let (set, seen) = capturing();
    set.record("w0", "hydrated", 1);
    set.record("w1", "hydrated", 1);
    set.recovered("w0");
    assert!(set.is_active(), "w1's episode is still burning");
    set.recovered_all();
    assert!(!set.is_active(), "every episode has now recovered");

    tokio::task::yield_now().await;
    tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
    tokio::task::yield_now().await;

    let lines = seen.0.lock().unwrap().clone();
    assert_eq!(lines[0], ("w0".into(), Edge::Rising, 1));
    assert_eq!(lines[1], ("w1".into(), Edge::Rising, 1));
    let falling: Vec<_> = lines[2..].iter().map(|l| (l.0.clone(), l.1, l.2)).collect();
    assert_eq!(falling.len(), 2, "one falling edge each: {lines:?}");
    assert!(falling.iter().all(|l| l.1 == Edge::Falling && l.2 == 1));
}

/// The headline flap contract at the set level: a source alternating
/// failure/recovery at its cap owes ONE rising edge and ONE falling edge,
/// not a pair per event — and spawns ONE driver task, not one per event.
#[tokio::test(start_paused = true)]
async fn a_flapping_key_stays_one_episode() {
    let (set, seen) = capturing();

    // First shed opens the episode; yield so its driver arms its cadence
    // timer at t=0 rather than from a post-advance instant.
    set.record("cps", "shed", 1);
    tokio::task::yield_now().await;

    // 1999 further admit/reject flaps, 1 ms apart — t = 1.999 s, well
    // inside the first cadence.
    for _ in 0..1_999 {
        set.recovered_all();
        tokio::time::advance(Duration::from_millis(1)).await;
        set.record("cps", "shed", 1);
    }
    let during = seen.0.lock().unwrap().clone();
    assert_eq!(during.len(), 1, "a flap owes one rising edge only: {during:?}");
    assert_eq!(during[0].1, Edge::Rising);

    // The offered load stops. The cadence deadline lands first (summary),
    // then the idle window that ends the episode — one advance each.
    tokio::time::advance(Duration::from_millis(3_001)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(DEFAULT_SUMMARY_EVERY).await;
    tokio::task::yield_now().await;

    let lines = seen.0.lock().unwrap().clone();
    assert_eq!(lines.len(), 3, "rising + one summary + falling: {lines:?}");
    assert_eq!(lines[1].1, Edge::Summary);
    assert_eq!(lines[2].1, Edge::Falling);
    assert_eq!(lines[2].2, 2_000, "the falling edge totals every shed");
    assert!(!set.is_active());
}

/// Lines of a payload-carrying set: key, edge, the episode's payload.
type PayloadLines = Arc<Mutex<Vec<(String, Edge, &'static str)>>>;

fn capturing_payloads() -> (Arc<WaveSet<&'static str>>, PayloadLines) {
    let seen = PayloadLines::default();
    let sink = seen.clone();
    let set = WaveSet::with_payload(move |key, report, payload: &&'static str| {
        sink.lock().unwrap().push((key.to_string(), report.edge, payload));
    });
    (set, seen)
}

/// A record that looked its key's slot up just before the driver closed
/// that episode does not land on the closed episode: it is refused there
/// and opens the key's next episode, which owns its payload and its slot.
#[tokio::test(start_paused = true)]
async fn a_record_racing_the_close_opens_the_next_episode_with_its_own_payload() {
    let (set, seen) = capturing_payloads();
    set.record_with("k", "events", 1, "old");
    tokio::task::yield_now().await;

    let raced = set.slot_for("k").expect("the key is open");
    tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
    tokio::task::yield_now().await;
    let mut payload = Some("new");
    assert!(
        !set.record_into("k", &raced, "events", 1, &mut payload),
        "a closed episode refuses the racing event",
    );
    set.record_with("k", "events", 1, payload.expect("a refused event keeps its payload"));
    set.record_with("k", "events", 1, "newer");
    tokio::task::yield_now().await;
    tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
    tokio::task::yield_now().await;

    let lines = seen.lock().unwrap().clone();
    let k = || "k".to_string();
    assert_eq!(
        lines,
        [
            (k(), Edge::Rising, "old"),
            (k(), Edge::Falling, "old"),
            (k(), Edge::Rising, "new"),
            (k(), Edge::Falling, "newer"),
        ],
    );
    assert!(!set.is_active());
    assert!(set.waves.lock().unwrap().is_empty(), "a closed episode frees its key");
}

/// An event recorded from the falling line itself — the earliest a record
/// can follow a close — opens a new episode with its own payload.
#[tokio::test(start_paused = true)]
async fn a_record_from_the_falling_line_opens_a_new_episode() {
    let seen = PayloadLines::default();
    let sink = seen.clone();
    let this: Arc<std::sync::OnceLock<std::sync::Weak<WaveSet<&'static str>>>> = Arc::default();
    let reenter = this.clone();
    let set = WaveSet::with_payload(move |key, report, payload: &&'static str| {
        sink.lock().unwrap().push((key.to_string(), report.edge, payload));
        if report.edge == Edge::Falling && *payload == "old" {
            if let Some(set) = reenter.get().and_then(std::sync::Weak::upgrade) {
                set.record_with(key, "events", 1, "new");
            }
        }
    });
    let _ = this.set(Arc::downgrade(&set));

    set.record_with("k", "events", 1, "old");
    tokio::task::yield_now().await;
    tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
    tokio::task::yield_now().await;

    let lines = seen.lock().unwrap().clone();
    let k = || "k".to_string();
    assert_eq!(
        lines,
        [(k(), Edge::Rising, "old"), (k(), Edge::Falling, "old"), (k(), Edge::Rising, "new")],
    );
    assert!(set.is_active(), "the new episode is burning");
}

/// The key bound holds against an unbounded key source, and a key that has
/// actually fallen frees its slot.
#[tokio::test(start_paused = true)]
async fn open_keys_are_bounded() {
    let (set, seen) = capturing();
    for i in 0..(MAX_KEYS + 50) {
        set.record(&format!("name-{i}"), "resolve_failed", 1);
    }
    assert_eq!(seen.0.lock().unwrap().len(), MAX_KEYS);

    // A recovery alone does NOT free the slot — quiet does.
    set.recovered("name-0");
    set.record("fresh", "resolve_failed", 1);
    assert_eq!(
        seen.0.lock().unwrap().len(),
        MAX_KEYS,
        "an armed-but-open key still holds its slot",
    );

    tokio::task::yield_now().await;
    tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
    tokio::task::yield_now().await;
    set.record("fresh", "resolve_failed", 1);
    assert!(seen.0.lock().unwrap().iter().any(|l| l.0 == "fresh"), "a freed slot admits a new key",);
}
