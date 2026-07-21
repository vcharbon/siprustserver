//! Paused-clock tests for the re-anchoring CPS [`Governor`] and the live
//! `POST /rate` HTTP surface. The governor tests count admission slots over a
//! window on the frozen `tokio::time` clock (no real network, no SUT) — the pure
//! scheduling contract: cadence matches cps, a mid-run rate cut re-anchors with NO
//! catch-up burst, and `cps == 0` pauses / a raise resumes cleanly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use loadgen::{Governor, RateHandle};

/// Spawn a task that pulls slots from a fresh [`Governor`] over `window`, bumping a
/// shared counter per admitted slot. Returns `(rate_handle, counter, join)` so the
/// test can re-target the rate mid-run and read the running count.
fn spawn_governor(
    cps: f64,
    window: Duration,
) -> (RateHandle, Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
    let rate = RateHandle::new(cps);
    let count = Arc::new(AtomicU64::new(0));
    let (r, c) = (rate.clone(), count.clone());
    let join = tokio::spawn(async move {
        let mut gov = Governor::new(r, window);
        while gov.next_slot().await.is_some() {
            c.fetch_add(1, Ordering::Relaxed);
        }
    });
    (rate, count, join)
}

/// (a) Spawn cadence matches cps: over a 1 s window at 10 cps the governor admits
/// ~10 slots (the fixed grid fires at 0,100,200,…,900 ms — 10 slots — then the
/// window closes).
#[tokio::test(start_paused = true)]
async fn cadence_matches_cps() {
    let (_rate, count, join) = spawn_governor(10.0, Duration::from_secs(1));
    // Let the whole window elapse on the frozen clock (tokio auto-advances to each
    // timer while the task parks).
    join.await.unwrap();
    let n = count.load(Ordering::Relaxed);
    assert_eq!(n, 10, "10 cps over 1 s should admit exactly 10 slots, got {n}");
}

/// (b) A mid-run rate CUT re-anchors with NO catch-up burst. Run 1 s at 100 cps
/// (the fast grid, 10 ms period), then cut to 10 cps for the next 1 s. The second
/// window must admit ~10 slots at the NEW rate — NOT a burst of the ~90 past-due
/// slots the old 100 cps grid would have laid down had it not re-anchored.
#[tokio::test(start_paused = true)]
async fn rate_cut_reanchors_without_catchup_burst() {
    // Long window; we drive it in two halves and cut the rate at the boundary.
    let (rate, count, join) = spawn_governor(100.0, Duration::from_secs(2));

    // First 1 s at 100 cps → ~100 slots.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let after_fast = count.load(Ordering::Relaxed);
    assert!(
        (95..=105).contains(&after_fast),
        "≈100 slots in the first second at 100 cps, got {after_fast}"
    );

    // CUT to 10 cps. Re-anchoring means the next second admits ~10 — the old grid's
    // backlog is discarded, so there is no burst.
    rate.set(10.0);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    join.await.unwrap();

    let total = count.load(Ordering::Relaxed);
    let after_cut = total - after_fast;
    assert!(
        after_cut <= 15,
        "a rate cut fired a catch-up burst: {after_cut} slots in the second second \
         at 10 cps (a re-anchored grid admits ~10, never the ~90 stale slots)"
    );
    assert!(after_cut >= 8, "the cut rate stalled admission entirely: only {after_cut} slots");
}

/// (c) `cps == 0` PAUSES new-call admission and a later RAISE resumes cleanly, with
/// no burst for the paused interval. Start paused, hold for a while (zero slots),
/// then raise to 20 cps for a second (~20 slots).
#[tokio::test(start_paused = true)]
async fn zero_pauses_and_raise_resumes() {
    let (rate, count, join) = spawn_governor(0.0, Duration::from_secs(3));

    // Paused for the first second: NO slots admitted.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(count.load(Ordering::Relaxed), 0, "cps=0 must admit no calls");

    // Raise to 20 cps. Resumes cleanly — the paused second lays down no backlog, so
    // the next second admits ~20 (a re-anchored grid), not a burst.
    rate.set(20.0);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let after_raise = count.load(Ordering::Relaxed);
    assert!(
        (16..=24).contains(&after_raise),
        "a raise from pause should resume at ~20 cps (no paused-interval burst), got {after_raise}"
    );

    join.await.unwrap();
}

/// The `POST /rate` HTTP surface end-to-end against the running metrics server:
/// GET returns the current target, POST re-targets it (validated + clamped, echoed
/// back), a bad value is a 400, and the live [`RateHandle`] reflects the applied
/// value. (Real loopback TCP — the same hand-rolled server the smoke suite drives.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_rate_retargets_the_running_server() {
    use loadgen::serve_metrics_on;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let rate = RateHandle::new(10.0);
    // Port-0 bind: a fixed port can collide with an unrelated host listener,
    // and the retry-connect loop below would then talk to the foreign server.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap();
    let render: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(|| "render-body\n".to_string());
    let srv_rate = rate.clone();
    tokio::spawn(async move {
        let _ = serve_metrics_on(listener, render, None, Some(srv_rate)).await;
    });

    // Retry-connect + one HTTP round-trip helper.
    async fn http(bind: std::net::SocketAddr, req: &str) -> String {
        let mut stream = loop {
            match tokio::net::TcpStream::connect(bind).await {
                Ok(s) => break s,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    // GET /rate → the initial target.
    let got = http(bind, "GET /rate HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(got.contains("200 OK"), "GET /rate should 200: {got}");
    assert!(got.contains("10"), "GET /rate should echo the initial 10 cps: {got}");

    // POST /rate?cps=42.5 → applied + echoed; the shared handle reflects it.
    let posted = http(bind, "POST /rate?cps=42.5 HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(posted.contains("200 OK"), "POST /rate should 200: {posted}");
    assert!(posted.contains("cps=42.5"), "POST /rate should echo the applied value: {posted}");
    assert_eq!(rate.cps(), 42.5, "the live handle reflects the POSTed rate");

    // POST /rate?cps=0 → pauses (applied 0).
    let paused = http(bind, "POST /rate?cps=0 HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(paused.contains("cps=0"), "POST cps=0 pauses: {paused}");
    assert!(rate.is_paused(), "cps=0 pauses admission");

    // A negative value clamps to 0 (still a 200 with the clamped echo).
    let neg = http(bind, "POST /rate?cps=-5 HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(neg.contains("cps=0"), "a negative cps clamps to 0: {neg}");

    // A malformed / missing cps is a 400 (never a silent no-op).
    let bad = http(bind, "POST /rate?cps=abc HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(bad.contains("400 Bad Request"), "a bad cps should 400: {bad}");
    let missing = http(bind, "POST /rate HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(missing.contains("400 Bad Request"), "a missing cps should 400: {missing}");
    // The rate is unchanged by the rejected requests (still paused from cps=0).
    assert!(rate.is_paused(), "a rejected /rate must not move the target");
}
