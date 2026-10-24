//! The compared fields of a call record and the closed exempt list
//! (ADR-0055 §7). Every field of the record is compared except the exempt
//! ones, so a field added to the record is compared until it is named here:
//! the comparison below destructures every struct it walks, borrowing both
//! sides, and a new field does not build until it is placed.
//!
//! The exempt fields are what a write changes without changing the call:
//!
//! - the maximum of two copies stands: the version vector, each dialog's
//!   local and remote CSeq (RFC 3261 §12.2.1.1), the message and turn
//!   counters, the timer generation, the limiter's change counter (no change
//!   number either copy reserved is reused);
//! - the union of two copies stands: the CDR events, each leg's message ring;
//! - the container's value stands: the keepalive request records, the
//!   keepalive and limiter-refresh ledger entries, the ladder rungs a quiet
//!   turn moves (none changes a dialog), the trace fields.

use crate::model::{
    Call, CallTopology, Dialog, InviteExchange, Ladder, Leg, MessageRing, Provisional,
    RequestRecord, Shown, TimerEntry, TimerType, Unacked2xx,
};

use super::sequence::raise_sequences;

/// Whether `a` and `b` agree on every compared field.
pub fn same_state(a: &Call, b: &Call) -> bool {
    let Call {
        call_ref,
        a_leg,
        b_legs,
        attempts,
        active_peer,
        callback_context,
        billing_context,
        a_leg_invite,
        limiter,
        timers,
        timer_generation: _,
        cdr_events: _,
        state,
        created_at,
        trace_id: _,
        root_span_id: _,
        sampled: _,
        worker_index,
        topology,
        emergency,
        features,
        policy_update_headers,
        policy_update_body,
        active_rules,
        ext,
        message_count: _,
        terminating_refresh_legs,
        relay_first_18x,
        promote_pem,
        transfer,
        subscriptions,
        reroute,
        provisionals,
        message_seq: _,
        decision_log,
        decision_ordinal,
        termination,
        message_turn: _,
        sm_cursors,
        tag_seed,
    } = a;
    *call_ref == b.call_ref
        && same_leg(a_leg, &b.a_leg)
        && all_pairs(b_legs, &b.b_legs, same_leg)
        && *attempts == b.attempts
        && *active_peer == b.active_peer
        && *callback_context == b.callback_context
        && *billing_context == b.billing_context
        && *a_leg_invite == b.a_leg_invite
        && limiter.same_but_change(&b.limiter)
        && timers
            .iter()
            .filter(|t| !exempt_timer(t))
            .eq(b.timers.iter().filter(|t| !exempt_timer(t)))
        && *state == b.state
        && *created_at == b.created_at
        && *worker_index == b.worker_index
        && same_option(topology, &b.topology, same_topology)
        && *emergency == b.emergency
        && *features == b.features
        && *policy_update_headers == b.policy_update_headers
        && *policy_update_body == b.policy_update_body
        && *active_rules == b.active_rules
        && *ext == b.ext
        && *terminating_refresh_legs == b.terminating_refresh_legs
        && *relay_first_18x == b.relay_first_18x
        && *promote_pem == b.promote_pem
        && *transfer == b.transfer
        && *subscriptions == b.subscriptions
        && *reroute == b.reroute
        && all_pairs(provisionals, &b.provisionals, same_provisional)
        && *decision_log == b.decision_log
        && *decision_ordinal == b.decision_ordinal
        && *termination == b.termination
        && *sm_cursors == b.sm_cursors
        && *tag_seed == b.tag_seed
}

/// Whether `a` and `b` hold as many items, pairwise `same`.
fn all_pairs<T>(a: &[T], b: &[T], same: impl Fn(&T, &T) -> bool) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y))
}

/// Whether `a` and `b` are both absent, or both present and `same`.
fn same_option<T>(a: &Option<T>, b: &Option<T>, same: impl Fn(&T, &T) -> bool) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => same(x, y),
        _ => false,
    }
}

/// The worker pair; the vector and the stamp are exempt.
fn same_topology(a: &CallTopology, b: &CallTopology) -> bool {
    let CallTopology { pri, bak, gen: _, bak_gen: _, stamp_gen: _, stamp_bak_gen: _ } = a;
    *pri == b.pri && *bak == b.bak
}

