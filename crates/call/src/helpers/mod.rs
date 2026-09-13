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
//!   - [`reliable`] — the a-facing reliable-provisional sequence (RFC 3262)
//!   - [`obligation`] — the dialog-level retransmission obligations: what the
//!     call owes, what an ACK discharges, the scopes a ladder is retired under
//!   - [`services`] — per-service slice accessors + opaque ext writes
//!   - [`record`] — CDR append, rule deactivation, SM-cursor rendering
//!   - [`message_ring`] — the message-ring append and its call-wide sequence
//!   - [`timer`] — the timer ledger (`replace_timer_by_id`, terminating cap,
//!     the keepalive one-interval ceiling)

pub mod dialog;
pub mod leg;
pub mod lens;
pub mod message_ring;
pub mod obligation;
pub mod peering;
pub mod record;
pub mod reliable;
pub mod services;
pub mod timer;

pub use dialog::{
    add_pending_request, bump_local_cseq, cache_sdp_on_leg_dialog, cached_sdp_for_leg_dialog,
    cancel_pending_request, close_rejected_invite_round, find_pending_request,
    invite_transaction_open, make_dialog_from_incoming, make_empty_dialog, relay_cseq_delta,
    remove_pending_request, retain_ack_branch, retain_emitted_ack, set_awaited_ack_cseq,
    update_remote_cseq, MakeDialogLegCtx,
};
pub use leg::{
    add_b_leg, b2bua_tag, confirmed_dialog, find_b_leg, find_b_leg_by_call_id,
    find_dialog_by_to_tag, find_leg, holds_local_tag, is_adopted, is_fully_resolved,
    leg_is_going_away, leg_is_resolved, leg_kind, record_invite_final, remote_tag,
    set_bye_disposition, set_leg_disposition, set_leg_state,
};
pub use lens::{update_dialog, update_leg};
pub use message_ring::record_message;
pub use obligation::{
    acked_2xx, advance_ladder, answers_initial_invite, clear_retained, obligations_in,
    retained_for, Scope,
};
pub use peering::{
    add_tag_mapping, all_peered_legs, find_by_a_tag, find_by_b_tag, get_peer, merge_leg,
    relay_peer_dialog, relay_peer_dialog_ready, resolve_relay_peer, split_leg,
};
pub use record::{add_cdr_event, deactivate_rule, dump_cursors};
pub use reliable::{
    admits_reliable_provisional, advance_reliable_ladder, assign_a_rseq, b_rseq_for,
    clear_all_reliable_provisional_emissions, clear_reliable_provisional_emission, leg_shown,
    owns_rseq_numbering, pending_invite_answered_by, pracked_provisional,
    record_pracked_provisional, record_reliable_provisional_emission,
    reliable_provisional_emission, reliable_provisional_relayed, retire_a_rseq,
    starts_reliable_ladder, unacknowledgeable_rack, RAckTokens,
};
pub use services::{
    promote_pem_promoted, promote_pem_state, promote_pem_window_open, record_relay_first_18x_value,
    refer_processed_locally, relay_first_18x_first_relayed, relay_first_18x_messages,
    relay_first_18x_stored_a_tag, relay_first_18x_strategy, relay_first_18x_value_relayed,
    set_call_ext, set_leg_ext, set_promote_pem, set_relay_first_18x_relayed, set_reroute,
    set_transfer, transfer_active, transfer_state,
};
pub use timer::{cap_keepalive_fire_at, replace_timer_by_id, TERMINATING_TIMEOUT_MS};
