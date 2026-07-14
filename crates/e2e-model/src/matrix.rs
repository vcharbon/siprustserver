//! The **declared compatibility matrix** that GENERATES the establishment ×
//! script cross-product of load shapes (callshapes program §7 / phase D1).
//!
//! Rather than hand-listing every `Establishment`+`Script` combination, the
//! axes are declared as data ([`ESTS`], [`SCRS`]) and a compatibility predicate
//! ([`compatible`]) gates the cross-product; [`generated_shapes`] composes each
//! legal cell through the callshapes pipeline algebra
//! ([`ShapePlan`](callshapes::plan::ShapePlan)) into a
//! [`ShapeDescriptor`](crate::registry::ShapeDescriptor) with a **stable
//! generated id** (`"<establishment>+<script>"`, e.g. `forked+reinvite`).
//!
//! # SUT-reachability
//!
//! Every cell here establishes a real dialog and renegotiates on it, so all are
//! reachable through a dialog-terminating B2BUA. The **peer-to-peer-only**
//! behaviours (`forked_loser_late_200`, the S5/S6 glare shapes) are NOT part of
//! this matrix — they have no through-SUT load cell by design (their coverage
//! is the SUT-less machinery tests). See the callshapes README.
//!
//! # Ids are interned once (no per-registry leak)
//!
//! The composed ids live in a process-lifetime `OnceLock<Vec<String>>`, so
//! `with_defaults()` — called once per test registry — reuses the same `'static`
//! strings instead of leaking a fresh set each call.

use std::sync::{Arc, OnceLock};

use callshapes::plan::{
    ByeFeed, DwellKnob, Establishment, Script, ShapePlan, Stage, Teardown,
};
use callshapes::shapes::default_binder;
use scenario_harness::actor::ActorScenario;

use crate::registry::ShapeDescriptor;
use crate::shape::Anchor;

/// One establishment axis point.
#[derive(Clone, Copy)]
struct Est {
    /// The id fragment (`"reliable"`, `"forked"`, `"reroute"`).
    frag: &'static str,
    /// The callshapes establishment this cell composes with.
    make: Establishment,
    /// This establishment needs a second callee receiver (`bob2`).
    needs_bob2: bool,
    /// Reliable (100rel) establishment — publishes the `Prack` anchor.
    reliable: bool,
}

/// One in-dialog script axis point.
#[derive(Clone, Copy)]
struct Scr {
    /// The id fragment (`"reinvite"`, `"update"`).
    frag: &'static str,
    /// The callshapes in-dialog script this cell runs on the current dialog.
    script: Script,
    /// Publishes the `ReInvite` anchor.
    adds_reinvite: bool,
}

/// The distinct fork To-tags a forked establishment emits (RFC 3261 §12.1.2).
const FORK_TAGS: &[&str] = &["fk1", "fk2", "fk3"];

/// The establishment axis. Reliable (E2), true forking (E3), reroute-on-reject
/// (E4 — the reject arm sends NO provisional, so this already models
/// "reroute-no-18x": bob's `Disposition::Reject` answers the final directly),
/// reroute-on-no-answer (E6/047 — ring-then-silent primary, SUT-timer-driven).
const ESTS: &[Est] = &[
    Est { frag: "reliable", make: Establishment::Reliable, needs_bob2: false, reliable: true },
    Est {
        frag: "forked",
        make: Establishment::Forked {
            tags: FORK_TAGS,
            winner: "fk2",
            reliable: false,
            loser_late_200: None,
        },
        needs_bob2: false,
        reliable: false,
    },
    Est {
        frag: "reroute",
        make: Establishment::RerouteOnReject { reject: 486, winner_reliable: false },
        needs_bob2: true,
        reliable: false,
    },
    // E6 (047): NO-ANSWER-triggered failover — bob rings then never answers,
    // the SUT's per-route no-answer timer CANCELs it and walks to bob2.
    Est {
        frag: "reroute_noanswer",
        make: Establishment::RerouteOnNoAnswer {
            no_answer_sec: callshapes::shapes::NOANSWER_RING_SEC,
            winner_reliable: false,
        },
        needs_bob2: true,
        reliable: false,
    },
];

/// The in-dialog script axis: re-INVITE (S1) and post-connect UPDATE (S2).
const SCRS: &[Scr] = &[
    Scr { frag: "reinvite", script: Script::Reinvite { n: 1 }, adds_reinvite: true },
    Scr { frag: "update", script: Script::UpdatePostConnect, adds_reinvite: false },
];

/// Whether the (establishment, script) cell is legal AND not redundant with a
/// canonical shape. `reliable+update` reproduces the canonical `prack_update`
/// flow, so it is excluded (the canonical id keeps its report/metrics label).
fn compatible(e: &Est, s: &Scr) -> bool {
    !(e.frag == "reliable" && s.frag == "update")
}

