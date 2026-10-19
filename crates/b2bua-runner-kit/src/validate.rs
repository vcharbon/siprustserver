//! The boot refusals of a worker's knobs a runner may override: each a pure
//! check returning the refusal's text.

use b2bua::destination_allowlist::{classify_admission, AdmissionVerdict};

/// The ingress brake percentage must keep the brake meaningful: it fires at
/// ingress depth >= queue_max × pct/100, so 0 would shed every packet and >100
/// would never fire. Checked at [`RunnerEnv::bind`](crate::RunnerEnv::bind);
/// exported for runners that override the field and want the same boot refusal.
pub fn validate_ingress_brake_pct(udp_ingress_brake_pct: u32) -> Result<(), String> {
    if !(1..=100).contains(&udp_ingress_brake_pct) {
        return Err(format!(
            "B2BUA_UDP_INGRESS_BRAKE_PCT={udp_ingress_brake_pct} out of range 1..=100: the ingress \
             brake fires at ingress depth >= queue_max × pct/100, so 0 would shed \
             every packet and >100 would never fire. Use 1..=100 (100 = brake only \
             when the queue is full)"
        ));
    }
    Ok(())
}

/// Boot-time coherence check for runners with a **static default callee**: the
/// b-leg admission gate (`apply_route`) classifies the callee's
/// `destination.host`, so a default the worker's own destination allow-list
/// rejects means EVERY default-routed call is 503'd before the b-leg and the
/// worker can never serve one. Per-call destinations are runtime and not
/// checkable here, but a self-rejecting default is an unambiguous
/// misconfiguration to refuse boot on. Pure (no env / IO) so it is
/// unit-testable.
pub fn validate_default_dest(
    dest_host: &str,
    worker_allowed_target_suffixes: &[String],
) -> Result<(), String> {
    if classify_admission(dest_host, worker_allowed_target_suffixes) == AdmissionVerdict::Reject {
        return Err(format!(
            "B2BUA_DEST host {dest_host:?} is rejected by its own destination \
             allow-list WORKER_ALLOWED_TARGET_SUFFIXES={worker_allowed_target_suffixes:?}: \
             every default-routed call would be 503'd before the b-leg. Add a \
             matching suffix (e.g. \".svc.cluster.local\"), use an IP literal, or \
             set \"*\" to disable the gate"
        ));
    }
    Ok(())
}

/// The process subscriber of the service `name`; a log writer the OS refuses
/// ends the process.
pub(crate) fn observe_or_exit(name: &str) -> observe::ObserveGuard {
    observe::init_production(name).unwrap_or_else(|e| {
        eprintln!("{name} FATAL: {e}");
        std::process::exit(1)
    })
}
