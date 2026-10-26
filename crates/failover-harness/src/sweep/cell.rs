//! One cell of the sweep: the shape's call placed on [`PRIMARY`], cut at one
//! turn point by one incident, driven until the cluster holds nothing of it,
//! and judged. Every check panics: the RFC audit and the model invariants when
//! the harness drops; here, no node holding anything of the call, no limiter
//! set left, its CDR count — exactly one, up to two in a split cell, one or
//! two where a backed primary is killed, at most one where an unbacked one is
//! — and the shape's check of it.

use std::time::Duration;

use scenario_harness::actor::run_actor_scenario_observed;
use scenario_harness::realcall::CallCtx;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser, SipResponse};
use sip_net_testkit::RecordedSipEntry;

use super::parties::{Parties, ALICE};
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
    let run = run_actor_scenario_observed(&*scenario, &env, &ctx);
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
    let mut was_clear = false;
    loop {
        tokio::select! {
            biased;
            held = async { reached.as_mut().expect("guarded").await }, if reached.is_some() => {
                reached = None;
                let held = held.expect("the turn points keep the arm");
                let cut = cut.expect("armed");
                if cut.incident != Incident::Split {
                    ctx.accept_clean_ends();
                }
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
                // A cluster clear for a whole step: what a party sent as the
                // cluster forgot the call (a hop ACK) has landed.
                let quiet = clear && reached.is_none() && !pending;
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
    if !super::outcome::accepted(cut.map(|c| (c.incident, c.at.point)), &verdict, &obs) {
        panic!("[{name}] the parties failed: {verdict:?} {:?}", ctx.notes());
    }
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
/// deduplicated, its loss in a split is accepted), for a kill of a backed
/// primary one or two — the dead primary's and the survivor's — never none,
/// and for a call with no backup, lost with its primary as on a non-HA
/// B2BUA, one or none.
fn cdr_count_accepted(incident: Option<Incident>, n: usize) -> bool {
    match incident {
        Some(Incident::Split | Incident::SplitHealed) => n <= 2,
        Some(Incident::PrimaryKilled | Incident::StalledKilled) => (1..=2).contains(&n),
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
            accept_reused_cseq(cluster);
            accept_cancel_of_an_unseen_final(cluster);
            cluster.kill(&placement.primary, &placement.backup);
            Some(placement.primary.as_str())
        }
        Incident::StalledKilled => {
            accept_clean_end_into_ended_dialogs(cluster, names);
            super::contact_repeat::accept_contact_only_2xx_repeat(cluster);
            cluster.fh.stall(&placement.primary, &placement.backup);
            None
        }
        Incident::BackupKilled => {
            cluster.kill(&placement.backup, &placement.primary);
            Some(placement.backup.as_str())
        }
        Incident::Split => {
            cluster.split(placement);
            accept_two_copies_never_healed(cluster, names);
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
/// backup applied the write, the request drawing its peer's reject, may send
/// a reliable provisional whose RSeq does not continue the dead primary's, and
/// may send the caller a non-2xx after the 2xx the dead primary sent her, and
/// may CANCEL once an INVITE the dead primary saw finally answered
/// (ADR-0055 §11).
fn kill_inside_the_window(cluster: &mut Cluster, placement: &Placement) {
    accept_reused_cseq(cluster);
    accept_cancel_of_an_unseen_final(cluster);
    accept_non_2xx_after_the_dead_owners_2xx(cluster);
    accept_unseen_rseq(cluster);
    cluster.kill(&placement.primary, &placement.backup);
}

/// A node holding no record of what the dead primary sent — a copy whose
/// backup never applied the write, or the node answering a response no node
/// holds of a call that had no backup — re-uses, once per dialog, a CSeq the
/// dead primary sent, the request drawing its peer's reject (ADR-0055 §11).
fn accept_reused_cseq(cluster: &mut Cluster) {
    cluster.fh.accept_rfc_deviations_once_per_dialog(
        crate::RULE_CSEQ_IN_DIALOG_ORDER,
        "ADR-0055 §11: a node without the dead primary's record re-uses once per dialog \
         a CSeq the dead primary sent inside the replication window",
    );
}

/// A kill takes the record of a callee's final with it: the copy holds the
/// callee's INVITE unanswered and CANCELs it once; the callee answers the
/// CANCEL and keeps what its final left it — a dialog its own timers end, or
/// none (ADR-0055 §11).
fn accept_cancel_of_an_unseen_final(cluster: &mut Cluster) {
    cluster.fh.accept_rfc_deviations_once_per_dialog_where(
        crate::RULE_NO_CANCEL_AFTER_FINAL,
        cancels_an_answered_invite,
        "ADR-0055 §11: a kill: the copy's one CANCEL of an INVITE the dead primary saw finally \
         answered changes nothing",
    );
}

/// Whether `cancel` names an INVITE an entry of `earlier` captured at or
/// before `upto` — before the kill, for a window opened at it — answered with
/// a final.
fn cancels_an_answered_invite(
    cancel: &RecordedSipEntry,
    earlier: &[RecordedSipEntry],
    upto: u64,
) -> bool {
    let Ok(SipMessage::Request(cancel)) = CustomParser::new().parse(&cancel.raw) else {
        return false;
    };
    let key = (cancel.call_id().as_str(), cancel.cseq().seq());
    let ours =
        |resp: &SipResponse| resp.status() >= 200 && resp.from().tag() == cancel.from().tag();
    answered_before(earlier, upto, key, None, ours)
}

/// A split that never heals leaves two copies acting, each in a degraded mode
/// only a leak check judges (ADR-0055 §11): the stale copy's requests into a
/// dialog the other copy ended, closed by the 481 they draw; one copy's CANCEL
/// of an INVITE the other copy saw answered; one copy's non-2xx after the 2xx
/// the other copy sent the caller, which she discards; and one copy's BYE of a
/// caller dialog it holds early, which the other copy answered.
fn accept_two_copies_never_healed(cluster: &mut Cluster, names: &[String]) {
    cluster.fh.accept_dialog_verdicts(
        scenario_harness::dialogs::DIALOG_ENDED,
        names,
        "ADR-0055 §11: a split that never heals sends the stale copy's requests \
         into a dialog the other copy ended, closed by the 481 they draw",
    );
    cluster.fh.accept_rfc_deviations_once_per_dialog_where(
        crate::RULE_NO_CANCEL_AFTER_FINAL,
        |cancel, earlier, _| cancels_an_answered_invite(cancel, earlier, u64::MAX),
        "ADR-0055 §11: a split that never heals: one copy CANCELs an INVITE the other copy \
         saw answered",
    );
    cluster.fh.accept_rfc_deviations_from_now_where(
        RULE_UNACKED_NON_2XX,
        |final_, earlier, _| non_2xx_after_a_2xx(final_, earlier, None, u64::MAX),
        "ADR-0055 §11: a split that never heals: one copy's non-2xx after the 2xx the other \
         copy sent, discarded",
    );
    cluster.fh.accept_rfc_deviations_once_per_dialog(
        "no-bye-outside-or-early-dialog",
        "ADR-0055 §11: a split that never heals: one copy BYEs a caller dialog it holds early, \
         which the other copy answered",
    );
}

/// The non-2xx a node without the dead primary's record sends the caller
/// after the 2xx the dead primary sent her: the caller discards it (RFC 6026
/// §8.4) and owes it no ACK (ADR-0055 §11).
fn accept_non_2xx_after_the_dead_owners_2xx(cluster: &mut Cluster) {
    cluster.fh.accept_rfc_deviations_from_now_where(
        RULE_UNACKED_NON_2XX,
        non_2xx_after_the_dead_owners_2xx,
        "ADR-0055 §11: a kill inside the replication window: the caller discards the copy's \
         non-2xx after the 2xx the dead primary sent her",
    );
}

/// The audit rule a caller's unACKed non-2xx INVITE final meets.
const RULE_UNACKED_NON_2XX: &str = "unacked-invite-non-2xx-final";

/// Whether `final_` is a non-2xx to the caller's INVITE after a 2xx to that
/// INVITE sent to her before the kill (`opened_at`).
fn non_2xx_after_the_dead_owners_2xx(
    final_: &RecordedSipEntry,
    earlier: &[RecordedSipEntry],
    opened_at: u64,
) -> bool {
    non_2xx_after_a_2xx(final_, earlier, ALICE.parse().ok(), opened_at)
}

/// Whether `final_` is a non-2xx to an INVITE, sent to `to` when it is
/// stated, after a 2xx to that INVITE sent there and captured at or before
/// `upto`.
fn non_2xx_after_a_2xx(
    final_: &RecordedSipEntry,
    earlier: &[RecordedSipEntry],
    to: Option<std::net::SocketAddr>,
    upto: u64,
) -> bool {
    let Ok(SipMessage::Response(resp)) = CustomParser::new().parse(&final_.raw) else {
        return false;
    };
    if to.is_some_and(|to| final_.to != to) {
        return false;
    }
    let invite = (resp.call_id().as_str(), resp.cseq().seq());
    let answer = |r: &SipResponse| (200..300).contains(&r.status());
    resp.status() >= 300
        && resp.cseq().method() == Method::Invite
        && answered_before(earlier, upto, invite, to, answer)
}

/// Whether an entry of `earlier` captured at or before `upto` is a response
/// to the INVITE `invite` names (its Call-ID and CSeq number), sent to `to`
/// when it is stated, of which `also` holds.
fn answered_before(
    earlier: &[RecordedSipEntry],
    upto: u64,
    invite: (&str, u32),
    to: Option<std::net::SocketAddr>,
    also: impl Fn(&SipResponse) -> bool,
) -> bool {
    let parser = CustomParser::new();
    earlier.iter().filter(|e| e.seq <= upto && to.is_none_or(|to| e.to == to)).any(|entry| {
        let Ok(SipMessage::Response(resp)) = parser.parse(&entry.raw) else { return false };
        resp.cseq().method() == Method::Invite
            && resp.cseq().seq() == invite.1
            && resp.call_id().as_str() == invite.0
            && also(&resp)
    })
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
