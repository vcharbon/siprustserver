//! One cell of the sweep: the shape's call placed on [`PRIMARY`], cut at one
//! turn point by one incident struck on the harness, driven until the cluster
//! holds nothing of it, and judged by the rows of the window table its
//! measured tier opens (ADR-0055 §11). Every check panics: the RFC audit, the
//! model invariants and the tier gate when the harness drops; here, the
//! parties' verdict, no node holding anything of the call, no limiter set
//! left, its CDR count and the shape's check of every CDR. A Guaranteed cell
//! fires no row, and the evidence rule ends none of its calls.

use std::sync::Mutex;
use std::time::Duration;

use b2bua::cdr::CdrRecord;
use b2bua::repl::SyncLevel;
use scenario_harness::actor::{
    run_actor_scenario_observed, ObservedState, ScenarioVerdict, StateInner,
};
use scenario_harness::realcall::CallCtx;
use sip_net_testkit::CaptureIndex;

use super::aftermath::Aftermath;
use super::parties::Parties;
use super::SweepShape;
use crate::cluster::Cluster;
use crate::incident::Incident;
use crate::roles::Placement;
use crate::turn_points::{HeldTurn, Released, Turn, TurnAt, TurnPoint};
use crate::windows::{self, CutFacts, Kind, Row, Strike, Tier};

/// The worker every cell places its call on; the other is its backup.
pub const PRIMARY: &str = "b1";

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

/// What a cell measured, kept whether it passes or not: its tier and the
/// rows that fired in it.
#[derive(Clone, Debug, Default)]
pub struct CellReport {
    pub tier: Option<Tier>,
    /// The ids of the rows that fired, in the order they fired, once each.
    pub fired: Vec<&'static str>,
    /// The evidence rule ended a call of the cell.
    pub evidence: bool,
}

impl CellReport {
    fn fire(&mut self, row: &'static Row) {
        if !self.fired.contains(&row.id) {
            self.fired.push(row.id);
        }
    }
}

