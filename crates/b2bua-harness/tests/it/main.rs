//! Every b2bua-harness integration test, in ONE binary.
//!
//! Cargo links one binary per `tests/*.rs` file, and each links the whole
//! dependency graph — so the tests live in this directory and are reached
//! through the `mod` lines below instead. A new test file goes here and gets a
//! line added; a file left directly in `tests/` silently costs another copy of
//! the graph. See docs/adr/0030-one-integration-test-binary-per-crate.md.
//!
//! Select one: `cargo test -p b2bua-harness --test it <module>::<name>`.

// Shared fixtures, reached from a test module as `crate::common::…`.
mod common;

mod ack_body_relayed;
mod admit_owes_release;
mod announcement;
mod basic_call;
mod basic_call_media;
mod bye_no_200_reap;
mod call_keyed_holds;
mod cancel_200_crossing;
mod cancel_200_crossing_internal;
mod cancel_after_answer;
mod cancel_before_provisional;
mod cancel_during_slow_decision;
mod capacity_admission;
mod cdr_write_before_remove;
mod decision_context;
mod decision_deadline;
mod decision_lands_on_cancelled_call;
mod decision_log;
mod discarded_invite_and_cancel;
mod discarded_request_readmitted;
mod failure;
mod failure_header_relay;
mod fake_prack;
mod foreign_dialog_tag;
mod going_away_gate;
mod header_lines;
mod info_body_relay;
mod invite_sent;
mod keepalive;
mod keepalive_481;
mod keepalive_configurable_interval;
mod keepalive_reap_both_directions;
mod keepalive_timeout;
mod keepalive_via_proxy;
mod lifetime_message_cap;
mod limit_cases;
mod limiter;
mod limiter_breaker;
mod limiter_fault_call_latency;
mod limiter_refresh;
mod limiter_refresh_batch;
mod limiter_release_queue;
mod long_ring;
mod max_duration_anchor;
mod max_forwards;
mod message_ring;
mod new_call_outcomes;
mod no_answer_absorb;
mod no_answer_cancelled_call;
mod numbering_plan;
mod orphan_reject_no_leak;
mod overflow_teardown;
mod own_timer_past_bounds;
mod prack;
mod prack_forking;
mod prack_update_forking;
mod promote_pem;
mod provisional_after_answer;
mod proxy_b2bua;
mod reack_retransmitted_2xx;
mod realcall_functional;
mod reaped_limiter_check;
mod reaper;
mod reaper_liveness;
mod refer_allow;
mod refer_c_realign;
mod refer_full_transfer;
mod refer_gating;
mod refer_no_notify_after_terminated;
mod refer_reject;
mod refer_timers;
mod refer_transparent_relay;
mod reinvite;
mod reinvite_cancel;
mod release_event;
mod reroute_limiter_holds;
mod response_contact_scope;
mod route_fold_limiter_holds;
mod second_final_refused;
mod service_http;
mod service_timers;
mod setup_stall_global_duration_reap;
mod setup_timeout;
mod stated_refusal;
mod store_fault;
mod suppress_18x;
mod target_admission_gate;
mod teardown_header_relay;
mod teardown_races;
mod termination;
mod tier3_admission_gate;
mod transaction_outcome_past_bounds;
mod unacked_2xx_reap;
mod unacked_reinvite_2xx_reap;
mod unanswered_at_release;
mod unreadable_routing_target;
mod unroutable;
mod unwired_store;
mod update_matrix;
mod update_response_relay;
mod x_overload_signal;
