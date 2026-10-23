//! One cell of the sweep: the shape's call placed on [`PRIMARY`], cut at one
//! turn point by one incident, driven until the cluster holds nothing of it,
//! and judged. Every check panics: the RFC audit and the model invariants when
//! the harness drops; here, no node holding anything of the call, no limiter
//! set left, its CDR count — exactly one, or up to two in a split cell — and
//! the shape's check of it.

use std::time::Duration;

use scenario_harness::actor::run_actor_scenario;
use scenario_harness::realcall::CallCtx;

use super::parties::Parties;
use super::{Incident, SweepShape};
use crate::cluster::Cluster;
use crate::roles::Placement;
use crate::turn_points::{HeldTurn, Turn, TurnAt};

/// The worker every cell places its call on; the other is its backup.
pub const PRIMARY: &str = "b1";

/// How long a split lasts before it heals ([`Incident::SplitHealed`]).
const HEAL_AFTER: Duration = Duration::from_secs(1);
/// The step the cell's clock moves by once the parties are done, answering
/// what the cluster still sends between steps.
const SETTLE_STEP: Duration = Duration::from_millis(200);
/// How long the cluster has to forget the call once the parties are done:
/// past a keepalive interval and its timeout, and past the replica TTL a reap
/// waits out.
const SETTLE_BUDGET: Duration = Duration::from_secs(900);

/// Where and how a cell is cut.
#[derive(Clone, Copy, Debug)]
pub struct Cut {
    pub at: TurnAt,
    pub incident: Incident,
}

/// Run one cell named `name`; `None` runs the shape uncut. `report` keeps the
/// cell's unified report under `target/seq-reports/`. Returns the turns the
/// call took at its primary. Panics on any finding.
pub async fn run_cell(
    shape: &dyn SweepShape,
    name: &str,
    cut: Option<Cut>,
    report: bool,
) -> Vec<Turn> {
    let mut cluster = Cluster::start(name, &shape.recipe()).await;
    if !report {
        cluster.fh.disarm_report_on_drop();
    }
    let parties = Parties::bind(&cluster.fh, &shape.shared_callees()).await;
    let placement = cluster.place_next_call(&parties.alice, PRIMARY);
    let turn_points = cluster.fh.turn_points().clone();
    let mut reached = cut.map(|c| turn_points.arm(c.at));

    let scenario = shape.scenario();
    let mut env = parties.env(cluster.proxy.addr(), name);
    shape.call_env(&mut env);
    let ctx = CallCtx::new();
    let run = run_actor_scenario(&*scenario, &env, &ctx);
    tokio::pin!(run);
    let heal = tokio::time::sleep(Duration::MAX);
    tokio::pin!(heal);

    let mut verdict = None;
    let mut heal_pending = false;
    let mut call_ref: Option<String> = None;
    let mut settled = Duration::ZERO;
    loop {
        tokio::select! {
            biased;
            held = async { reached.as_mut().expect("guarded").await }, if reached.is_some() => {
                reached = None;
                let held = held.expect("the turn points keep the arm");
                let cut = cut.expect("armed");
                strike(&mut cluster, &placement, cut.incident, held);
                if cut.incident == Incident::SplitHealed {
                    heal.as_mut().reset(tokio::time::Instant::now() + HEAL_AFTER);
                    heal_pending = true;
                }
            }
            () = &mut heal, if heal_pending => {
                heal_pending = false;
                cluster.heal(&placement);
            }
            v = &mut run, if verdict.is_none() => {
                verdict = Some(v);
            }
            () = tokio::time::sleep(SETTLE_STEP), if verdict.is_some() => {
                parties.answer_strays().await;
                settled += SETTLE_STEP;
                let call = call_ref.get_or_insert_with(|| first_call(&cluster, name));
                let clear = cluster.is_clear_of(call).await;
                if (clear && reached.is_none() && !heal_pending) || settled >= SETTLE_BUDGET {
                    break;
                }
            }
        }
    }

    if let Some(cut) = cut.filter(|_| reached.is_some()) {
        panic!("[{name}] the call never reached {}", cut.at.label());
    }
    let call_ref = call_ref.unwrap_or_else(|| first_call(&cluster, name));
    let verdict = verdict.expect("the parties finished");
    eprintln!("[{name}] the parties ended: {verdict:?}");
    assert_eq!(
        call::call_ref_primary(&call_ref),
        Some(placement.primary.as_str()),
        "[{name}] the call landed off its placement"
    );
    crate::assert_call_fully_released(&cluster.nodes(), &call_ref).await;
    let held = cluster.limiter.stats().current_total;
    assert_eq!(held, 0, "[{name}] the limiter still holds {held} set(s) of the call");
    let cdrs: Vec<_> = cluster
        .nodes()
        .iter()
        .flat_map(|n| n.cdr_records())
        .filter(|r| r.call_ref == call_ref)
        .collect();
    // A split call's two copies may each write its CDR, or neither: no CDR
    // is deduplicated, and its loss in a split is accepted.
    let split = cut.is_some_and(|c| matches!(c.incident, Incident::Split | Incident::SplitHealed));
    match (cdrs.len(), split) {
        (1, _) | (0..=2, true) => {}
        (n, _) => panic!("[{name}] the call left {n} CDRs"),
    }
    if let Some(cdr) = cdrs.first() {
        if let Err(why) = shape.check_cdr(cdr) {
            panic!("[{name}] the CDR is wrong: {why}");
        }
    }
    turn_points.turns(&call_ref)
}

/// The call the cell placed: the first that took a turn.
fn first_call(cluster: &Cluster, name: &str) -> String {
    cluster
        .fh
        .turn_points()
        .first_call()
        .map(|(call_ref, _)| call_ref)
        .unwrap_or_else(|| panic!("[{name}] no call took a turn"))
}

/// Befall the call's owners with `incident` while `held` stands at its turn
/// point. A node the incident kills stays frozen there; any other goes on.
fn strike(cluster: &mut Cluster, placement: &Placement, incident: Incident, held: HeldTurn) {
    let victim = match incident {
        Incident::PrimaryKilled => {
            cluster.kill(&placement.primary, &placement.backup);
            Some(placement.primary.as_str())
        }
        Incident::BackupKilled => {
            cluster.kill(&placement.backup, &placement.primary);
            Some(placement.backup.as_str())
        }
        Incident::Split | Incident::SplitHealed => {
            cluster.split(placement);
            None
        }
    };
    if victim != Some(held.node.as_str()) {
        held.release();
    }
}
