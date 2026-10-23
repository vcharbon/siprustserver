//! One cell of the sweep: the shape's call placed on [`PRIMARY`], cut at one
//! turn point by one incident, driven until the cluster holds nothing of it,
//! and judged. Every check panics: the RFC audit and the model invariants when
//! the harness drops; here, no node holding anything of the call, no limiter
//! set left, its CDR count — exactly one, up to two in a split cell, at most
//! one where the incident may lose the call — and the shape's check of it.

use std::time::Duration;

use scenario_harness::actor::run_actor_scenario;
use scenario_harness::realcall::CallCtx;

use super::parties::Parties;
use super::{Incident, SweepShape};
use crate::cluster::Cluster;
use crate::roles::Placement;
use crate::turn_points::{HeldTurn, Turn, TurnAt, TurnPoint};

/// The worker every cell places its call on; the other is its backup.
pub const PRIMARY: &str = "b1";

/// How long a split lasts before it heals ([`Incident::SplitHealed`]).
const HEAL_AFTER: Duration = Duration::from_secs(1);
/// How long the replication stalls before the primary dies
/// ([`Incident::StalledKilled`]): past the hold's cap, and the §17.2.1 bound.
const STALL_FOR: Duration = Duration::from_millis(300);
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
    let mut recipe = shape.recipe();
    if cut.is_some_and(|c| c.incident == Incident::UnbackedKilled) {
        // Every backup refuses every replica: each call has no backup.
        recipe.replica.backup.calls = Some(0);
    }
    let mut cluster = Cluster::start(name, &recipe).await;
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
    let kill = tokio::time::sleep(Duration::MAX);
    tokio::pin!(kill);
    let mut kill_pending = false;

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
                strike(&mut cluster, &placement, cut, held, &parties.names());
                if cut.incident == Incident::SplitHealed {
                    heal.as_mut().reset(tokio::time::Instant::now() + HEAL_AFTER);
                    heal_pending = true;
                }
                if cut.incident == Incident::StalledKilled {
                    kill.as_mut().reset(tokio::time::Instant::now() + STALL_FOR);
                    kill_pending = true;
                }
            }
            () = &mut kill, if kill_pending => {
                kill_pending = false;
                kill_inside_the_window(&mut cluster, &placement);
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
                let pending = heal_pending || kill_pending;
                if (clear && reached.is_none() && !pending) || settled >= SETTLE_BUDGET {
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
    if !cdr_count_accepted(cut.map(|c| c.incident), cdrs.len()) {
        panic!("[{name}] the call left {} CDRs", cdrs.len());
    }
    if let Some(cdr) = cdrs.first() {
        if let Err(why) = shape.check_cdr(cdr) {
            panic!("[{name}] the CDR is wrong: {why}");
        }
    }
    turn_points.turns(&call_ref)
}

/// Whether `n` CDRs of the call are what `incident` may leave (ADR-0055 §5):
/// one, or for a split call's two copies each one or neither (no CDR is
/// deduplicated, its loss in a split is accepted), for a stalled primary its
/// own and the departure sweep's, and for an unbacked kill one or none.
fn cdr_count_accepted(incident: Option<Incident>, n: usize) -> bool {
    match incident {
        Some(Incident::Split | Incident::SplitHealed) => n <= 2,
        Some(Incident::StalledKilled) => (1..=2).contains(&n),
        Some(Incident::UnbackedKilled) => n <= 1,
        _ => n == 1,
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

/// Befall the call's owners with `cut`'s incident while `held` stands at its turn
/// point. A node the incident kills stays frozen there; any other goes on.
/// `names` are the parties' UAs, whose dialog verdicts a split that never
/// heals declares.
fn strike(
    cluster: &mut Cluster,
    placement: &Placement,
    cut: Cut,
    held: HeldTurn,
    names: &[String],
) {
    let victim = match cut.incident {
        // At an applied sub-point the backup holds the turn's write: the kill is
        // inside a replication window only when the primary already wrote a
        // later turn of the call, and declares nothing otherwise.
        Incident::PrimaryKilled
            if cut.at.point == TurnPoint::Applied && !wrote_past(cluster, &held, cut) =>
        {
            cluster.kill(&placement.primary, &placement.backup);
            Some(placement.primary.as_str())
        }
        Incident::PrimaryKilled => {
            accept_clean_end_into_ended_dialogs(cluster, names);
            kill_inside_the_window(cluster, placement);
            Some(placement.primary.as_str())
        }
        Incident::UnbackedKilled => {
            accept_clean_end_into_ended_dialogs(cluster, names);
            cluster.kill(&placement.primary, &placement.backup);
            Some(placement.primary.as_str())
        }
        Incident::StalledKilled => {
            accept_clean_end_into_ended_dialogs(cluster, names);
            cluster.fh.stall(&placement.primary, &placement.backup);
            None
        }
        Incident::BackupKilled => {
            cluster.kill(&placement.backup, &placement.primary);
            Some(placement.backup.as_str())
        }
        Incident::Split => {
            cluster.split(placement);
            cluster.fh.accept_dialog_verdicts(
                scenario_harness::dialogs::DIALOG_ENDED,
                names,
                "ADR-0055 §11: a split that never heals sends the stale copy's requests \
                 into a dialog the other copy ended, closed by the 481 they draw",
            );
            None
        }
        Incident::SplitHealed => {
            accept_clean_end_into_ended_dialogs(cluster, names);
            accept_unseen_rseq(cluster);
            cluster.split(placement);
            None
        }
    };
    if victim != Some(held.node.as_str()) {
        held.release();
    }
}

/// Whether the primary wrote a turn of `held`'s call after the cut's turn.
fn wrote_past(cluster: &Cluster, held: &HeldTurn, cut: Cut) -> bool {
    let turns = cluster.fh.turn_points().turns(&held.call_ref);
    turns.iter().skip(cut.at.turn).any(|t| t.wrote)
}

/// Kill the primary of `placement` inside the replication window: a copy may
/// re-use, once per dialog, an in-dialog CSeq the dead primary sent before its
/// backup applied the write, the request drawing its peer's reject, and may
/// send a reliable provisional whose RSeq does not continue the dead
/// primary's (ADR-0055 §11).
fn kill_inside_the_window(cluster: &mut Cluster, placement: &Placement) {
    cluster.fh.accept_rfc_deviations_once_per_dialog(
        crate::RULE_CSEQ_IN_DIALOG_ORDER,
        "ADR-0055 §11: a copy re-uses once per dialog a CSeq the dead primary sent \
         inside the replication window",
    );
    accept_unseen_rseq(cluster);
    cluster.kill(&placement.primary, &placement.backup);
}

/// The §4 clean end BYEs every dialog the record or the evidence names; one
/// the other owner already ended draws its 481 at the party (ADR-0055 §11).
fn accept_clean_end_into_ended_dialogs(cluster: &Cluster, names: &[String]) {
    cluster.fh.accept_dialog_verdicts(
        scenario_harness::dialogs::DIALOG_ENDED,
        names,
        "ADR-0055 §11: the §4 clean end's BYE into a dialog the other owner ended, \
         closed by the 481 it draws",
    );
}

/// A copy's reliable provisional, under the dialog's derived tag, whose RSeq
/// cannot continue the sequence the dead primary sent (RFC 3262 §4); the
/// call still ends cleanly (ADR-0055 §11).
fn accept_unseen_rseq(cluster: &mut Cluster) {
    for rule in ["non-contiguous-rseq", "no-prack-of-out-of-order-rseq"] {
        cluster.fh.accept_rfc_deviations_from_now(
            rule,
            "ADR-0055 §11: a copy's reliable provisional cannot continue the RSeq the \
             dead primary sent",
        );
    }
}
