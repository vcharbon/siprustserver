//! The rule chain's turn on one in-dialog event: the framework's ladders and
//! screens before the rules, the rules, and the passes after them, with the
//! message-cap defense wrapped around the whole. A turn reads the per-process
//! values it is handed ([`Framework`]) and the event as the shell resolved it
//! ([`TurnInput`]), and nothing else (ADR-0052 §2).

pub mod message_cap;
pub mod quiet_turn;
pub mod response_facts;
pub mod timer_screen;

use call::helpers::{Arrival, Resolved};
use call::{Call, CallModelState, Direction, Obligation, TimerType};
use sip_message::{Method, SipMessage};

use crate::answer_deadline::Screened;
use crate::config::B2buaConfig;
use crate::effects::{HandlerEffects, HandlerResult, QuietTurn};
use crate::obligations::ObligationSet;
use crate::rules::actions::{possibly_confirmed, Owed, Settle};
use crate::rules::lifecycle_delivery::{close_turn, TurnStart};
use crate::rules::{execute_rules, ActionExecutor, RuleCall, RuleContext, RuleDefinition};
use crate::turn_notes::TurnNotes;
use crate::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;

/// The per-process values every turn reads, borrowed from the shell.
#[derive(Clone, Copy)]
pub struct Framework<'a> {
    pub config: &'a B2buaConfig,
    /// The composed rule list (ADR-0016).
    pub rules: &'a [RuleDefinition],
    pub obligations: &'a ObligationSet,
}

/// The event a turn runs on, as the shell resolved it, with the turn's clock
/// reading.
pub struct TurnInput<'a> {
    pub event: &'a CallEvent,
    pub call_ref: &'a str,
    /// The leg the event arrived on, and its direction.
    pub source_leg_id: &'a str,
    pub direction: Direction,
    /// What the dialog gate resolved the event's message to.
    pub resolved: Resolved,
    /// A response: whether it answered a client transaction the transaction
    /// layer holds, which ACKs a non-2xx INVITE final hop-by-hop itself (RFC
    /// 3261 §17.1.1.3). `false` for a request.
    pub matched_client_txn: bool,
    pub now_ms: i64,
}