/// Run one cell named `name` at the sync `level`; `None` runs the shape uncut.
/// `report` keeps the cell's unified report under `target/seq-reports/`.
/// What it measured lands in `measured` before any check. Returns the turns
/// the call took at its primary. Panics on any finding.
pub async fn run_cell(
    shape: &dyn SweepShape,
    name: &str,
    cut: Option<Cut>,
    level: SyncLevel,
    report: bool,
    measured: &Mutex<CellReport>,
) -> Vec<Turn> {
    let mut recipe = shape.recipe();
    if cut.is_some_and(|c| c.incident == Incident::UnbackedKilled) {
        // Every backup refuses every replica: each call has no backup.
        recipe.replica.backup.calls = Some(0);
    }
    let mut cluster = Cluster::start_at(name, &recipe, level).await;
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
    let run = run_actor_scenario_observed(&*scenario, &env, &ctx);
    tokio::pin!(run);

    let mut verdict = None;
    let mut struck = None;
    let mut aftermath = Aftermath::default();
    let mut call_ref: Option<String> = None;
    let mut settled = Duration::ZERO;
    let mut was_clear = false;
    loop {
        tokio::select! {
            biased;
            held = async { reached.as_mut().expect("guarded").await }, if reached.is_some() => {
                reached = None;
                let held = held.expect("the turn points keep the arm");
                let cut = cut.expect("armed");
                let strike = strike(&mut cluster, &placement, cut, held);
                if goal_drop_row(strike, level).is_some() {
                    ctx.accept_clean_ends();
                }
                struck = Some(strike);
                aftermath = Aftermath::of(cut.incident);
            }
            () = tokio::time::sleep_until(aftermath.next_at()), if aftermath.pending() => {
                aftermath.act(&mut cluster, &placement).await;
            }
            v = &mut run, if verdict.is_none() => {
                verdict = Some(v);
            }
            () = tokio::time::sleep(SETTLE_STEP), if verdict.is_some() => {
                let answered = parties.answer_strays().await;
                settled += SETTLE_STEP;
                let call = call_ref.get_or_insert_with(|| first_call(&cluster, name));
                let clear = cluster.is_clear_of(call).await;
                let pending = aftermath.pending();
                // A cluster clear for a whole step: what a party sent as the
                // cluster forgot the call (a hop ACK, a stray's answer) has
                // landed.
                let quiet = clear && reached.is_none() && !pending && !answered;
                if (quiet && was_clear) || settled >= SETTLE_BUDGET {
                    break;
                }
                was_clear = quiet;
            }
        }
    }

    if let Some(cut) = cut.filter(|_| reached.is_some()) {
        panic!("[{name}] the call never reached {}", cut.at.label());
    }
    let call_ref = call_ref.unwrap_or_else(|| first_call(&cluster, name));
    let (verdict, obs) = verdict.expect("the parties finished");
    eprintln!("[{name}] the parties ended: {verdict:?}");
    let turns = turn_points.turns(&call_ref);
    let cdrs: Vec<_> = cluster
        .nodes()
        .iter()
        .flat_map(|n| n.cdr_records())
        .filter(|r| r.call_ref == call_ref)
        .collect();

    let mut cell = CellReport { tier: cluster.fh.tier(), ..CellReport::default() };
    for fired in cluster.fh.fired_rows() {
        cell.fire(fired.row);
    }
    let mut failures = Vec::new();
    match (cut, struck) {
        (Some(cut), Some(strike)) => {
            let facts = cut_facts(&cluster, &placement, cut, &turns);
            if !verdict.is_success() {
                match outcome_row(strike, level, &facts, &verdict, &obs) {
                    Some(row) => cell.fire(row),
                    None => {
                        failures.push(format!("the parties failed: {verdict:?} {:?}", ctx.notes()))
                    }
                }
            }
            if obs.with_snapshot(StateInner::goal_dropped) {
                if let Some(row) = goal_drop_row(strike, level) {
                    cell.fire(row);
                }
            }
            match cdr_row(strike, level, cdrs.len()) {
                Ok(Some(row)) => cell.fire(row),
                Ok(None) => {}
                Err(()) => failures.push(format!("the call left {} CDRs", cdrs.len())),
            }
        }
        _ if !verdict.is_success() => {
            failures.push(format!("the parties failed: {verdict:?} {:?}", ctx.notes()))
        }
        _ if cdrs.len() != 1 => failures.push(format!("the call left {} CDRs", cdrs.len())),
        _ => {}
    }
    let every_cdr: Vec<_> = cluster.nodes().iter().flat_map(|n| n.cdr_records()).collect();
    cell.evidence = ended_on_evidence(&every_cdr);
    *measured.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = cell.clone();
    if let Some(why) = failures.first() {
        panic!("[{name}] {why}");
    }
    assert_eq!(
        call::call_ref_primary(&call_ref),
        Some(placement.primary.as_str()),
        "[{name}] the call landed off its placement"
    );
    crate::assert_call_fully_released(&cluster.nodes(), &call_ref).await;
    let held = cluster.limiter.stats().current_total;
    assert_eq!(held, 0, "[{name}] the limiter still holds {held} set(s) of the call");
    for cdr in &cdrs {
        if let Err(why) = shape.check_cdr(cdr) {
            panic!("[{name}] a CDR is wrong: {why}");
        }
    }
    if cell.tier == Some(Tier::Guaranteed) && (!cell.fired.is_empty() || cell.evidence) {
        let evidence = if cell.evidence { "; the evidence rule ended a call" } else { "" };
        panic!("[{name}] guaranteed tier: rows fired: {}{evidence}", cell.fired.join(", "));
    }
    turns
}

