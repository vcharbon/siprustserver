//! The actor future lives as long as its call while it mostly idles on its
//! inbox, so every byte of it is held per endpoint for the whole call: the state
//! rides behind a pointer and a reaction's future exists only while it runs. The
//! boxed inbox wait is a heap block held for as long as the actor idles.

use std::mem::size_of;

use crate::actor::runner::{react_until_done, run_actor, ActorState};
use crate::actor::shared_endpoint::Inbox;

/// The size of the future `f` returns, from the fn item alone (no call, no poll).
fn future_size<A, R, F: FnOnce(A) -> R>(_: F) -> usize {
    size_of::<R>()
}

/// Every endpoint's live state: its transactions and dialogs live behind pointers.
const ACTOR_STATE_MAX: usize = 3 * 1024;
/// The reactor loop idling on its inbox: its arms' futures, no reaction inlined.
const REACT_UNTIL_DONE_MAX: usize = 4 * 1024;
/// The actor future held for the whole call: a pointer to its state plus the loop.
const RUN_ACTOR_MAX: usize = 4 * 1024;
/// The inbox wait an idle actor holds on the heap: its timer plus pointers.
const INBOX_RECV_MAX: usize = 256;

#[test]
fn the_actor_state_holds_its_transactions_and_dialogs_boxed() {
    let size = size_of::<ActorState<'static>>();
    assert!(size <= ACTOR_STATE_MAX, "ActorState is {size} B, bound {ACTOR_STATE_MAX} B");
}

#[test]
fn the_reactor_loop_future_inlines_no_reaction() {
    let size = future_size(react_until_done);
    assert!(
        size <= REACT_UNTIL_DONE_MAX,
        "the react_until_done future is {size} B, bound {REACT_UNTIL_DONE_MAX} B"
    );
}

#[test]
fn the_actor_future_holds_its_state_behind_a_pointer() {
    let size = future_size(run_actor);
    assert!(size <= RUN_ACTOR_MAX, "the run_actor future is {size} B, bound {RUN_ACTOR_MAX} B");
}

#[test]
fn the_idle_inbox_wait_holds_only_its_timer_and_pointers() {
    let size = future_size(Inbox::recv);
    assert!(size <= INBOX_RECV_MAX, "the Inbox::recv future is {size} B, bound {INBOX_RECV_MAX} B");
}