fn same_leg(a: &Leg, b: &Leg) -> bool {
    let Leg {
        leg_id,
        call_id,
        source,
        dialogs,
        local_uri,
        remote_uri,
        invite_request_uri,
        invite,
        own_tag,
        ext,
        kind,
        attempt,
        ring,
        in_session_timer,
        adopted,
        messages: _,
        sdp_session,
    } = a;
    *leg_id == b.leg_id
        && *call_id == b.call_id
        && *source == b.source
        && dialogs.len() == b.dialogs.len()
        && dialogs.iter().zip(b.dialogs.iter()).all(|(x, y)| same_dialog(x, y))
        && *local_uri == b.local_uri
        && *remote_uri == b.remote_uri
        && *invite_request_uri == b.invite_request_uri
        && same_option(invite, &b.invite, same_record)
        && *own_tag == b.own_tag
        && *ext == b.ext
        && *kind == b.kind
        && *attempt == b.attempt
        && *ring == b.ring
        && *in_session_timer == b.in_session_timer
        && *adopted == b.adopted
        && *sdp_session == b.sdp_session
}

fn same_dialog(a: &Dialog, b: &Dialog) -> bool {
    let Dialog {
        our_tag,
        peer_tag,
        state,
        our_cseq: _,
        peer_cseq: _,
        remote_target,
        route_set,
        fronts,
        shown,
        shows_media,
        requests,
        sent_invite,
        answered_invite,
        cached_sdp,
        sdp_session,
        owed_answer,
        subscriptions,
        queued,
    } = a;
    let compared = |r: &&RequestRecord| !keepalive_record(r);
    *our_tag == b.our_tag
        && *peer_tag == b.peer_tag
        && *state == b.state
        && *remote_target == b.remote_target
        && *route_set == b.route_set
        && *fronts == b.fronts
        && *shown == b.shown
        && *shows_media == b.shows_media
        && requests.iter().filter(compared).count() == b.requests.iter().filter(compared).count()
        && requests
            .iter()
            .filter(compared)
            .zip(b.requests.iter().filter(compared))
            .all(|(x, y)| same_record(x, y))
        && same_option(sent_invite, &b.sent_invite, same_record)
        && same_option(answered_invite, &b.answered_invite, same_record)
        && *cached_sdp == b.cached_sdp
        && *sdp_session == b.sdp_session
        && *owed_answer == b.owed_answer
        && *subscriptions == b.subscriptions
        && *queued == b.queued
}

/// A request record, the rung of the 2xx ladder it holds left out.
fn same_record(a: &RequestRecord, b: &RequestRecord) -> bool {
    let RequestRecord {
        method,
        origin,
        usage,
        outbound_cseq,
        sent,
        deadline_ms,
        inbound_cseq,
        source_vias,
        source_call_id,
        source_from,
        source_to,
        cancelled,
        offered_100rel,
        offered,
        source_timestamp,
        relayed_by,
        author,
        invite,
    } = a;
    *method == b.method
        && *origin == b.origin
        && *usage == b.usage
        && *outbound_cseq == b.outbound_cseq
        && *sent == b.sent
        && *deadline_ms == b.deadline_ms
        && *inbound_cseq == b.inbound_cseq
        && *source_vias == b.source_vias
        && *source_call_id == b.source_call_id
        && *source_from == b.source_from
        && *source_to == b.source_to
        && *cancelled == b.cancelled
        && *offered_100rel == b.offered_100rel
        && *offered == b.offered
        && *source_timestamp == b.source_timestamp
        && *relayed_by == b.relayed_by
        && *author == b.author
        && same_option(invite, &b.invite, same_exchange)
}

