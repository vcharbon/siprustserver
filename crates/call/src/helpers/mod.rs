//! Pure lens / accessor / timer helpers over the [`Call`](crate::model::Call)
//! tree. Helpers take
//! the `Call`/`Leg` by value, mutate in place, and return it — value semantics
//! from the caller's view, without deep clones. The data types themselves live
//! in [`crate::model`].
//!
//! Concern map:
//!   - [`lens`] — the base `update_leg` / `update_dialog` lenses
//!   - [`leg`] — role, lookup, resolution
//!   - [`leg_end`] — the INVITE outcome and dialog ends a leg's end records
//!   - [`resolved`] — what a received message names: its dialog, the relay a
//!     response answers
//!   - [`lifecycle`] — the two monotone axes of a call's life and how two copies
//!     of one call compare on them (replication reconciliation)
//!   - [`dialog`] — CSeq, ACK branch, pending and retired relays, SDP cache
//!   - [`peering`] — caller-facing fronts, active peer pair, relay-peer
//!     resolution
//!   - [`reliable`] — the a-facing reliable-provisional sequence (RFC 3262)
//!   - [`obligation`] — the dialog-level retransmission obligations: what the
//!     call owes, what an ACK discharges, the scopes a ladder is retired under
//!   - [`services`] — per-service slice accessors + opaque ext writes
//!   - [`record`] — CDR append (and the originated leg's attach-and-record), rule
//!     deactivation, SM-cursor rendering
//!   - [`decision_log`] — the decision-log append and its ordinal
//!   - [`message_ring`] — the message-ring append and its call-wide sequence
//!   - [`termination`] — the termination record and its message-ring cut
//!   - [`timer`] — the timer ledger (`replace_timer_by_id`, terminating cap,
//!     the keepalive one-interval ceiling)

pub mod decision_log;
pub mod dialog;
pub mod leg;
pub mod leg_end;
pub mod lens;
pub mod lifecycle;
pub mod message_ring;
pub mod obligation;
pub mod peering;
pub mod record;
pub mod reliable;
pub mod resolved;
pub mod services;
pub mod termination;
pub mod timer;

pub use decision_log::mark_decision;
pub use dialog::{
    add_pending_request, advance_peer_cseq, bump_our_cseq, cache_sdp_on_leg_dialog,
    cached_sdp_for_leg_dialog, cancel_pending_request, close_rejected_invite_round, close_relay,
    find_pending_request, invite_glare, invite_transaction_open, peer_update_pending,
    relayed_pending, remove_pending_request, retain_ack_branch, retain_emitted_ack,
    retired_relay_count, sender_invite_unacknowledged, sender_invite_unanswered,
    set_awaited_ack_cseq, take_retired_relays, PendingHolder,
};
pub use leg::{
    add_b_leg, callee_dialog, callee_dialog_mut, caller_answered, caller_dialog, caller_dialog_mut,
    carrying_dialog, dialog_with_peer, find_b_leg, find_b_leg_by_call_id, find_leg, is_adopted,
    is_fully_resolved, leg_is_going_away, leg_is_resolved, leg_kind, leg_own_tag, own_dialog,
};
pub use leg_end::{
    abandon_leg, cancel_early_dialogs, record_cancel_sent, record_cancelled_invite,
    record_dialled_invite_final, record_dialling_invite_rejected, record_invite_final,
    reject_early_dialogs, retire_early_dialogs, settle_straggler_release,
};
pub use lens::{update_dialog, update_leg};
pub use lifecycle::{lifecycle_advances, lifecycle_position, lifecycle_regresses};
pub use message_ring::{record_message, seal_turn};
pub use obligation::{
    acked_2xx, advance_ladder, answers_initial_invite, clear_retained, obligations_in,
    restate_leg_invite, restate_retained, retained_for, Scope,
};
pub use peering::{
    a_tag_shown, all_peered_legs, fronting_dialog, get_peer, merge_leg, relay_peer_dialog,
    relay_peer_dialog_ready, resolve_relay_peer, split_leg, RelayPeer,
};
pub use record::{add_cdr_event, add_originated_b_leg, deactivate_rule, dump_cursors};
pub use reliable::{
    admits_reliable_provisional, advance_reliable_ladder, assign_a_rseq, b_rseq_for,
    clear_all_reliable_provisional_emissions, clear_reliable_provisional_emission, leg_shown,
    note_own_prack_branch, note_responder_offer, offered_in_reliable_provisional, own_prack_branch,
    owns_rseq_numbering, pending_invite_answered_by, pracked_provisional, provisional_acknowledged,
    rack_pracked_here, record_pracked_provisional, record_reliable_provisional_emission,
    reliable_provisional_emission, reliable_provisional_relayed, retire_a_rseq, rseq_in_order,
    starts_reliable_ladder, unacknowledgeable_rack, unacknowledged_relayed_provisionals,
    unacknowledged_shown, OwedPrack, RAckTokens, UnackedShown,
};
pub use resolved::{response_origin, Resolved, ResponseFacts, ResponseOrigin, UNRESOLVED};
pub use services::{
    promote_pem_promoted, promote_pem_state, promote_pem_window_open, record_relay_first_18x_value,
    refer_processed_locally, relay_first_18x_first_relayed, relay_first_18x_messages,
    relay_first_18x_stored_a_tag, relay_first_18x_strategy, relay_first_18x_value_relayed,
    set_call_ext, set_leg_ext, set_promote_pem, set_relay_first_18x_relayed, set_reroute,
    set_transfer, transfer_active, transfer_state,
};
pub use termination::{record_termination, seal_termination_seq};
pub use timer::{cap_keepalive_fire_at, replace_timer_by_id, TERMINATING_TIMEOUT_MS};
