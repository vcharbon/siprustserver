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

use b2bua_harness::{model_gate, B2buaScene, B2buaSut};
use callshapes::generator::{self, Cell, Ending, Winner};
use callshapes::shapes::default_binder;
use callshapes::CancelAt;
use futures::FutureExt;
use scenario_harness::actor::ScenarioVerdict;
use scenario_harness::realcall::{run_actor_collecting, CallEnv};

/// The port the transfer target, charlie, answers on.
const CHARLIE_PORT: u16 = 5090;

/// A scene whose B2BUA routes every call to bob and authorizes a transfer
/// (`generator::REFER_KEY`), with charlie bound as the transfer target.
pub(crate) async fn refer_scene(name: &str) -> (B2buaScene, scenario_harness::Agent) {
    let scene = B2buaScene::with_b2bua(name, |bob_port| {
        B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
    })
    .await;
    let charlie =
        scene.h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await.transfer_target();
    (scene, charlie)
}

/// Which cells a pending SUT defect fails.
type CellsOf = fn(&Cell) -> bool;

/// Cells a SUT defect fails, each with the defect it names, pending the
/// decision on the defect: skipped and counted, never run green. Empty while
/// every compatible cell passes.
const PENDING_SUT_DEFECTS: &[(CellsOf, &str)] = &[];

/// Drive one cell to its end and run every net over it. A failure names the
/// model invariant a SUT turn broke, when one did, before what the call met.
async fn run_cell(cell: Cell) {
    let since = model_gate::Mark::now();
    let (scene, charlie) = refer_scene(&format!("cell {cell}")).await;
    let env = CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        Some(&charlie),
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

/// Part `part` (0..`of`) of the generator's fixed sample: the cells from
/// `len * part / of` up to `len * (part + 1) / of`, so the `of` parts hold
/// every cell of the sample exactly once and differ in size by one cell at
/// most.
fn sample_part(part: usize, of: usize) -> Vec<Cell> {
    let sample = generator::sample();
    let len = sample.len();
    sample[len * part / of..len * (part + 1) / of].to_vec()
}

/// One default-lane test per name, each running its part of the fixed sample
/// (every triple of dimension values), so the parts run in parallel and each
/// stays well inside the lane's per-test timeout on a loaded box. The part
/// count, `SAMPLE_PARTS`, is the number of names. The first part opens with
/// the callee's early UPDATEs then its re-INVITE, the sample's first cell.
macro_rules! sample_parts {
    ($($name:ident),+ $(,)?) => {
        const SAMPLE_PARTS: usize = [$(stringify!($name)),+].len();
        sample_parts!(@part 0; $($name),+);
    };
    (@part $part:expr; $name:ident $(, $rest:ident)*) => {
        #[tokio::test(start_paused = true)]
        async fn $name() {
            run_cells(sample_part($part, SAMPLE_PARTS)).await;
        }
        sample_parts!(@part $part + 1; $($rest),*);
    };
    (@part $part:expr;) => {};
}

sample_parts!(
    the_sample_of_call_shapes_part_01,
    the_sample_of_call_shapes_part_02,
    the_sample_of_call_shapes_part_03,
    the_sample_of_call_shapes_part_04,
    the_sample_of_call_shapes_part_05,
    the_sample_of_call_shapes_part_06,
    the_sample_of_call_shapes_part_07,
    the_sample_of_call_shapes_part_08,
    the_sample_of_call_shapes_part_09,
    the_sample_of_call_shapes_part_10,
    the_sample_of_call_shapes_part_11,
    the_sample_of_call_shapes_part_12,
    the_sample_of_call_shapes_part_13,
    the_sample_of_call_shapes_part_14,
    the_sample_of_call_shapes_part_15,
    the_sample_of_call_shapes_part_16,
);

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

/// The one cell `SHAPE_CELL` names by its label (`by=… … xfer=…`), when set:
/// a failing cell of the lanes above, run alone.
#[tokio::test(start_paused = true)]
async fn the_cell_named_by_shape_cell() {
    let Ok(label) = std::env::var("SHAPE_CELL") else { return };
    let cell = generator::cells()
        .into_iter()
        .find(|c| c.to_string() == label)
        .unwrap_or_else(|| panic!("no cell is labelled {label:?}"));
    run_cell(cell).await;
}