/// What the cell measured of its cut turn, read by the outcome rows.
fn cut_facts(cluster: &Cluster, placement: &Placement, cut: Cut, turns: &[Turn]) -> CutFacts {
    let turn = turns.get(cut.at.turn - 1);
    // The sub-points reach the write applied and the effects left in the
    // order the turn's release says: under `applied` with no cap hit the
    // write is applied before the effects leave, else after.
    let held_and_left = match cut.at.point {
        TurnPoint::Logged => false,
        TurnPoint::Left => turn.is_some_and(|t| t.released == Some(Released::Applied)),
        TurnPoint::Applied => turn.is_some_and(|t| t.released.is_some()),
    };
    let held = held_and_left || cut.at.point == TurnPoint::Applied;
    // The copy sees no later turn whose write the backup never applied.
    let later_applied = turns.iter().skip(cut.at.turn).any(|t| t.applied);
    let living: Vec<_> = cluster
        .workers
        .iter()
        .filter(|w| w.ordinal() != placement.primary)
        .map(|w| w.sip_addr())
        .collect();
    CutFacts {
        held_and_left,
        held,
        on_request: turn.is_none_or(|t| t.on_request),
        answer_in_flight: turn.is_some_and(|t| t.called_out || t.awaits_answer) && !later_applied,
        living_answered: super::answered::living_workers_answered(
            &CaptureIndex::new(cluster.fh.sip_entries()),
            &living,
            cluster.node(&placement.primary).sip_addr(),
        ),
    }
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

/// Befall the call's owners with `cut`'s incident while `held` stands at its
/// turn point, and strike it on the harness. A node the incident kills stays
/// frozen there; any other goes on. Returns the strike.
fn strike(cluster: &mut Cluster, placement: &Placement, cut: Cut, held: HeldTurn) -> Strike {
    let strike = strike_of(cluster, &held, cut);
    cluster.fh.strike(strike);
    let victim = match cut.incident {
        Incident::PrimaryKilled | Incident::UnbackedKilled => {
            cluster.kill(&placement.primary, &placement.backup);
            Some(placement.primary.as_str())
        }
        Incident::PrimaryRebooted => {
            cluster.kill_and_depart(&placement.primary);
            Some(placement.primary.as_str())
        }
        Incident::PrimaryKilledUnannounced => {
            cluster.kill_unannounced(&placement.primary);
            Some(placement.primary.as_str())
        }
        Incident::StalledKilled => {
            cluster.fh.stall(&placement.primary, &placement.backup);
            None
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
    strike
}

/// The strike of `cut`: inside the replication window, but for a kill of the
/// primary, announced or not, at an applied sub-point past which the primary wrote no later
/// turn — the backup then holds everything the dead primary did.
fn strike_of(cluster: &Cluster, held: &HeldTurn, cut: Cut) -> Strike {
    let applied_and_no_later_write = cut.at.point == TurnPoint::Applied && {
        let turns = cluster.fh.turn_points().turns(&held.call_ref);
        !turns.iter().skip(cut.at.turn).any(|t| t.wrote)
    };
    let in_window = !(cut.incident.kills_the_primary() && applied_and_no_later_write);
    Strike::Incident { incident: cut.incident, in_window }
}

/// The first outcome row `strike` opens at `level` that accepts the parties'
/// failed `verdict`.
fn outcome_row(
    strike: Strike,
    level: SyncLevel,
    facts: &CutFacts,
    verdict: &ScenarioVerdict,
    obs: &ObservedState,
) -> Option<&'static Row> {
    windows::opened_by(strike, level).find(|row| match row.kind {
        Kind::Outcome(accepts) => accepts(facts, verdict, obs),
        _ => false,
    })
}

/// The row `strike` opens at `level` that accepts `n` CDRs beyond the
/// incident's own count, or `None` when no row is needed; `Err` when no row
/// accepts them.
fn cdr_row(strike: Strike, level: SyncLevel, n: usize) -> Result<Option<&'static Row>, ()> {
    let own = strike.incident().map_or(1..=1, Incident::cdrs);
    if own.contains(&n) {
        return Ok(None);
    }
    let row = windows::opened_by(strike, level).find(|row| match row.kind {
        Kind::Cdr { min, max } => (min..=max).contains(&n),
        _ => false,
    });
    row.map(Some).ok_or(())
}

/// The row of a goal dropped on a clean end `strike` opens at `level`.
fn goal_drop_row(strike: Strike, level: SyncLevel) -> Option<&'static Row> {
    windows::opened_by(strike, level).find(|row| matches!(row.kind, Kind::GoalDrop))
}

/// Whether the evidence rule ended a call one of `cdrs` records, the cell's
/// or another its cluster held (ADR-0055 §4): it notes `evidence` as the
/// reason of the ending it began.
fn ended_on_evidence(cdrs: &[CdrRecord]) -> bool {
    cdrs.iter().flat_map(|c| &c.events).any(|e| e.reason.as_deref() == Some("evidence"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cdrs(incident: Incident, n: usize) -> Result<Option<&'static str>, ()> {
        cdr_row(Strike::from(incident), SyncLevel::Applied, n).map(|r| r.map(|r| r.id))
    }

    /// A split that heals leaves its call one CDR or two, never none; only a
    /// split that never heals may lose it.
    #[test]
    fn a_healed_split_leaves_one_cdr_or_two() {
        assert!(cdrs(Incident::SplitHealed, 0).is_err());
        assert_eq!(cdrs(Incident::SplitHealed, 1), Ok(None));
        assert_eq!(cdrs(Incident::SplitHealed, 2), Ok(Some("W-cdr-two")));
        assert_eq!(cdrs(Incident::Split, 0), Ok(Some("W-cdr-split")));
    }

    /// A killed primary may leave two CDRs without a row (ADR-0055 §5); a
    /// killed backup leaves one.
    #[test]
    fn a_killed_primary_may_leave_two_cdrs_of_its_own() {
        assert_eq!(cdrs(Incident::PrimaryKilled, 2), Ok(None));
        assert!(cdrs(Incident::PrimaryKilled, 0).is_err());
        assert!(cdrs(Incident::BackupKilled, 2).is_err());
    }
}