fn same_exchange(a: &InviteExchange, b: &InviteExchange) -> bool {
    let InviteExchange {
        ack_branch,
        emitted_ack,
        awaited_ack_cseq,
        answered_2xx,
        bye_behind_ack,
        dialled,
        final_sent,
        answer_preempted,
    } = a;
    let same_2xx = |x: &Unacked2xx, y: &Unacked2xx| {
        let Unacked2xx { dialog_tag, cseq, emission } = x;
        *dialog_tag == y.dialog_tag && *cseq == y.cseq && emission.same_at_rest(&y.emission)
    };
    *ack_branch == b.ack_branch
        && *emitted_ack == b.emitted_ack
        && *awaited_ack_cseq == b.awaited_ack_cseq
        && same_option(answered_2xx, &b.answered_2xx, same_2xx)
        && *bye_behind_ack == b.bye_behind_ack
        && *dialled == b.dialled
        && *final_sent == b.final_sent
        && *answer_preempted == b.answer_preempted
}

/// A reliable provisional, the rung of its shown ladder left out.
fn same_provisional(a: &Provisional, b: &Provisional) -> bool {
    let Provisional { id, responder_sdp, responder, shown } = a;
    let same_shown = |x: &Shown, y: &Shown| {
        let Shown { tag, rseq, invite_cseq, carried_sdp, ladder } = x;
        let same_ladder = match (ladder, &y.ladder) {
            (Ladder::Repeating(e), Ladder::Repeating(f)) => e.same_at_rest(f),
            (l, m) => l == m,
        };
        *tag == y.tag
            && *rseq == y.rseq
            && *invite_cseq == y.invite_cseq
            && *carried_sdp == y.carried_sdp
            && same_ladder
    };
    *id == b.id
        && *responder_sdp == b.responder_sdp
        && *responder == b.responder
        && same_option(shown, &b.shown, same_shown)
}

/// `container` with the exempt fields of `other` merged in: the higher of
/// the two vectors and of each counter and dialog sequence, the union of the
/// CDR events and of each leg's message ring; a limiter release either copy
/// owes stays owed. Every other field, the stamp included, is the
/// container's.
pub fn merge_exempt(container: Call, other: &Call) -> Call {
    let mut merged = raise_sequences(container, other);
    if let (Some(m), Some(o)) = (merged.topology.as_mut(), other.topology.as_ref()) {
        m.gen = m.gen.max(o.gen);
        m.bak_gen = m.bak_gen.max(o.bak_gen);
    }
    merged.timer_generation = merged.timer_generation.max(other.timer_generation);
    merged.limiter.merge_change(&other.limiter);
    merged.message_seq = merged.message_seq.max(other.message_seq);
    merged.message_turn = merged.message_turn.max(other.message_turn);
    merged.message_count = match (merged.message_count, other.message_count) {
        (Some(m), Some(o)) => Some(m.max(o)),
        (m, o) => m.or(o),
    };
    for event in &other.cdr_events {
        if !merged.cdr_events.contains(event) {
            merged.cdr_events.push(event.clone());
        }
    }
    let theirs = |leg_id: &str| super::leg::find_leg(other, leg_id).map(|l| &l.messages);
    merged.a_leg.messages = union_ring(&merged.a_leg.messages, theirs(&merged.a_leg.leg_id));
    for leg in merged.b_legs.iter_mut() {
        leg.messages = union_ring(&leg.messages, theirs(&leg.leg_id));
    }
    merged
}

/// The entries of `ours` and those of `theirs` it lacks, by the call-wide
/// sequence; the higher eviction count.
fn union_ring(ours: &MessageRing, theirs: Option<&MessageRing>) -> MessageRing {
    let Some(theirs) = theirs else { return ours.clone() };
    let mut entries = ours.entries.clone();
    entries.extend(theirs.entries.iter().filter(|e| !ours.entries.contains(e)).cloned());
    entries.sort_by_key(|e| (e.seq, e.at_ms));
    MessageRing { entries, dropped: ours.dropped.max(theirs.dropped) }
}

/// Whether `timer` is an exempt ledger entry: the keepalive, its timeout,
/// the limiter refresh, a ladder rung.
fn exempt_timer(timer: &TimerEntry) -> bool {
    matches!(
        timer.timer_type(),
        TimerType::Keepalive
            | TimerType::KeepaliveTimeout
            | TimerType::LimiterRefresh
            | TimerType::Rung
    )
}

/// Whether `record` is a keepalive request record: the in-dialog OPTIONS this
/// stack sends on its own.
fn keepalive_record(record: &RequestRecord) -> bool {
    record.is_own() && record.method.eq_ignore_ascii_case("OPTIONS")
}