/// Run the synchronous rule chain for one in-dialog event, with the
/// MAX_MESSAGES_PER_CALL cap-defense wrapped around it.
///
/// The count opens at 1 on the initial INVITE (`initial_invite`) and every
/// in-dialog rule-chain event adds one, except a rung of this node's own
/// ladder: a rung repeats retained bytes on this node's clock and is no
/// message of the peer's, so a deaf peer under a lossy path is not a runaway
/// dialog. A repeated inbound 2xx is the peer's message and counts. If the
/// bump crosses `max_messages_per_call` and the handler did not itself
/// terminate the call, append a begin-termination so a runaway dialog
/// (re-INVITE/OPTIONS storm, glare loop, a peer that never stops) is torn down
/// instead of processing unbounded in-dialog events forever — each of which
/// allocates a txn (`set_txn`), a working `Call` clone, and a store body. The
/// async limiter-refresh does not count. The bump rides the turn's own write;
/// on a quiet turn (`QuietTurn`) it rides the next write. Order: bump +
/// capture `cap_exceeded` BEFORE the handler runs, terminate AFTER, so the
/// in-flight event (e.g. relaying this re-INVITE's response) is still serviced
/// before teardown; a turn that trips the cap is the teardown, never quiet.
pub fn rule_chain_turn(
    fw: &Framework<'_>,
    mut call: Call,
    input: TurnInput<'_>,
    turn: &mut TurnScope,
) -> HandlerResult {
    let TurnInput {
        event,
        call_ref,
        source_leg_id,
        direction,
        mut resolved,
        matched_client_txn,
        now_ms,
    } = input;
    // The cap budget is a PER-LIVENESS-INTERVAL rate, not a lifetime total: the
    // keepalive tick RESETS the counter, opening a fresh window. A runaway
    // dialog still lands >cap events inside one window and is torn down; a
    // healthy long call — whose per-interval traffic is its own self-paced
    // probes plus a bounded trickle — never consumes the defense meant for
    // runaway peers. A lifetime reading tore down every hour-long keepalive
    // call around message 200, BYEing a healthy dialog mid-call.
    let bumped = if fired(event, &TimerType::Keepalive) {
        0
    } else if fired(event, &TimerType::Rung)
        || fired(event, &TimerType::NotifyAnswerCap)
        || fired(event, &TimerType::HeldAnswer)
    {
        call.message_count.unwrap_or(0)
    } else {
        call.message_count.unwrap_or(0) + 1
    };
    call.message_count = Some(bumped);
    let cap_exceeded = i64::try_from(fw.config.max_messages_per_call).is_ok_and(|cap| bumped > cap)
        && !matches!(call.state, CallModelState::Terminating | CallModelState::Terminated);
    let mut exec = ActionExecutor { config: fw.config, turn, now_ms };
    // The dialog-level ladders are the framework's (ADR-0032 X4): a rung is
    // repeated here and never reaches a rule; the discharging ACK or PRACK
    // retires its ladder before the rules run and they read the fact; a
    // give-up's timers are scrubbed before the CORE give-up rule decides, and
    // its verdict is settled after (`settle_give_up`: an un-ACKed 2xx ends the
    // session whatever the rules made of it).
    let mut ladder_fx = HandlerEffects::new();
    // A service request's answer, or its deadline, is screened against the
    // call's pending deadlines before anything reads it (ADR-0039): an expired
    // deadline is read as the request's lost answer, and an answer or a
    // deadline no longer awaited reaches no rule.
    let lost_answer;
    let (event, rules_read) = match crate::answer_deadline::screen(&mut call, &mut ladder_fx, event)
    {
        Screened::Expired(lost) => {
            lost_answer = lost;
            (&lost_answer, true)
        }
        Screened::Unawaited => (event, false),
        Screened::Awaited | Screened::Other => (event, true),
    };
    if let Some(obligation) = fired_obligation(event, &TimerType::Rung) {
        let before = call.clone();
        exec.repeat(&mut call, &mut ladder_fx, obligation);
        ladder_fx.quiet = Some(QuietTurn::OwnRung);
        let repeated = HandlerResult { call, effects: ladder_fx };
        return enforced(fw, &before, repeated, now_ms, &mut exec.turn.notes);
    }
    // The end of the wait behind this stack's own NOTIFY is the framework's:
    // the requests held there leave, and no rule sees the fire.
    if fired(event, &TimerType::NotifyAnswerCap) {
        let before = call.clone();
        exec.release_at_cap(&mut call, &mut ladder_fx);
        let released = HandlerResult { call, effects: ladder_fx };
        return enforced(fw, &before, released, now_ms, &mut exec.turn.notes);
    }
    // A caller dialog held possibly confirmed is the framework's: its hold
    // ends with her ACK naming it or at its deadline, its BYE leaves, and no
    // rule sees either (ADR-0055 §4).
    if let Some(tag) = possibly_confirmed::released_by(&call, event, source_leg_id) {
        let before = call.clone();
        exec.release_possibly_confirmed(&mut call, &mut ladder_fx, &tag);
        let released = HandlerResult { call, effects: ladder_fx };
        return enforced(fw, &before, released, now_ms, &mut exec.turn.notes);
    }
    // The deadline of a request held for a service's final is the
    // framework's: the platform answers it, and no rule sees the fire.
    if fired(event, &TimerType::HeldAnswer) {
        let before = call.clone();
        exec.answer_overdue_held(&mut call, &mut ladder_fx);
        let answered = HandlerResult { call, effects: ladder_fx };
        return enforced(fw, &before, answered, now_ms, &mut exec.turn.notes);
    }
    let start = TurnStart::of(&call);
    // A reliable provisional is the engine's before it is any rule's (RFC
    // 3262): recorded with its early dialog and offer, or — a repeat, or out
    // of `RSeq` order — dropped unread, its turn a write of nothing else.
    let arrival = exec.take_reliable_provisional(&mut call, event, source_leg_id);
    let screened = matches!(arrival, Some(Arrival::Repeat | Arrival::OutOfOrder));
    // A fire of the setup's deadline once the setup is over reaches no rule:
    // its scope is read as the call stands at the fire.
    let rules_read = rules_read && !screened && !setup_spent(event, &call);
    let discharged = exec.discharge(&mut call, &mut ladder_fx, event, source_leg_id);
    if let CallEvent::Sip { message, .. } = event {
        if let SipMessage::Response(resp) = message.as_ref() {
            let leg = source_leg_id;
            let fx = &mut ladder_fx;
            response_facts::record(&mut exec, &mut call, fx, leg, resp, screened);
            // Only the final on this stack's NOTIFY's own transaction ends
            // the wait behind it and speaks for its subscription: a final to
            // another request reusing its number (a dead owner's) answers
            // nothing here.
            if let Some((holder, record)) = resolved.record.as_deref() {
                if resp.status() >= 200 {
                    let status = Some(resp.status());
                    exec.notify_settled(&mut call, fx, leg, holder, record, status);
                }
            }
        }
    }
    // The request record a final or a timeout settles is settled before the
    // rules read the event, which they read on `resolved` (ADR-0055 §2).
    exec.settle_before_rules(&mut call, event, source_leg_id, &mut resolved);
    // This stack's NOTIFY that timed out ends its subscription (RFC 6665
    // §4.2.2) and no longer holds the requests behind it.
    if let (CallEvent::Timeout { .. }, Some((holder, record))) = (event, resolved.record.as_deref())
    {
        exec.notify_settled(&mut call, &mut ladder_fx, source_leg_id, holder, record, None);
    }
    // The transaction layer answered an out-of-dialog CANCEL itself — 200 to
    // the CANCEL, 487 to the initial INVITE (RFC 3261 §9.2): the record
    // carries that transaction's one final before the rules read it, over a
    // 2xx this stack built too late for the completed transaction to send.
    if let CallEvent::Cancelled { in_dialog: false, .. } = event {
        call = call::helpers::record_cancelled_invite(call, source_leg_id);
    }
    if let Some(obligation) = fired_obligation(event, &TimerType::RepeatGiveUp) {
        exec.turn.notes.repeat_give_up(obligation.kind());
        exec.give_up(&mut call, &mut ladder_fx, obligation);
    }
    // The record names the message before the rules read it, so what the
    // turn sends follows what it received.
    if let Some(ring) = crate::message_ring::Ring::of(fw.config) {
        let dialog = resolved.dialog.as_ref();
        let repeat = arrival == Some(Arrival::Repeat);
        let received = crate::message_ring::Received {
            event,
            matched_client_txn,
            repeat,
            discharged: discharged.as_ref(),
            dialog,
        };
        call = ring.received(call, source_leg_id, received, now_ms);
    }
    // An INVITE (offerless included) or an UPDATE offer the leg's peer sends
    // opens an exchange on that leg, counted once as it is received, before
    // the rules read it.
    if let CallEvent::Sip { message, .. } = event {
        if let SipMessage::Request(req) = message.as_ref() {
            let source = resolved.dialog.as_ref();
            crate::rules::relay::note_request(&mut call, source_leg_id, req, source);
        }
    }
    // A decision folding back while its consult is awaited records the routes
    // admission refused before it, then its mark, whichever rule claims it.
    if rules_read {
        crate::attempts::record_refused_routes(&mut call, &mut ladder_fx, event);
        call = crate::decision_log::fold_decided(call, event, now_ms);
    }
    // An admit sent off the call's turn is applied before the rules read its
    // event (an unawaited fold's route is not the call's); one of another key
    // (an earlier call under this call_ref) releases that key when owed.
    let fx = &mut ladder_fx;
    if let Some(report) = crate::limiter::report::apply_to_call(&mut call, event, rules_read, fx) {
        let what = || format!("admit: {:?} -> {:?}", report.entries, report.outcome);
        exec.turn.notes.trace(&call, "limiter", now_ms, what);
    }
    let rule_ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref,
        event,
        source_leg_id,
        direction,
        now_ms,
        config: fw.config,
        discharged: discharged.as_ref(),
        resolved: &resolved,
    };
    let mut result = if rules_read {
        execute_rules(fw.rules, &call, &rule_ctx, &mut exec, fw.obligations)
    } else {
        HandlerResult { call: call.clone(), effects: HandlerEffects::new() }
    };
    // The final to the INVITE that dialled the leg is its outcome, recorded
    // after the rules matched the leg as the response found it: a rule that
    // acted on it already recorded it, the first final standing (§17.1.1.2).
    if let CallEvent::Sip { message, .. } = event {
        if let SipMessage::Response(resp) = message.as_ref() {
            if resp.status() >= 200 && resp.cseq().method() == Method::Invite {
                result.call = call::helpers::record_dialled_invite_final(
                    result.call,
                    source_leg_id,
                    i64::from(resp.cseq().seq()),
                    resp.status(),
                );
            }
        }
    }
    quiet_turn::name_candidate(&mut result, &call, event, discharged.as_ref());
    // The ladder's own effects (its cancels) precede the rule's.
    ladder_fx.extend(std::mem::take(&mut result.effects));
    result.effects = ladder_fx;
    if let Some(obligation) = fired_obligation(event, &TimerType::RepeatGiveUp) {
        let before = result.call.clone();
        let settled = exec.settle_give_up(result, obligation, &rule_ctx);
        result = enforced(fw, &before, settled, now_ms, &mut exec.turn.notes);
    }
    if cap_exceeded
        && !matches!(result.call.state, CallModelState::Terminating | CallModelState::Terminated)
    {
        result = message_cap::tear_down_runaway(fw, &mut exec, result, &rule_ctx);
    }
    // A reliable provisional shown to no one, or whose INVITE, leg or dialog
    // the turn ended, is PRACKed by the engine whatever the rules made of it
    // (RFC 3262 §4).
    exec.settle_owed_pracks(&mut result.call, &mut result.effects, Owed::Call, Settle::TurnEnd);
    // The turn's lifecycle closes over what every layer above made of it.
    close_turn(fw.rules, &start, result, &mut exec)
}

