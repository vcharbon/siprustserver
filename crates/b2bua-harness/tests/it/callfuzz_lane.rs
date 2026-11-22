//! The seeded call-flow fuzzer (`callfuzz`) over the generated call shapes,
//! driven through the B2BUA's transparent relay on the paused clock. Each
//! call draws a compatible cell, then everything [`FuzzWrap`] draws: held
//! callees with forks, 100rel and provisional trains, a caller CANCEL,
//! jittered dwells, hostile headers, odd INFO bodies, the offer in the INVITE
//! or not, and descriptions framed in multipart bodies. Every call is judged
//! as a cell is ([`super::call_shape_cells`]): its accepted ending, the RFC
//! audit, the SUT's per-turn model invariants, the leak and reap checks.
//!
//! The fuzzer is random and runs only when asked: with `CALLFUZZ_CALLS`
//! unset the test returns at once. `CALLFUZZ_SEED` is the run seed (default
//! 1); `CALLFUZZ_FROM` the first call's index (default 0), so a campaign runs
//! as batches under the test timeout; `CALLFUZZ_CALL_SEED` replays one call.
//! A failure names each failing call's seed and draw.
//!
//! `CALLFUZZ_DROP` and `CALLFUZZ_LATE` (percent of datagrams, default 0) put
//! the call on a lossy network ([`faults`]); a call replays with its faults,
//! which its draw counts. A call one of whose requests a later one overtook
//! after a loss, refused 500 for it, may fail on that request's transaction
//! or its direct consequence on the request's leg; the other nets judge it
//! still ([`overtaken`]).

mod faults;
mod overtaken;

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use b2bua_harness::model_gate;
use callfuzz::{FuzzConfig, FuzzWrap, Policy, Rng, CANCEL_ENDINGS};
use callshapes::generator;
use callshapes::shapes::default_binder;
use futures::FutureExt;
use scenario_harness::actor::ScenarioVerdict;
use scenario_harness::realcall::{run_actor_collecting, CallEnv};
use scenario_harness::{Harness, SIMULATED_TRANSIT_DELAY_MS};

use faults::{fate_of, Faults, Tally};

/// The salt of the cell pick: a call's cell is drawn apart from its fuzz draw.
const CELL_SALT: u64 = 0xCE11_5A17_CE11_5A17;

fn knob(name: &str) -> Option<u64> {
    let v = std::env::var(name).ok()?;
    let parsed = match v.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => v.parse(),
    };
    Some(parsed.unwrap_or_else(|_| panic!("{name}={v:?} is not a number")))
}

/// The cell call `seed` draws among the compatible ones.
fn cell_of(seed: u64) -> generator::Cell {
    let cells: Vec<_> =
        generator::cells().into_iter().filter(|c| generator::compatible(c).is_ok()).collect();
    cells[Rng::new(seed ^ CELL_SALT).below(cells.len() as u64) as usize]
}

/// Drive the call of `token` to its end and run every net over it; a failure
/// panics with why, naming the model invariant a SUT turn broke first. A
/// scenario that failed on an overtaken request's 500 or its direct
/// consequence on that request's leg ([`overtaken::excuses`]) fails no
/// further than the nets after it say. A
/// transfer cell's parties hold their answers but never refuse: a refused
/// realign rolls the transfer back, an ending no transfer stage states.
async fn run_call(config: FuzzConfig, token: &str, cell: generator::Cell, net: Harness) {
    let wrap = FuzzWrap {
        id: "fz_cell",
        inner: Arc::new(generator::plan(&cell, default_binder())),
        policy: Policy {
            cancel_endings: Some(CANCEL_ENDINGS),
            callee_reliable: true,
            callee_roles: &["bob"],
        },
        config: FuzzConfig { refuse: cell.transfer == generator::Transferred::None, ..config },
    };
    let since = model_gate::Mark::now();
    let (scene, charlie) = super::call_shape_cells::refer_scene_on(net).await;
    let env = CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        Some(&charlie),
        scene.b2bua.addr,
        "X-Loadgen-Id",
        token,
    );
    if let ScenarioVerdict::Failure { at, cause } = run_actor_collecting(&wrap, &env).await {
        let broken = model_gate::first_violation(since)
            .map(|b| format!("red because {}; then ", b.lines().next().unwrap_or_default()));
        let events = scene.h.recording().channel().snapshot();
        let overtaken = overtaken::overtaken_requests(&events);
        let party = |role: &str| {
            [&scene.alice, &scene.bob, &charlie]
                .into_iter()
                .find(|p| p.name() == role)
                .map(|p| p.addr())
        };
        if broken.is_some() || !overtaken::excuses(&overtaken, &at, &cause, party) {
            panic!("{}failed at {at}: {cause:?}", broken.unwrap_or_default());
        }
        let seed = config.seed_of(token);
        eprintln!("[callfuzz {token}] seed {seed:#018x} overtaken, its 500 met at {at}: {cause:?}");
    }
    let report = scene.finish().await;
    assert!(report.passed(), "the RFC audit failed");
}

/// Run call `i`; `Err` names its seed, cell, draw and the first line of why.
async fn fuzz_one(config: FuzzConfig, faults: Faults, i: u64) -> Result<(), String> {
    let token = format!("fz-{i}");
    let seed = config.seed_of(&token);
    let cell = cell_of(seed);
    let label = cell.to_string();
    let name = format!("callfuzz {token}");
    let tally = Arc::new(Tally::default());
    let net = if faults.any() {
        let fate = fate_of(faults, seed, tally.clone());
        Harness::with_transit_delay_and_send_fate(name, SIMULATED_TRANSIT_DELAY_MS, fate)
    } else {
        Harness::new(name)
    };
    let outcome = AssertUnwindSafe(run_call(config, &token, cell, net)).catch_unwind().await;
    let draw = callfuzz::take_drawn()
        .into_values()
        .map(|d| format!("{d:?}"))
        .collect::<Vec<_>>()
        .join(" ")
        + &tally.note();
    let Err(payload) = outcome else { return Ok(()) };
    let why = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default();
    let why = why.lines().next().unwrap_or_default();
    Err(format!("seed {seed:#018x} cell {label}: {why}\n    draw: {draw}"))
}

/// `CALLFUZZ_CALLS` fuzzed calls, each through a fresh B2BUA.
#[tokio::test(start_paused = true)]
async fn fuzzed_calls_through_the_b2bua() {
    let Some(calls) = knob("CALLFUZZ_CALLS") else { return };
    let config = FuzzConfig {
        run_seed: knob("CALLFUZZ_SEED").unwrap_or(1),
        call_seed: knob("CALLFUZZ_CALL_SEED"),
        train: true,
        ..FuzzConfig::default()
    };
    let calls = if config.call_seed.is_some() { 1 } else { calls };
    let from = knob("CALLFUZZ_FROM").unwrap_or(0);
    let faults = Faults {
        drop: knob("CALLFUZZ_DROP").unwrap_or(0),
        late: knob("CALLFUZZ_LATE").unwrap_or(0),
    };
    let mut failed = Vec::new();
    for i in from..from + calls {
        if let Err(why) = fuzz_one(config, faults, i).await {
            failed.push(why);
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {calls} fuzzed calls failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
}
