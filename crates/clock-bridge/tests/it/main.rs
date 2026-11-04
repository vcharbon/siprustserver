//! The bridge on a paused clock against work that takes real time: delivery at
//! the virtual instant whatever the real order, the clock held only while a due
//! result is late, a stall past the cap, and plain delivery on the real clock.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clock_bridge::{ClockMode, EnvFault, RealSide, Undelivered};
use tokio::time::{sleep, Instant};

const LATENCY: Duration = Duration::from_millis(10);
const CAP: Duration = Duration::from_millis(500);

/// Work standing for an external process that answers after `ms` of real time.
async fn answers_after(ms: u64) -> u64 {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    ms
}

#[tokio::test(start_paused = true)]
async fn a_late_result_holds_the_paused_clock_until_it_is_delivered() {
    let side = RealSide::new(ClockMode::Paused, CAP);
    let start = Instant::now();
    let neighbour = tokio::spawn(async move {
        sleep(Duration::from_millis(5)).await;
        start.elapsed()
    });
    let pending = side.start("slow", answers_after(30));
    let out = tokio::time::timeout(Duration::from_secs(2), pending.deliver_at(start + LATENCY))
        .await
        .expect("the result, not the 2 s timeout: the clock was held");
    assert_eq!(out, Ok(30));
    assert_eq!(start.elapsed(), LATENCY, "delivered at its virtual instant");
    assert_eq!(
        neighbour.await.unwrap(),
        Duration::from_millis(5),
        "virtual time ran until the instant"
    );
    assert!(side.faults().faults().is_empty());
}

#[tokio::test(start_paused = true)]
async fn deliveries_follow_the_virtual_order_whatever_the_real_order() {
    let side = RealSide::new(ClockMode::Paused, CAP);
    let start = Instant::now();
    let log = Arc::new(Mutex::new(Vec::new()));
    // Real arrival order C, B, A; virtual delivery order A and C at 10 ms, B at 11 ms.
    let mut calls = Vec::new();
    for (name, at, real_ms) in [("A", 0u64, 30u64), ("C", 0, 5), ("B", 1, 1)] {
        let (side, log) = (side.clone(), log.clone());
        calls.push(tokio::spawn(async move {
            sleep(Duration::from_millis(at)).await;
            let issued = Instant::now();
            side.start(name, answers_after(real_ms)).deliver_at(issued + LATENCY).await.unwrap();
            log.lock().unwrap().push((name, start.elapsed()));
        }));
    }
    for call in calls {
        call.await.unwrap();
    }
    let mut seen = log.lock().unwrap().clone();
    assert_eq!(seen.pop(), Some(("B", Duration::from_millis(11))), "B last, at its own instant");
    seen.sort();
    assert_eq!(seen, [("A", LATENCY), ("C", LATENCY)], "a tie is delivered at the same instant");
}

#[tokio::test(start_paused = true)]
async fn concurrent_exchanges_each_land_on_their_instant() {
    let side = RealSide::new(ClockMode::Paused, CAP);
    let mut calls = Vec::new();
    for i in 0..100u64 {
        let side = side.clone();
        calls.push(tokio::spawn(async move {
            sleep(Duration::from_millis(i * 3)).await;
            let issued = Instant::now();
            side.start(format!("call {i}"), answers_after(1 + i % 8))
                .deliver_at(issued + LATENCY)
                .await
                .unwrap();
            let first = issued.elapsed();
            sleep(Duration::from_secs(5)).await;
            let issued = Instant::now();
            side.start(format!("call {i} again"), answers_after(1 + i * 3 % 8))
                .deliver_at(issued + LATENCY)
                .await
                .unwrap();
            (first, issued.elapsed())
        }));
    }
    for call in calls {
        assert_eq!(call.await.unwrap(), (LATENCY, LATENCY));
    }
    assert!(side.faults().faults().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_result_past_the_cap_is_a_stall_and_its_arrival_is_late() {
    let side = RealSide::new(ClockMode::Paused, Duration::from_millis(50));
    let start = Instant::now();
    let out = side.start("stuck", answers_after(200)).deliver_at(start + LATENCY).await;
    assert_eq!(out, Err(Undelivered::Stall));
    assert_eq!(start.elapsed(), LATENCY, "the clock stayed held through the stall");
    std::thread::sleep(Duration::from_millis(300));
    let faults = side.faults().faults();
    assert!(matches!(&faults[0], EnvFault::Stall { label, .. } if label == "stuck"), "{faults:?}");
    assert!(matches!(&faults[1], EnvFault::Late { label, .. } if label == "stuck"), "{faults:?}");
}

#[tokio::test]
async fn on_the_real_clock_a_result_is_delivered_on_arrival() {
    let side = RealSide::new(ClockMode::Real, CAP);
    let start = std::time::Instant::now();
    let far = Instant::now() + Duration::from_secs(60);
    assert_eq!(side.start("real", answers_after(20)).deliver_at(far).await, Ok(20));
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(20) && took < Duration::from_secs(1), "{took:?}");
}
