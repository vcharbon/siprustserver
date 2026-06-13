//! On-demand CPU flamegraph capture for the runner binaries.
//!
//! A SIGPROF-sampling profiler (pprof-rs) that, on request, samples ALL threads
//! of the current process for a fixed window and renders an inferno flamegraph
//! SVG. No `perf`, no privileges, no kernel PMU — works in an unprivileged
//! container on the WSL2 kernel where `perf` is unavailable. Backs the
//! `/debug/flamegraph` HTTP route on the b2bua-worker + front-proxy metrics
//! servers, so an endurance run can profile either tier live (e.g. around a
//! chaos reboot) without rebuilding or redeploying.
//!
//! When idle there is ZERO overhead: the SIGPROF handler exists only while a
//! [`pprof::ProfilerGuard`] is alive (i.e. only during an active capture).

use std::time::Duration;

/// Sample the current process for `seconds` at `freq` Hz and return a flamegraph
/// SVG.
///
/// Blocking (it `thread::sleep`s the window and renders synchronously) — call it
/// from `tokio::task::spawn_blocking` so it never parks an async worker. Errs if
/// a profiler is already running (pprof permits one active guard per process) or
/// if report/SVG rendering fails.
pub fn capture_svg(seconds: u64, freq: i32) -> Result<Vec<u8>, String> {
    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(freq)
        // Don't attribute samples to the unwinder/runtime/libc frames — keeps
        // the graph to application stacks.
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .map_err(|e| format!("profiler start failed (already running?): {e}"))?;
    std::thread::sleep(Duration::from_secs(seconds));
    let report = guard
        .report()
        .build()
        .map_err(|e| format!("report build failed: {e}"))?;
    let mut svg = Vec::new();
    report
        .flamegraph(&mut svg)
        .map_err(|e| format!("flamegraph render failed: {e}"))?;
    Ok(svg)
}

/// Parse `?seconds=N` out of a raw HTTP request target (e.g.
/// `/debug/flamegraph?seconds=30`), falling back to `default` and clamping to
/// `[1, max]`. Shared by both metrics servers' route handlers.
pub fn parse_seconds(target: &str, default: u64, max: u64) -> u64 {
    target
        .split('?')
        .nth(1)
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("seconds=")))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
        .clamp(1, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_seconds_defaults_and_clamps() {
        assert_eq!(parse_seconds("/debug/flamegraph", 20, 120), 20);
        assert_eq!(parse_seconds("/debug/flamegraph?seconds=30", 20, 120), 30);
        assert_eq!(parse_seconds("/debug/flamegraph?seconds=9999", 20, 120), 120);
        assert_eq!(parse_seconds("/debug/flamegraph?seconds=0", 20, 120), 1);
        assert_eq!(parse_seconds("/debug/flamegraph?x=1&seconds=5", 20, 120), 5);
        assert_eq!(parse_seconds("/debug/flamegraph?seconds=junk", 20, 120), 20);
    }

    #[test]
    fn capture_produces_an_svg() {
        // Burn CPU in worker threads for the whole window so the sampler has
        // real stacks to collect.
        let handles: Vec<_> = (0..2)
            .map(|_| {
                std::thread::spawn(|| {
                    let mut x = 0u64;
                    let start = std::time::Instant::now();
                    while start.elapsed().as_millis() < 1200 {
                        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    }
                    x
                })
            })
            .collect();
        let svg = capture_svg(1, 197).expect("capture");
        for h in handles {
            let _ = h.join();
        }
        // inferno emits a standalone SVG document; the `<svg` tag always appears.
        assert!(!svg.is_empty(), "empty svg");
        assert!(
            svg.windows(4).any(|w| w == b"<svg"),
            "no <svg tag in {} bytes",
            svg.len()
        );
    }
}
