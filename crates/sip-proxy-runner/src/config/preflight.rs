//! Cross-component configuration checks: invariants that live between the
//! health probe and the worker load observer, which neither component can
//! enforce alone.

use sip_proxy::load_observer::LoadObserverConfig;

#[derive(Debug, Clone, Copy)]
pub struct ProbeTimingConfig {
    pub interval_ms: u64,
    pub timeout_ms: u64,
}

/// Checks the invariants between the health probe and the load observer and
/// returns one line per violation, all of them, so one boot attempt names
/// every problem. Pure: no I/O, no logging, no `tokio`.
///
/// `payload_stale_ms` is at least two probe cycles (`interval_ms +
/// timeout_ms`). Probe replies arrive one cycle apart, and a shorter window
/// lets `sweep_stale` halve the per-worker admit cap between two replies until
/// it sits at `cap_floor_cps` for good. The second cycle survives one dropped
/// probe. The AIMD cooldown is at least one probe cycle, so one noisy probe
/// cannot drive a decrease and the next increase.
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

    let probe_cycle_ms = probe.interval_ms.saturating_add(probe.timeout_ms);
    let min_stale = 2u64.saturating_mul(probe_cycle_ms);
    let stale_ms = u64::try_from(observer.payload_stale_ms).unwrap_or(0);
    if stale_ms > 0 && stale_ms < min_stale {
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

    observer.validate_bands(&mut violations);

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

/// The boot refusal of `violations`: every one on its own line after a fixed
/// header. A misconfigured proxy exits non-zero (`CrashLoopBackOff`) on it
/// before it binds.
pub fn refusal(violations: &[String]) -> String {
    let header = "sip-front-proxy: refusing to start — invalid configuration:";
    let bullets = violations.iter().map(|v| format!("  - {v}")).collect::<Vec<_>>().join("\n");
    format!("{header}\n{bullets}")
}

#[cfg(test)]
mod tests;
