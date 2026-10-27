//! A placed call's task lives as long as the call, so its future's size is
//! memory held per in-flight call: the task holds the call once, and the call
//! holds no step's future beyond the step.

use std::future::Future;
use std::mem::size_of;

use crate::call_task::call_task;
use crate::driver::run_one;

/// The size of the future `f` returns, from the fn item alone (no call, no poll).
#[allow(clippy::too_many_arguments)]
fn future_size<A, B, C, D, E, F, G, H, I, J, R>(
    _: impl FnOnce(A, B, C, D, E, F, G, H, I, J) -> R,
) -> usize {
    size_of::<R>()
}

/// The size of the spawned task that runs the future `f` returns.
#[allow(clippy::too_many_arguments)]
fn task_size<A, B, C, D, E, F, G, H, I, J, R: Future<Output = ()>>(
    _: impl FnOnce(A, B, C, D, E, F, G, H, I, J) -> R,
) -> usize {
    fn ret_size<A, B, C, D, R>(_: impl FnOnce(A, B, C, D) -> R) -> usize {
        size_of::<R>()
    }
    ret_size(call_task::<R>)
}

/// One placed call's future, held for the whole call.
const RUN_ONE_MAX: usize = 6 * 1024;
/// The spawned task around it: the call once, plus its panic boundary.
const CALL_TASK_MAX: usize = 6 * 1024;

#[test]
fn the_call_future_holds_no_step_inline() {
    let size = future_size(run_one);
    assert!(size <= RUN_ONE_MAX, "the run_one future is {size} B, bound {RUN_ONE_MAX} B");
}

#[test]
fn the_call_task_holds_its_call_once() {
    let size = task_size(run_one);
    assert!(size <= CALL_TASK_MAX, "the call task is {size} B, bound {CALL_TASK_MAX} B");
}
