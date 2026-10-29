//! The one-line run status a load generator prints on each window.

use e2e_model::{Progress, SeriesPoint};

use crate::html::{clock, cps};

/// `elapsed 0:35 / 1:00 · 19.9 cps (target 20) · in flight 41 · ok 650 fail 3
/// rej 0 · error 0.5 % (last 5 s)`: where the run stands, and the failed
/// share of the calls ended in `window`, the window just closed.
pub fn status_line(progress: &Progress, duration_secs: u64, window: &SeriesPoint) -> String {
    let err = window.error_pct().map_or("–".to_string(), |e| format!("{e:.1} %"));
    format!(
        "elapsed {} / {} · {:.1} cps (target {}) · in flight {} · ok {} fail {} rej {} · \
         error {err} (last {} s)",
        clock(progress.elapsed_ms),
        clock(duration_secs.saturating_mul(1000)),
        progress.achieved_cps(),
        cps(progress.target_cps),
        progress.in_flight,
        progress.ok,
        progress.failed,
        progress.rejected,
        (window.span_ms + 500) / 1000
    )
}
