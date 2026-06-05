//! The transparent-failover matrix (ADR-0013, docs/plan/failover-test-matrix-cells.md).
//! Each cell runs the same scenario clean (baseline) and with the failover
//! injected at the safe-point, asserting (1) the external observation is
//! identical and (2) the universal teardown sweep holds (no held context on
//! either node, a CDR written, the limiter drained).
//!
//! v1 = Established state, Kill fault. Early / ConfirmedPreAck / Drain / Keepalive
//! cells are staged behind the runner support that lands next.

use failover_harness::transparent_matrix;
use failover_harness::{DialogState::*, Event::*, Fault::*, Party, Recovery::*};

transparent_matrix! {
    // ── Terminating: BYE handled by the backup, both directions ──────────────
    established__bye_alice__kill__stay_dead:
        Established, Bye(Party::Caller), Kill, StayDead, 0;
    established__bye_alice__kill__reboot_after_takeover:
        Established, Bye(Party::Caller), Kill, RebootAfterTakeover, 0;
    established__bye_bob__kill__stay_dead:
        Established, Bye(Party::Callee), Kill, StayDead, 0;
    established__bye_bob__kill__reboot_after_takeover:
        Established, Bye(Party::Callee), Kill, RebootAfterTakeover, 0;

    // ── Generic non-terminating in-dialog (seeded method), both directions ───
    established__generic_alice_s0__kill__stay_dead:
        Established, Generic { method: sip_message::generators::InDialogMethod::Invite, from: Party::Caller }, Kill, StayDead, 0;
    established__generic_alice_s0__kill__reboot_after_takeover:
        Established, Generic { method: sip_message::generators::InDialogMethod::Invite, from: Party::Caller }, Kill, RebootAfterTakeover, 0;
    established__generic_bob_s0__kill__reboot_after_takeover:
        Established, Generic { method: sip_message::generators::InDialogMethod::Invite, from: Party::Callee }, Kill, RebootAfterTakeover, 0;

    // ── Nothing: pure re-hydration on reboot, then terminate on the primary ──
    established__nothing__kill__reboot_no_traffic:
        Established, Nothing, Kill, RebootNoTraffic, 0;
}

// AS-generated keepalive OPTIONS (its own category): after the primary reboots
// + reclaims with no traffic, the reclaimed AS probes BOTH legs with in-dialog
// OPTIONS at the production 300 s cadence, refreshing dead-peer detection AND
// the call-limiter hold — and the failover must be transparent: a long, idle
// call survives the kill→reboot→reclaim cycle exactly as the clean baseline does
// (the long-call-on-reboot loss the endurance run flagged). The runner's
// `keepalive_tick` poll-advances to whichever keepalive deadline applies
// (interval-from-establish in the baseline, interval-from-reclaim in the
// variant) in sub-reap steps, so each leg is answered inside its 5 s window
// regardless of the three interleaving timers (eager-takeover keepalive,
// reclaimed-primary keepalive, dead-peer reap). Core keepalive-rearm-on-takeover
// is also covered by `failover.rs::hydrated_call_rearms_keepalive_and_reaps_dead_peer`
// and the happy-path foundation by `failover.rs::successful_long_call_with_as_generated_options`.
transparent_matrix! {
    established__keepalive__kill__reboot_no_traffic:
        Established, Keepalive, Kill, RebootNoTraffic, 0;
}

// Generic method rotation (UPDATE / INFO / OPTIONS) — the seeded {method}
// coverage the matrix rotates so all generic shapes are exercised.
transparent_matrix! {
    established__generic_update_alice__kill__reboot_after_takeover:
        Established, Generic { method: sip_message::generators::InDialogMethod::Update, from: Party::Caller }, Kill, RebootAfterTakeover, 1;
    established__generic_info_alice__kill__stay_dead:
        Established, Generic { method: sip_message::generators::InDialogMethod::Info, from: Party::Caller }, Kill, StayDead, 2;
    established__generic_options_bob__kill__stay_dead:
        Established, Generic { method: sip_message::generators::InDialogMethod::Options, from: Party::Callee }, Kill, StayDead, 3;
}

// ConfirmedPreAck: 200 sent + replicated, ACK pending. The primary dies before
// the ACK; the ACK routes to the backup, which absorbs it (takeover). This is
// the "switch on backup for the 200/ACK sequence then back to nominal" case.
transparent_matrix! {
    confirmed_pre_ack__nothing__kill__stay_dead:
        ConfirmedPreAck, Nothing, Kill, StayDead, 0;
    confirmed_pre_ack__nothing__kill__reboot_after_takeover:
        ConfirmedPreAck, Nothing, Kill, RebootAfterTakeover, 0;
    confirmed_pre_ack__bye_alice__kill__reboot_after_takeover:
        ConfirmedPreAck, Bye(Party::Caller), Kill, RebootAfterTakeover, 0;
    confirmed_pre_ack__nothing__drain__reboot_after_takeover:
        ConfirmedPreAck, Nothing, Drain, RebootAfterTakeover, 0;
}

// Graceful-drain fault: the same transparency must hold when the primary drains
// (grace window) before the pod terminates, rather than crashing abruptly.
transparent_matrix! {
    established__bye_alice__drain__reboot_after_takeover:
        Established, Bye(Party::Caller), Drain, RebootAfterTakeover, 0;
    established__generic_reinvite_alice__drain__reboot_after_takeover:
        Established, Generic { method: sip_message::generators::InDialogMethod::Invite, from: Party::Caller }, Drain, RebootAfterTakeover, 0;
    established__nothing__drain__reboot_no_traffic:
        Established, Nothing, Drain, RebootNoTraffic, 0;
}
