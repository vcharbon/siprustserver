//! Pure lens / accessor / timer helpers over the [`Call`](crate::model::Call)
//! tree. Helpers take
//! the `Call`/`Leg` by value, mutate in place, and return it — value semantics
//! from the caller's view, without deep clones. The data types themselves live
//! in [`crate::model`].
//!
//! Concern map:
//!   - [`lens`] — the base `update_leg` / `update_dialog` lenses
//!   - [`leg`] — role, lookup, resolution, the next leg id
//!   - [`attempt`] — open, join, end and settle the call's attempts
//!   - [`leg_end`] — the INVITE outcome and dialog ends a leg's end records
//!   - [`resolved`] — what a received message names: its dialog, the request
//!     record a response answers
//!   - [`lifecycle`] — the two monotone axes of a call's life and how two copies
//!     of one call rank on them
//!   - [`compared`] — the compared fields of a call record and the closed
//!     exempt list (ADR-0055 §7)
//!   - [`sequence`] — the in-dialog requests two copies of one call carry:
//!     the sequences they reached, the relays they hold
//!   - [`dialog`] — CSeq, INVITE glare, SDP cache
//!   - [`request_records`] — the request records a call's dialogs hold: add,
//!     find by final or branch, close (ADR-0055 §2)
//!   - [`invite_records`] — the INVITE records: the one a dialog last sent
//!     and the ACK its 2xx owes, the one it answered 2xx, a dialling
//!     INVITE's image
//!   - [`peering`] — caller-facing fronts, active peer pair, relay-peer
//!     resolution
//!   - [`reliable`] — the reliable provisionals the call received and where
//!     each was shown (RFC 3262)
//!   - [`prack_disposition`](mod@prack_disposition) — what a PRACK this stack receives draws
//!   - [`obligation`] — the dialog-level retransmission obligations: what the
//!     call owes, what an ACK discharges, the scopes a ladder is retired under
//!   - [`services`] — per-service slice accessors + opaque ext writes
//!   - [`record`] — CDR append (and the originated leg's attach-and-record), rule
//!     deactivation, SM-cursor rendering
//!   - [`decision_log`] — the decision-log append and its ordinal
//!   - [`instants`] — every absolute instant the record holds, moved together
//!     into another clock frame
//!   - [`message_ring`] — the message-ring append and its call-wide sequence
//!   - [`termination`] — the termination record and its message-ring cut
//!   - [`timer`] — the timer ledger (`arm_timer`, `disarm_timer`, terminating cap,
//!     the keepalive one-interval ceiling)
//!   - [`timer_scope`] — whether the scope a timer guards has ended

pub mod attempt;
pub mod compared;
pub mod decision_log;
pub mod dialog;
pub mod instants;
pub mod invite_records;
pub mod leg;
pub mod leg_end;
pub mod lens;
pub mod lifecycle;
pub mod message_ring;
pub mod obligation;
pub mod peering;
pub mod prack_disposition;
pub mod record;
pub mod reliable;
pub mod request_records;
pub mod resolved;
pub mod sequence;
pub mod services;
pub mod termination;
pub mod timer;
pub mod timer_scope;

pub use attempt::{
    dialled_leg_ended, end_attempt, find_attempt, join_attempt, next_attempt_id, open_attempt,
    settle_attempts,
};
pub use compared::{merge_exempt, same_state, with_own_running};
pub use decision_log::mark_decision;
pub use dialog::{
    advance_peer_cseq, bump_our_cseq, cache_sdp_on_leg_dialog, cached_sdp_for_leg_dialog,
    invite_glare, invite_transaction_open, peer_update_pending, sender_invite_unacknowledged,
    sender_invite_unanswered,
};
pub use instants::shift_instants;
pub use invite_records::{
    ack_branch, answered_2xx, answered_2xx_mut, awaited_ack_cseq, close_rejected_invite_round,
    emitted_ack, hold_answered_2xx, invite_image, invite_relay, invite_taken, invite_untaken,
    open_sent_invite, relayed_invite_pending, retain_ack_branch, retain_emitted_ack, sent_exchange,
    sent_exchange_mut, sent_invite_image, set_awaited_ack_cseq,
};
pub use leg::{
    add_b_leg, callee_dialog, callee_dialog_mut, caller_answered, caller_dialog, caller_dialog_mut,
    carrying_dialog, dialog_with_peer, dialog_with_peer_mut, find_b_leg, find_b_leg_by_call_id,
    find_leg, is_adopted, is_fully_resolved, leg_is_going_away, leg_is_resolved, leg_kind,
    leg_own_tag, mark_rang, next_b_leg_id, own_dialog,
};
pub use leg_end::{
    abandon_leg, cancel_early_dialogs, record_cancel_sent, record_cancelled_invite,
    record_dialled_invite_final, record_dialling_invite_rejected, record_invite_final,
    reject_early_dialogs, retire_early_dialogs,
};
pub use lens::{update_dialog, update_leg};
pub use lifecycle::{lifecycle_behind, lifecycle_position};
pub use message_ring::{record_message, seal_turn};
pub use obligation::{
    acked_2xx, advance_ladder, answers_initial_invite, clear_retained, obligations_in,
    restate_leg_invite, restate_retained, retained_for, Scope,
};
pub use peering::{
    a_tag_shown, all_peered_legs, fronting_dialog, get_peer, merge_leg, note_shown,
    relay_peer_dialog, relay_peer_dialog_ready, resolve_relay_peer, split_leg, RelayPeer,
};
pub use prack_disposition::{
    answerable_shown, named_by, prack_disposition, responder_dialog_carries_prack, AnswerableShown,
    PrackDisposition, RAckTokens,
};
pub use record::{add_cdr_event, add_originated_b_leg, deactivate_rule, dump_cursors};
pub use reliable::{
    acknowledge_shown, admits_reliable_provisional, advance_shown_ladder, arrival,
    cease_shown_ladder, described_before, leg_shown, note_responder_ack, provisional,
    record_provisional, responder_of, show_provisional, shown_as, shown_emission,
    start_shown_ladder, starts_shown_sequence, Arrival,
};
pub use request_records::{
    add_record, answered_record, cancel_relay, close_held, close_record, every_record, find_relay,
    held_record, next_held_deadline, record_with_branch, records_of, remove_relay,
    take_overdue_held, take_retired_owed, Answers, RecordHolder,
};
pub use resolved::{response_origin, Resolved, ResponseFacts, ResponseOrigin, UNRESOLVED};
pub use sequence::{holds_records_of, keep_dialogs, raise_sequences};
pub use services::{
    promote_pem_promoted, promote_pem_state, promote_pem_window_open, record_relay_first_18x_value,
    refer_processed_locally, relay_first_18x_first_relayed, relay_first_18x_messages,
    relay_first_18x_stored_a_tag, relay_first_18x_strategy, relay_first_18x_value_relayed,
    set_call_ext, set_leg_ext, set_promote_pem, set_relay_first_18x_relayed, set_reroute,
    set_transfer, transfer_active, transfer_state,
};
pub use termination::{record_termination, seal_termination_seq};
pub use timer::{
    arm_timer, cap_keepalive_fire_at, disarm_timer, holds_timer, retire_timers,
    TERMINATING_TIMEOUT_MS,
};
pub use timer_scope::{scope_ended, setup_over};
