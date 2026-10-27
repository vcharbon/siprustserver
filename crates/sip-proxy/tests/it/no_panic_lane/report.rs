//! A worker's load report as the proxy reads it (ADR-0050 §6): each line an
//! `X-Overload` value applied to the load observer a probe interval after
//! the last, or noted missing, and read as an `X-Readiness` value; then the
//! observer's bucket, band and stale sweep. A payload that reads
//! holds what the AIMD step relies on: `elu` and `gc` in `0..=1`, `adm`
//! finite and not negative.

use sip_message::worker_report::Readiness;
use sip_proxy::load_observer::{parse_x_overload_header, LoadObserverConfig, WorkerLoadObserver};

/// The interval between two answers of the worker.
const INTERVAL_MS: i64 = 1_000;

/// One case: the report's lines, in order.
pub fn case(report: &[u8]) {
    let observer = WorkerLoadObserver::new(LoadObserverConfig::default());
    let mut now = 0;
    for line in String::from_utf8_lossy(report).split('\n') {
        now += INTERVAL_MS;
        let _ = Readiness::parse(line);
        match parse_x_overload_header(Some(line)) {
            Some(p) => {
                assert!((0.0..=1.0).contains(&p.elu), "elu {} of {line:?}", p.elu);
                assert!((0.0..=1.0).contains(&p.gc), "gc {} of {line:?}", p.gc);
                assert!(p.adm.is_finite() && p.adm >= 0.0, "adm {} of {line:?}", p.adm);
                observer.apply_payload("w", &p, now);
            }
            None => observer.note_payload_missing("w", now),
        }
        observer.record_own_admitted("w");
        let _ = observer.try_consume_for("w", now);
    }
    let _ = observer.band_for("w");
    observer.sweep_stale(now + 10 * INTERVAL_MS, |_| true);
}
