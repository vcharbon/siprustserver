//! The reaped check covers the shell: a lock recovered from poison or a
//! supervised task restarted during the run fails it, unless the test
//! declares the fault.

use b2bua::supervise::{GAUGE_SAMPLER, RESTARTS};
use b2bua_harness::B2buaScene;

/// One established call hung up by alice and reaped, then one restart of a
/// supervised task, as a panicking sampler would count it.
async fn a_call_then_a_restart(name: &str) -> B2buaScene {
    let s = B2buaScene::new(name).await;
    let mut dialog = s.establish().await;
    s.hangup(&mut dialog).await;
    RESTARTS.restarted(GAUGE_SAMPLER);
    s
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "shell defect: 1 poisoned lock(s) recovered or task(s) restarted")]
async fn a_task_restarted_during_the_run_fails_the_reaped_check() {
    let s = a_call_then_a_restart("reaped-shell-restart").await;
    let _ = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_declared_shell_fault_passes_the_reaped_check() {
    let s = a_call_then_a_restart("reaped-shell-declared").await;
    s.b2bua.expect_shell_faults();
    let _ = s.finish().await;
}
