//! The worker under the poll watchdog (ADR-0057 §6): the watchdog started
//! over a core, and the exit a stuck poll takes.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use b2bua::drain::DrainBounds;
use b2bua::B2buaCore;
use poll_watchdog::{Stuck, Watchdog};

use super::close_cdr;

/// Start `watchdog` at `threshold` over `core`: a stuck task names the call
/// it serves, and the process ends past [`stuck_exit_bound`]. `Err` when a
/// thread's CPU time does not read: the worker refuses to boot.
pub(crate) fn watch_core(
    watchdog: &Watchdog,
    core: &B2buaCore,
    threshold: Duration,
    bounds: DrainBounds,
) -> Result<(), String> {
    watchdog
        .start(threshold)
        .map_err(|e| format!("the poll watchdog cannot read a thread's CPU time: {e}"))?;
    watchdog.call_of(Arc::new(core.call_of_task()));
    watchdog.exit_after(stuck_exit_bound(bounds));
    Ok(())
}

/// How long the stuck exit may take before the watchdog ends the process:
/// the discharge and the limiter release flush, each within the release
/// flush bound, the CDR close within its own, and one second.
pub(crate) fn stuck_exit_bound(bounds: DrainBounds) -> Duration {
    bounds.release_flush * 2 + bounds.cdr_flush + Duration::from_secs(1)
}

/// A poll stuck on the CPU (the watchdog logged it): Draining, the call the
/// stuck task serves discharged within the release flush bound unless
/// [`leaves_the_call`] says why not, the queued limiter releases flushed, the
/// CDR sink closed, and the exit with the watchdog's code.
pub(crate) async fn stuck_exit(
    core: &B2buaCore,
    name: &str,
    bounds: DrainBounds,
    stuck: Stuck,
) -> ExitCode {
    core.begin_draining();
    if let (Some(call_ref), Some(reason)) = (&stuck.call_ref, leaves_the_call(core)) {
        tracing::error!(service = name, %call_ref, reason, "the call a stuck poll holds: left to the takeover");
    } else if let (Some(call_ref), Some(task)) = (&stuck.call_ref, stuck.task) {
        let discharge = core.discharge_stuck(call_ref, task, bounds.release_flush).await;
        tracing::error!(
            service = name,
            %call_ref,
            discharge = discharge.label(),
            "the call a stuck poll holds: discharge ended"
        );
    }
    let flush = core.limiter().flush(bounds.release_flush).await;
    core.metrics().limiter().count_release_flush(&flush);
    tracing::error!(
        service = name,
        releases_queued = flush.queued,
        releases_given_up = flush.given_up,
        exit_code = poll_watchdog::EXIT_CODE,
        "exiting after a stuck poll"
    );
    close_cdr(core, name, bounds.cdr_flush).await;
    ExitCode::from(poll_watchdog::EXIT_CODE)
}

/// Why the stuck exit leaves its call to the takeover rather than discharge
/// it: a superseded worker authors nothing, and once the CDR sink's close has
/// begun the discharge's CDR could be dropped while its delete still reaches
/// the backup. `None` when it discharges.
pub(crate) fn leaves_the_call(core: &B2buaCore) -> Option<&'static str> {
    if core.is_superseded() {
        return Some("superseded");
    }
    core.cdr().closing().then_some("the CDR sink is closing")
}

#[cfg(test)]
mod child_tests;
