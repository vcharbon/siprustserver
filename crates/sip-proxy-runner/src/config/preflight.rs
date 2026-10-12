//! Cross-component configuration checks: invariants that live between the
//! health probe and the worker load observer, which neither component can
//! enforce alone.

use sip_proxy::load_observer::LoadObserverConfig;

/// The health probe timings the cross-component checks need.
#[derive(Debug, Clone, Copy)]
pub struct ProbeTimingConfig {
    pub interval_ms: u64,
    pub timeout_ms: u64,
}

/// Cross-component config validation for the SIP front proxy.
///
/// Pure (no I/O, no logging, no `tokio`). Returns one human-readable line per
/// violated invariant, all of them, so the first boot attempt names every
/// problem.
///
/// Example: `interval_ms=2000 timeout_ms=1500` (cycle=3500 ms) with
/// `payload_stale_ms=3000` lets the 1 s sweep catch `age > 3000` between
/// probe-recv events on about half of the cycles, halving the per-worker admit
/// cap each time; the cap collapses to `cap_floor_cps` and never recovers. The
/// `payload_stale_ms ≥ 2 × probe_cycle` check refuses that configuration.
pub fn validate_config(
    probe: ProbeTimingConfig,
    observer: &LoadObserverConfig,
) -> Result<(), Vec<String>> {
    let mut violations: Vec<String> = Vec::new();

    if probe.interval_ms == 0 {
        violations.push(format!(
            "health_probe.interval_ms must be a positive number (got {}).",
            probe.interval_ms,
        ));
    }
    if probe.timeout_ms == 0 {
        violations.push(format!(
            "health_probe.timeout_ms must be a positive number (got {}).",
            probe.timeout_ms,
        ));
    }
    if observer.payload_stale_ms <= 0 {
        violations.push(format!(
            "worker_load_observer.payload_stale_ms must be positive (got {}).",
            observer.payload_stale_ms,
        ));
    }

    // Payload staleness outlasts two probe cycles: replies arrive
    // `interval + timeout` apart, and a shorter stale window lets
    // `sweep_stale` halve the cap between two replies. One cycle of margin
    // survives a single dropped probe.
    let probe_cycle_ms = probe.interval_ms.saturating_add(probe.timeout_ms);
    let min_stale = 2u64.saturating_mul(probe_cycle_ms);
    if observer.payload_stale_ms > 0 && (observer.payload_stale_ms as u64) < min_stale {
        violations.push(format!(
            "worker_load_observer.payload_stale_ms ({} ms) must be at least 2× the \
             HealthProbe cycle (interval_ms + timeout_ms = {} + {} = {} ms; minimum \
             allowed payload_stale_ms = {} ms). Otherwise sweep_stale halves the \
             per-worker admit cap on most probe cycles and the cluster silently \
             throttles every worker to cap_floor_cps.",
            observer.payload_stale_ms,
            probe.interval_ms,
            probe.timeout_ms,
            probe_cycle_ms,
            min_stale,
        ));
    }

    if observer.cap_floor_cps <= 0.0 {
        violations.push(format!("cap_floor_cps must be > 0 (got {}).", observer.cap_floor_cps,));
    }
    if observer.cap_floor_cps > observer.cap_initial_cps {
        violations.push(format!(
            "cap_floor_cps ({}) must be <= cap_initial_cps ({}).",
            observer.cap_floor_cps, observer.cap_initial_cps,
        ));
    }
    if observer.cap_initial_cps > observer.cap_ceiling_cps {
        violations.push(format!(
            "cap_initial_cps ({}) must be <= cap_ceiling_cps ({}).",
            observer.cap_initial_cps, observer.cap_ceiling_cps,
        ));
    }

    // Band ordering and hysteresis bounds belong to the band classifier.
    observer.validate_bands(&mut violations);

    // A cooldown shorter than one probe cycle lets the AIMD ladder follow a
    // single noisy probe.
    let cooldown_ms = observer.aimd_cooldown_ticks * observer.options_interval_ms as f64;
    if cooldown_ms < probe_cycle_ms as f64 {
        violations.push(format!(
            "aimd cooldown ({} × {} = {} ms) must be >= one HealthProbe cycle ({} ms) \
             so a single decrease isn't immediately followed by an increase tick.",
            observer.aimd_cooldown_ticks, observer.options_interval_ms, cooldown_ms, probe_cycle_ms,
        ));
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Panics with every violation on its own line after a fixed header: a
/// misconfigured proxy exits (non-zero, `CrashLoopBackOff`) before it binds.
pub fn refuse_to_start(violations: &[String]) -> ! {
    let header = "sip-front-proxy: refusing to start — invalid configuration:";
    let bullets = violations.iter().map(|v| format!("  - {v}")).collect::<Vec<_>>().join("\n");
    panic!("{header}\n{bullets}");
}

#[cfg(test)]
mod tests;
