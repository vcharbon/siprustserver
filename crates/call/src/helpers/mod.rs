//! Pure lens / accessor / timer helpers over the [`Call`](crate::model::Call)
//! tree. Helpers take
//! the `Call`/`Leg` by value, mutate in place, and return it — value semantics
//! from the caller's view, without deep clones. The data types themselves live
//! in [`crate::model`].
//!
//! The RNG seam is deferred (ADR-0008): the dialog constructors take the
//! initial CSeq as a parameter instead of drawing it from a runtime RNG.
//!
//! Concern map:
//!   - [`lens`] — the base `update_leg` / `update_dialog` lenses
//!   - [`leg`] — role, lookup, state/disposition setters, resolution, tags
//!   - [`dialog`] — CSeq, ACK branch, pending relays, SDP cache, constructors
//!   - [`peering`] — tag map, active peer pair, relay-peer resolution
//!   - [`services`] — per-service slice accessors + opaque ext writes
//!   - [`record`] — CDR append, rule deactivation, SM-cursor rendering
//!   - [`timer`] — the timer ledger (`replace_timer_by_id`, terminating cap)

pub mod dialog;
pub mod leg;
pub mod lens;
pub mod peering;
pub mod record;
pub mod services;
pub mod timer;

pub use record::{add_cdr_event, deactivate_rule, dump_cursors};
pub use dialog::{
    add_pending_request, bump_local_cseq, cache_sdp_on_leg_dialog, cached_sdp_for_leg_dialog,
    cancel_pending_request, find_pending_request, make_dialog_from_incoming, make_empty_dialog,
    relay_cseq_delta, remove_pending_request, retain_ack_branch, update_remote_cseq,
    MakeDialogLegCtx,
};
pub use leg::{
    add_b_leg, b2bua_tag, confirmed_dialog, find_b_leg, find_b_leg_by_call_id,
    find_dialog_by_to_tag, find_leg, is_adopted, is_fully_resolved, leg_is_resolved, leg_kind,
    remote_tag, set_bye_disposition, set_leg_disposition, set_leg_state,
};
pub use lens::{update_dialog, update_leg};
pub use peering::{
    add_tag_mapping, all_peered_legs, find_by_a_tag, find_by_b_tag, get_peer, merge_leg,
    relay_peer_dialog_ready, resolve_relay_peer, split_leg,
};
pub use services::{
    promote_pem_promoted, promote_pem_state, promote_pem_window_open,
    record_relay_first_18x_value, relay_first_18x_first_relayed, relay_first_18x_messages,
    relay_first_18x_stored_a_tag, relay_first_18x_strategy, relay_first_18x_value_relayed,
    set_call_ext, set_leg_ext, set_promote_pem, set_relay_first_18x_relayed, set_reroute,
    set_transfer, transfer_active, transfer_state,
};
pub use timer::{replace_timer_by_id, TERMINATING_TIMEOUT_MS};