/// `result`, the turn's outcome on `before`, finalized and held to the
/// invariants the framework enforces, what it meets noted on `notes`.
pub fn enforced(
    fw: &Framework<'_>,
    before: &Call,
    result: HandlerResult,
    now_ms: i64,
    notes: &mut TurnNotes,
) -> HandlerResult {
    crate::rules::invariants::enforce(
        fw.obligations,
        before,
        crate::rules::invariants::finalize(result),
        now_ms,
        crate::rules::invariants::UnansweredCaller::Answer(&fw.config.minted_final_advertisement),
        notes,
    )
}

/// Whether `event` fires a timer guarding the call's setup on a call whose
/// setup is over ([`call::helpers::setup_over`]).
fn setup_spent(event: &CallEvent, call: &Call) -> bool {
    matches!(event, CallEvent::Timer { key, .. } if key.scope == call::TimerScope::Setup)
        && call::helpers::setup_over(call)
}

/// Whether `event` is a fire of a timer of type `timer_type`.
pub fn fired(event: &CallEvent, timer_type: &TimerType) -> bool {
    matches!(event, CallEvent::Timer { key, .. } if &key.timer_type == timer_type)
}

/// The obligation of a fired `timer_type` ladder timer.
fn fired_obligation<'a>(event: &'a CallEvent, timer_type: &TimerType) -> Option<&'a Obligation> {
    match event {
        CallEvent::Timer { key, .. } if &key.timer_type == timer_type => key.obligation_of(),
        _ => None,
    }
}