// ── anchor sets per cell shape (advisory sampling metadata) ─────────────────
const REINVITE_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::ReInvite,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Bye,
];
const RELIABLE_REINVITE_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::Prack,
    Anchor::ReInvite,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Bye,
];
const UPDATE_ANCHORS: &[Anchor] =
    &[Anchor::InitialInvite, Anchor::FirstProvisional, Anchor::Answer, Anchor::Ack, Anchor::Bye];

fn anchor_set(e: &Est, s: &Scr) -> &'static [Anchor] {
    match (e.reliable, s.adds_reinvite) {
        (true, true) => RELIABLE_REINVITE_ANCHORS,
        (false, true) => REINVITE_ANCHORS,
        (_, false) => UPDATE_ANCHORS,
    }
}

/// The composed ids, interned once for the process lifetime.
static MATRIX_IDS: OnceLock<Vec<String>> = OnceLock::new();

fn cell_ids() -> &'static [String] {
    MATRIX_IDS
        .get_or_init(|| {
            let mut ids = Vec::new();
            for e in ESTS {
                for s in SCRS {
                    if compatible(e, s) {
                        ids.push(format!("{}+{}", e.frag, s.frag));
                    }
                }
            }
            ids
        })
        .as_slice()
}

/// Compose one cell's [`ShapePlan`] — establishment → the in-dialog script →
/// caller BYE. `id` is the interned generated id (the plan's report label).
fn build_plan(id: &'static str, est: Establishment, script: Script) -> ShapePlan {
    ShapePlan {
        id,
        binder: default_binder(),
        establish: est,
        stages: vec![Stage::Script(script)],
        teardown: Teardown::CallerBye {
            after: DwellKnob::ReinviteGap,
            feed: ByeFeed::CheckpointAndPhase,
        },
        ringing_gate: true,
        stamp_connected: true,
    }
}

/// The generated cross-product cells, as id-addressable
/// [`ShapeDescriptor`]s (no mix weight — the default mix samples a
/// representative subset via the canonical + phase-C shapes; the full matrix is
/// addressable by id for targeted runs and the phase-D loss soaks). The
/// `reroute` cells carry `needs_bob2` so a rig that binds the second receiver
/// can drive them.
pub(crate) fn generated_shapes() -> Vec<ShapeDescriptor> {
    let ids = cell_ids();
    let mut out = Vec::new();
    let mut i = 0;
    for e in ESTS {
        for s in SCRS {
            if !compatible(e, s) {
                continue;
            }
            let id: &'static str = ids[i].as_str();
            i += 1;
            let (est, script) = (e.make, s.script);
            let mut d = ShapeDescriptor::new(id).anchors(anchor_set(e, s));
            if e.needs_bob2 {
                d = d.needs_bob2();
            }
            out.push(d.load_shared(Arc::new(build_plan(id, est, script)) as Arc<dyn ActorScenario>));
        }
    }
    out
}

/// The generated ids in declaration order — for tests / catalog listings.
pub fn generated_ids() -> &'static [String] {
    cell_ids()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ScenarioInputs;

    #[test]
    fn matrix_generates_the_compatible_cross_product() {
        let ids: Vec<&str> = generated_ids().iter().map(String::as_str).collect();
        // 4 establishments × 2 scripts − reliable+update (== prack_update) = 7.
        assert_eq!(
            ids,
            vec![
                "reliable+reinvite",
                "forked+reinvite",
                "forked+update",
                "reroute+reinvite",
                "reroute+update",
                "reroute_noanswer+reinvite",
                "reroute_noanswer+update",
            ],
            "the compatibility matrix generates exactly the legal cells",
        );
    }

    #[test]
    fn generated_cells_build_a_valid_plan_with_the_generated_id() {
        // Each generated descriptor mints a load body whose id is the cell id
        // (the report/metrics label).
        let inputs = ScenarioInputs::default();
        for d in generated_shapes() {
            let body = d.load_scenario(&inputs).expect("generated cell has a load body");
            assert_eq!(body.id(), d.id, "the body id is the generated cell id");
        }
    }

    #[test]
    fn reroute_cells_declare_the_second_receiver() {
        let reroute: Vec<&'static str> = generated_shapes()
            .iter()
            .filter(|d| d.id.starts_with("reroute"))
            .map(|d| d.id)
            .collect();
        assert_eq!(
            reroute,
            vec![
                "reroute+reinvite",
                "reroute+update",
                "reroute_noanswer+reinvite",
                "reroute_noanswer+update",
            ]
        );
        assert!(
            generated_shapes().iter().filter(|d| d.id.starts_with("reroute")).all(|d| d.needs_bob2),
            "reroute cells need bob2",
        );
    }
}
