//! Every `failover-harness` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=failover-harness) & binary(it)' <module>::`.

// The third-party RFC 3261 §16 proxy actor the b2bua-harness spiral
// scenarios run, shared as a source file.
#[path = "../../../b2bua-harness/tests/it/common/stateful_proxy.rs"]
#[allow(dead_code)]
mod stateful_proxy;

mod abandoned_callee_via_lb;
mod answer_lost_with_primary_no_answer;
mod backup_capacity;
mod call_terminate_on_backup;
mod consult_answer_deadline;
mod crossed_reinvite_round;
mod departure_support;
mod drain_exits_caught_up_inside_grace;
mod drain_flushes_releases_before_a_verified_exit;
mod drain_waits_for_a_backup_behind;
mod dual_face;
mod failover;
mod fault_primitives;
mod forward_flush_never_regresses_backup_progress;
mod held_cancel_takeover;
mod inflight_resend;
mod limiter_ha;
mod limiter_refresh_batch;
mod limiter_release_by_call;
mod limiter_uncounted_calls;
mod long_ring_ha;
mod message_ring_takeover;
mod new_call_queue_headroom;
mod new_call_replication_window;
mod not_ready_member_stays_pulled;
mod prack_repeat_takeover;
mod prack_takeover;
mod reinvite_2xx_acked_through_the_primary;
mod reinvite_answer_failover;
mod reinvite_round_gives_up_after_takeover;
mod reinvite_round_not_yet_replicated;
mod reject_final_loss_via_lb;
mod released_copy;
mod retransmission_turn_is_quiet;
mod retry_on_a_challenged_identity;
mod rfc_acceptance_window;
mod ring_deadline_takeover;
mod ringing_call_taken_over_on_departure;
mod service_answer_deadline;
mod silent_replication_stream;
mod spiral_takeover;
mod stale_no_answer_reclaim;
mod stood_down_round;
mod swept_call_answered_inside_the_lag;
mod swept_call_primary_returns;
mod takeover_request_uri_rewritten;
mod timer_generation_takeover;
mod transparent_v1;
mod undecided_call_settled_on_departure;
mod via_lb_reroute_glare;
mod withdrawn_endpoint_replacement;
mod withdrawn_primary_answers_in_its_window;
mod withdrawn_worker_latches_draining;
