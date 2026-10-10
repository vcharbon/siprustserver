//! The generated call shapes (`callshapes::generator`) driven through the
//! B2BUA's transparent relay on the paused clock, each a full call judged by
//! every net at once: the RFC audit, the simulated UAs' dialog tables, the
//! SUT's per-turn model invariants, and the leak and reap checks of
//! [`B2buaScene::finish`]. A failing cell is named by all its dimension
//! values, first.
//!
//! The default lane runs the generator's fixed sample; the slow lane runs
//! every compatible cell. Both are split into tests that run in parallel.

use std::panic::AssertUnwindSafe;

use b2bua_harness::{model_gate, B2buaScene};
use callshapes::generator::{self, Cell, Ending, Winner};
use callshapes::shapes::default_binder;
use callshapes::CancelAt;
use futures::FutureExt;
use scenario_harness::actor::ScenarioVerdict;
use scenario_harness::realcall::{run_actor_collecting, CallEnv};

/// Which cells a pending SUT defect fails.
type CellsOf = fn(&Cell) -> bool;

/// Cells a SUT defect fails, each with its FIXME, pending the decision on the
/// defect: skipped and counted, never run green. Empty while every
/// compatible cell passes.
const PENDING_SUT_DEFECTS: &[(CellsOf, &str)] = &[];

/// Drive one cell to its end and run every net over it. A failure names the
/// model invariant a SUT turn broke, when one did, before what the call met.
async fn run_cell(cell: Cell) {
    let since = model_gate::Mark::now();
    let scene = B2buaScene::new(&format!("cell {cell}")).await;
    let env = CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        None,
        scene.b2bua.addr,
        "X-Loadgen-Id",
        "cell-tok",
    );
    let plan = generator::plan(&cell, default_binder());
    if let ScenarioVerdict::Failure { at, cause } = run_actor_collecting(&plan, &env).await {
        match model_gate::first_violation(since) {
            Some(broken) => {
                let broken = broken.lines().next().unwrap_or_default().to_string();
                panic!("cell {cell}: red because {broken}; then failed at {at}: {cause:?}")
            }
            None => panic!("cell {cell}: failed at {at}: {cause:?}"),
        }
    }
    let report = scene.finish().await;
    assert!(report.passed(), "cell {cell}: the RFC audit failed");
}

/// Run every cell of `cells` and fail once, naming each failing cell with the
/// first line of why.
async fn run_cells(cells: Vec<Cell>) {
    let (mut ran, mut skipped, mut failed) = (0, 0, Vec::new());
    for cell in cells {
        if PENDING_SUT_DEFECTS.iter().any(|(holds, _)| holds(&cell)) {
            skipped += 1;
            continue;
        }
        ran += 1;
        if let Err(payload) = AssertUnwindSafe(run_cell(cell)).catch_unwind().await {
            let why = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_default();
            let why = why.lines().next().unwrap_or_default().to_string();
            failed.push(if why.starts_with("cell ") { why } else { format!("cell {cell}: {why}") });
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {ran} cells failed ({skipped} skipped pending a SUT decision):\n{}",
        failed.len(),
        failed.join("\n")
    );
}

/// The compatible cells ending with `ending` and won by `winner`.
fn cells_of(ending: Ending, winner: Option<Winner>) -> Vec<Cell> {
    generator::cells()
        .into_iter()
        .filter(|c| c.ending == ending && winner.is_none_or(|w| c.winner == w))
        .filter(|c| generator::compatible(c).is_ok())
        .collect()
}

/// The quarter `part` (0..4) of the generator's fixed sample.
fn sample_part(part: usize) -> Vec<Cell> {
    let sample = generator::sample();
    let quarter = sample.len().div_ceil(4);
    sample.into_iter().skip(part * quarter).take(quarter).collect()
}

/// The fixed sample, every triple of dimension values, in four parts that
/// run in parallel; the first holds the callee's early UPDATEs then its
/// re-INVITE.
#[tokio::test(start_paused = true)]
async fn the_sample_of_call_shapes_part_1() {
    run_cells(sample_part(0)).await;
}

#[tokio::test(start_paused = true)]
async fn the_sample_of_call_shapes_part_2() {
    run_cells(sample_part(1)).await;
}

#[tokio::test(start_paused = true)]
async fn the_sample_of_call_shapes_part_3() {
    run_cells(sample_part(2)).await;
}

#[tokio::test(start_paused = true)]
async fn the_sample_of_call_shapes_part_4() {
    run_cells(sample_part(3)).await;
}

/// One slow-lane test per slice of the compatible cells, so the slices run
/// in parallel.
macro_rules! slow_slice {
    ($name:ident, $ending:expr, $winner:expr) => {
        #[tokio::test(start_paused = true)]
        #[ignore = "slow lane: every generated call shape"]
        async fn $name() {
            run_cells(cells_of($ending, $winner)).await;
        }
    };
}

slow_slice!(caller_bye_sole, Ending::CallerBye, Some(Winner::Sole));
slow_slice!(caller_bye_first_fork, Ending::CallerBye, Some(Winner::First));
slow_slice!(caller_bye_later_fork, Ending::CallerBye, Some(Winner::Later));
slow_slice!(caller_bye_unrung_fork, Ending::CallerBye, Some(Winner::Unrung));
slow_slice!(callee_bye_sole, Ending::CalleeBye, Some(Winner::Sole));
slow_slice!(callee_bye_first_fork, Ending::CalleeBye, Some(Winner::First));
slow_slice!(callee_bye_later_fork, Ending::CalleeBye, Some(Winner::Later));
slow_slice!(callee_bye_unrung_fork, Ending::CalleeBye, Some(Winner::Unrung));
slow_slice!(cancel_before_1xx, Ending::Cancel(CancelAt::BeforeProvisional), None);
slow_slice!(cancel_crossing_1xx, Ending::Cancel(CancelAt::CrossingProvisional), None);
slow_slice!(cancel_after_1xx, Ending::Cancel(CancelAt::AfterProvisional), None);
slow_slice!(cancel_then_early_bye, Ending::Cancel(CancelAt::AfterProvisionalThenEarlyBye), None);
slow_slice!(cancel_crossing_final, Ending::Cancel(CancelAt::CrossingFinal), None);
slow_slice!(cancel_after_final, Ending::Cancel(CancelAt::AfterFinal), None);
