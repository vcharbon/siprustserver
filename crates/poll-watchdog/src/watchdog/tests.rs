//! Which threads register: real runtimes, real threads.

use std::sync::mpsc;

use super::*;

fn watched_runtime(watchdog: &Watchdog, workers: usize) -> tokio::runtime::Runtime {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(workers).enable_all();
    watchdog.install(&mut builder);
    builder.build().expect("runtime builds")
}

#[test]
fn a_blocking_pool_thread_is_never_registered() {
    let watchdog = Watchdog::new();
    let runtime = watched_runtime(&watchdog, 2);
    let (tids, blocking) = runtime.block_on(async {
        let mut tasks = Vec::new();
        for _ in 0..32 {
            tasks.push(tokio::spawn(async { schedstat::own_tid().expect("tid") }));
        }
        let mut tids = Vec::new();
        for task in tasks {
            tids.push(task.await.expect("task runs"));
        }
        let blocking = tokio::task::spawn_blocking(|| {
            // A blocking thread that drives a future of its own still runs
            // no task poll.
            let handle = tokio::runtime::Handle::current();
            handle.block_on(async { tokio::task::yield_now().await });
            schedstat::own_tid().expect("tid")
        })
        .await
        .expect("blocking task runs");
        (tids, blocking)
    });
    let registered = watchdog.registered_threads();
    assert!(!registered.is_empty(), "a worker that polled registered");
    assert!(tids.iter().all(|t| registered.contains(t)), "every polling thread registered");
    assert!(!registered.contains(&blocking), "the blocking thread {blocking} is in {registered:?}");
    runtime.shutdown_timeout(Duration::from_secs(1));
}

#[test]
fn a_thread_polling_for_another_watchdog_stays_with_its_first() {
    let first = Watchdog::new();
    let second = Watchdog::new();
    let (done, wait) = mpsc::channel();
    std::thread::spawn(move || {
        for watchdog in [&first, &second] {
            let mut builder = tokio::runtime::Builder::new_current_thread();
            watchdog.install(&mut builder);
            let runtime = builder.build().expect("runtime builds");
            runtime.block_on(async { tokio::spawn(async {}).await.expect("task runs") });
        }
        let _ = done.send((first.registered_threads().len(), second.registered_threads().len()));
    });
    assert_eq!(wait.recv().expect("thread reports"), (1, 0));
}

#[test]
fn an_exited_thread_leaves_the_live_threads() {
    let watchdog = Watchdog::new();
    let runtime = watched_runtime(&watchdog, 1);
    runtime.block_on(async { tokio::spawn(async {}).await.expect("task runs") });
    assert_eq!(watchdog.registered_threads().len(), 1);
    runtime.shutdown_timeout(Duration::from_secs(1));
    // The slot is marked gone by the thread's own exit, after the runtime
    // let go of it.
    let gone = std::time::Instant::now();
    while !watchdog.registered_threads().is_empty() && gone.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(watchdog.registered_threads().is_empty());
}

#[test]
fn the_read_period_is_a_second_or_half_the_threshold() {
    assert_eq!(period_for(Duration::from_secs(10)), READ_PERIOD);
    assert_eq!(period_for(Duration::from_millis(200)), Duration::from_millis(100));
}

#[test]
fn a_started_watchdog_keeps_running_on_a_second_start() {
    let watchdog = Watchdog::new();
    watchdog.start(Duration::from_secs(600)).expect("this host reads schedstat");
    watchdog.start(Duration::from_secs(600)).expect("a second start is no failure");
}
