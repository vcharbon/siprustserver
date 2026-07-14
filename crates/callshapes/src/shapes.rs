//! The shipped catalog, REGENERATED through the pipeline algebra — same ids,
//! same downstream contract (`docs/todos/actor-harness-p1-contract-table.md`),
//! byte-for-byte on the wire as the historic hand-written bodies in
//! `scenario_harness::actor::scenarios`. This is the phase-B equivalence
//! proof: the whole existing test surface (loadgen smoke + fake-net, actor
//! tests, e2e) runs these compositions via the shape registry.
//!
//! Every constructor takes the platform [`RouteBinder`]; upstream callers pass
//! [`EgressBinder`](crate::EgressBinder) (see [`default_binder`]).

use std::sync::Arc;

use crate::binder::RouteBinder;
use crate::plan::{
    ByeFeed, DwellKnob, Establishment, Script, ShapePlan, Stage, Teardown, Transfer,
};
use crate::EgressBinder;

/// The upstream default binder, shared across the shipped catalog.
pub fn default_binder() -> Arc<dyn RouteBinder> {
    Arc::new(EgressBinder)
}

/// `basic_call` — transparent establishment, talk, BYE (contract §5.1).
pub fn basic_call(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "basic_call",
        binder,
        establish: Establishment::Transparent,
        stages: vec![],
        teardown: Teardown::CallerBye {
            after: DwellKnob::TalkTime,
            feed: ByeFeed::CheckpointAndPhase,
        },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// `reinvite` — transparent establishment, one delayed-offer re-INVITE
/// renegotiation, BYE (contract §5.2). The n=1 special case of [`reinvite_n`].
pub fn reinvite(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    reinvite_n(binder, "reinvite", 1)
}

/// `reinvite_n` — transparent establishment, then `n` **serialized**
/// delayed-offer re-INVITE renegotiations (each gated on the previous one
/// completing, so no two are ever in flight — C6), BYE. The shipped `reinvite10`
/// shape (the "10 re-INVITEs" ask) is `reinvite_n(.., "reinvite10", 10)`; the
/// `reinvite`/`reinvite_em` shapes are the n=1 case. `id` is the shape's stable
/// id (report/metrics label) so each ×N variant is distinctly addressable.
pub fn reinvite_n(binder: Arc<dyn RouteBinder>, id: &'static str, n: u32) -> ShapePlan {
    ShapePlan {
        id,
        binder,
        establish: Establishment::Transparent,
        stages: vec![Stage::Script(Script::Reinvite { n })],
        teardown: Teardown::CallerBye {
            after: DwellKnob::ReinviteGap,
            feed: ByeFeed::CheckpointAndPhase,
        },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// `crossing_bye` — transparent establishment, talk, then a CROSSING BYE (C3/S3,
/// RFC 3261 §15.1.2): the caller and the winning callee both hang up at the same
/// instant, so each BYE crosses the peer's in flight.
pub fn crossing_bye(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "crossing_bye",
        binder,
        establish: Establishment::Transparent,
        stages: vec![],
        teardown: Teardown::CrossingBye { after: DwellKnob::TalkTime },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// `forked` — TRUE forking (RFC 3261 §12.1.2): bob emits three distinct-tag
/// 18x on one INVITE server txn, wins under the middle tag; talk, BYE. Only
/// valid under the SUT's transparent CORE relay (E3 — see
/// [`Establishment::Forked`]).
pub fn forked(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "forked",
        binder,
        establish: Establishment::Forked {
            tags: &["fk1", "fk2", "fk3"],
            winner: "fk2",
            reliable: false,
            loser_late_200: None,
        },
        stages: vec![],
        teardown: Teardown::CallerBye {
            after: DwellKnob::TalkTime,
            feed: ByeFeed::CheckpointAndPhase,
        },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// `forked_loser_late_200` — forking where a LOSING fork also sends a late 200
/// (§13.2.2.4): the caller ACKs then BYEs the loser while the winning dialog
/// lives on; talk, BYE.
///
/// **PEER-TO-PEER ONLY — not a loadgen-through-B2BUA shape.** A dialog-
/// terminating B2BUA forwards only the FIRST 2xx of its b-leg INVITE to the
/// caller and absorbs the loser's late 200, so the caller never sees it and
/// the ACK+BYE-the-loser path is never exercised through a SUT (the losing
/// fork then dangles on the callee and the call settles NOK). This shape is
/// kept as a valid composition for a peer-to-peer harness; the actual
/// loser-late-200 behavior is pinned SUT-less by
/// `scenario_harness::actor::tests::{forking_ring_loser_late_200_is_acked_and_byed,
/// actor_caller_acks_and_byes_losing_fork_late_200}`. It is deliberately NOT in
/// the loadgen registry.
pub fn forked_loser_late_200(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "forked_loser_late_200",
        binder,
        establish: Establishment::Forked {
            tags: &["fk1", "fk2", "fk3"],
            winner: "fk2",
            reliable: false,
            loser_late_200: Some("fk3"),
        },
        stages: vec![],
        teardown: Teardown::CallerBye {
            after: DwellKnob::TalkTime,
            feed: ByeFeed::CheckpointAndPhase,
        },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// `forked_reliable` — forking where each fork's 18x is a reliable 183 the
/// caller PRACKs per early dialog (RFC 3262 §5); wins under the middle tag,
/// talk, BYE.
pub fn forked_reliable(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "forked_reliable",
        binder,
        establish: Establishment::Forked {
            tags: &["fk1", "fk2", "fk3"],
            winner: "fk2",
            reliable: true,
            loser_late_200: None,
        },
        stages: vec![],
        teardown: Teardown::CallerBye {
            after: DwellKnob::TalkTime,
            feed: ByeFeed::CheckpointAndPhase,
        },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// `prack_update` — reliable (100rel) establishment, one in-dialog UPDATE
/// renegotiation, BYE (contract §5.6).
pub fn prack_update(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "prack_update",
        binder,
        establish: Establishment::Reliable,
        stages: vec![Stage::Script(Script::UpdatePostConnect)],
        teardown: Teardown::CallerBye {
            after: DwellKnob::ReinviteGap,
            feed: ByeFeed::CheckpointAndPhase,
        },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// `rerouting_prack` — bob 486s, the SUT fails over to bob2, which answers
/// RELIABLY; talk, BYE (contract §5.7).
pub fn rerouting_prack(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "rerouting_prack",
        binder,
        establish: Establishment::RerouteOnReject { reject: 486, winner_reliable: true },
        stages: vec![],
        teardown: Teardown::CallerBye {
            after: DwellKnob::TalkTime,
            feed: ByeFeed::CheckpointAndPhase,
        },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// `options_hold` — transparent establishment, an OPTIONS keepalive loop for
/// the hold, BYE (contract §5.4).
pub fn options_hold(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "options_hold",
        binder,
        establish: Establishment::Transparent,
        stages: vec![Stage::Script(Script::KeepaliveLoop)],
        teardown: Teardown::CallerBye { after: DwellKnob::None, feed: ByeFeed::CheckpointAndPhase },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// `long_call` — transparent establishment, ONE keepalive ping, survive the
/// long hold (reactors answering SUT keepalives), tolerant BYE — the BYE
/// stamps its checkpoint but no phase: the terminal phase stays
/// `keepalive_ack` (contract §5.5).
pub fn long_call(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "long_call",
        binder,
        establish: Establishment::Transparent,
        stages: vec![Stage::Script(Script::KeepaliveOnce)],
        teardown: Teardown::CallerBye {
            after: DwellKnob::LongHold,
            feed: ByeFeed::CheckpointOnly,
        },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// `refer` — transparent establishment, blind transfer to charlie, BYE once
/// the post-transfer media merge completes (contract §5.3: no `connected`
/// phase, no ringing gate, no BYE feed).
pub fn refer(binder: Arc<dyn RouteBinder>, refer_key: impl Into<String>) -> ShapePlan {
    ShapePlan {
        id: "refer",
        binder,
        establish: Establishment::Transparent,
        stages: vec![Stage::Transfer(Transfer::Blind { refer_key: refer_key.into() })],
        teardown: Teardown::CallerBye { after: DwellKnob::None, feed: ByeFeed::NoFeed },
        ringing_gate: false,
        stamp_connected: false,
    }
}

/// `refer_charlie_reject` — the transfer target DECLINES (603); the original
/// dialog is BYE'd once the decline is observed (contract §5.10).
pub fn refer_charlie_reject(
    binder: Arc<dyn RouteBinder>,
    refer_key: impl Into<String>,
) -> ShapePlan {
    ShapePlan {
        id: "refer_charlie_reject",
        binder,
        establish: Establishment::Transparent,
        stages: vec![Stage::Transfer(Transfer::BlindDeclined {
            refer_key: refer_key.into(),
            code: 603,
        })],
        teardown: Teardown::CallerBye { after: DwellKnob::None, feed: ByeFeed::NoFeed },
        ringing_gate: false,
        stamp_connected: false,
    }
}

/// `invite_reject` — bob 486s the initial INVITE; terminal (contract §5.8).
pub fn invite_reject(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "invite_reject",
        binder,
        establish: Establishment::RejectTerminal { code: 486 },
        stages: vec![],
        teardown: Teardown::None,
        ringing_gate: false,
        stamp_connected: false,
    }
}

/// `abandon_ringing` — the caller CANCELs after the 180; terminal
/// (contract §5.9).
pub fn abandon_ringing(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "abandon_ringing",
        binder,
        establish: Establishment::AbandonAfterRinging,
        stages: vec![],
        teardown: Teardown::None,
        ringing_gate: false,
        stamp_connected: false,
    }
}

/// `cancel_answer_crossing` — the caller CANCELs while the callee answers (C2/E5,
/// RFC 3261 §9.2): a branch-aware race whose terminal is EITHER a confirmed +
/// torn-down call (the 200 crossed the CANCEL) OR the abandoned terminal (the
/// CANCEL won). Terminal-style; the load lane accepts whichever legal branch
/// occurred ([`Expect::EitherOf`](scenario_harness::actor::Expect::EitherOf)).
pub fn cancel_answer_crossing(binder: Arc<dyn RouteBinder>) -> ShapePlan {
    ShapePlan {
        id: "cancel_answer_crossing",
        binder,
        establish: Establishment::CancelAnswerCrossing,
        stages: vec![],
        teardown: Teardown::None,
        ringing_gate: false,
        stamp_connected: false,
    }
}
