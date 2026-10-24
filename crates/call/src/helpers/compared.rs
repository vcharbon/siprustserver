//! The compared fields of a call record and the closed exempt list
//! (ADR-0055 §7). Every field of the record is compared except the exempt
//! ones, so a field added to the record is compared until it is named here:
//! the projection below destructures every struct it walks, and a new field
//! does not build until it is placed.
//!
//! The exempt fields are what a write changes without changing the call:
//!
//! - the maximum of two copies stands: the version vector, each dialog's
//!   local and remote CSeq (RFC 3261 §12.2.1.1), the message and turn
//!   counters, the timer generation;
//! - the union of two copies stands: the CDR events, each leg's message ring;
//! - the container's value stands: the keepalive request records, the
//!   keepalive and limiter-refresh ledger entries, the ladder rungs a quiet
//!   turn moves (none changes a dialog), the limiter's change counter (its
//!   lease expires), the trace fields.

use crate::model::{
    Call, CallTopology, Dialog, InviteExchange, Leg, MessageRing, Provisional, RequestRecord,
    Shown, TimerEntry, TimerType, Unacked2xx,
};

use super::sequence::raise_sequences;

/// Whether `a` and `b` agree on every compared field.
pub fn same_state(a: &Call, b: &Call) -> bool {
    compared(a) == compared(b)
}

/// `container` with the exempt fields of `other` merged in: the higher of
/// the two vectors and of each counter and dialog sequence, the union of the
/// CDR events and of each leg's message ring. Every other field, the stamp
/// included, is the container's.
pub fn merge_exempt(container: Call, other: &Call) -> Call {
    let mut merged = raise_sequences(container, other);
    if let (Some(m), Some(o)) = (merged.topology.as_mut(), other.topology.as_ref()) {
        m.gen = m.gen.max(o.gen);
        m.bak_gen = m.bak_gen.max(o.bak_gen);
    }
    merged.timer_generation = merged.timer_generation.max(other.timer_generation);
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

/// The record with every exempt field at a fixed value.
fn compared(call: &Call) -> Call {
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
    } = call;
    Call {
        call_ref: call_ref.clone(),
        a_leg: compared_leg(a_leg),
        b_legs: b_legs.iter().map(compared_leg).collect(),
        attempts: attempts.clone(),
        active_peer: active_peer.clone(),
        callback_context: callback_context.clone(),
        billing_context: billing_context.clone(),
        a_leg_invite: a_leg_invite.clone(),
        limiter: limiter.without_change(),
        timers: timers.iter().filter(|t| !exempt_timer(t)).cloned().collect(),
        timer_generation: 0,
        cdr_events: Vec::new(),
        state: *state,
        created_at: *created_at,
        trace_id: None,
        root_span_id: None,
        sampled: None,
        worker_index: *worker_index,
        topology: topology.as_ref().map(compared_topology),
        emergency: *emergency,
        features: features.clone(),
        policy_update_headers: policy_update_headers.clone(),
        policy_update_body: policy_update_body.clone(),
        active_rules: active_rules.clone(),
        ext: ext.clone(),
        message_count: None,
        terminating_refresh_legs: terminating_refresh_legs.clone(),
        relay_first_18x: relay_first_18x.clone(),
        promote_pem: promote_pem.clone(),
        transfer: transfer.clone(),
        subscriptions: subscriptions.clone(),
        reroute: reroute.clone(),
        provisionals: provisionals.iter().map(compared_provisional).collect(),
        message_seq: 0,
        decision_log: decision_log.clone(),
        decision_ordinal: *decision_ordinal,
        termination: termination.clone(),
        message_turn: 0,
        sm_cursors: sm_cursors.clone(),
        tag_seed: *tag_seed,
    }
}

/// The worker pair; the vector and the stamp are exempt.
fn compared_topology(topology: &CallTopology) -> CallTopology {
    let CallTopology { pri, bak, gen: _, bak_gen: _, stamp_gen: _, stamp_bak_gen: _ } = topology;
    CallTopology::new(pri.clone(), bak.clone(), 0, 0)
}

fn compared_leg(leg: &Leg) -> Leg {
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
    } = leg;
    let mut compared_dialogs = crate::model::Dialogs::new();
    for d in dialogs.iter() {
        compared_dialogs.insert(compared_dialog(d));
    }
    Leg {
        leg_id: leg_id.clone(),
        call_id: call_id.clone(),
        source: source.clone(),
        dialogs: compared_dialogs,
        local_uri: local_uri.clone(),
        remote_uri: remote_uri.clone(),
        invite_request_uri: invite_request_uri.clone(),
        invite: invite.as_ref().map(compared_record),
        own_tag: own_tag.clone(),
        ext: ext.clone(),
        kind: *kind,
        attempt: *attempt,
        ring: *ring,
        in_session_timer: *in_session_timer,
        adopted: *adopted,
        messages: MessageRing::default(),
        sdp_session: sdp_session.clone(),
    }
}

fn compared_dialog(dialog: &Dialog) -> Dialog {
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
    } = dialog;
    Dialog {
        our_tag: our_tag.clone(),
        peer_tag: peer_tag.clone(),
        state: state.clone(),
        our_cseq: 0,
        peer_cseq: None,
        remote_target: remote_target.clone(),
        route_set: route_set.clone(),
        fronts: fronts.clone(),
        shown: *shown,
        shows_media: shows_media.clone(),
        requests: requests.iter().filter(|r| !keepalive_record(r)).map(compared_record).collect(),
        sent_invite: sent_invite.as_ref().map(compared_record),
        answered_invite: answered_invite.as_ref().map(compared_record),
        cached_sdp: cached_sdp.clone(),
        sdp_session: sdp_session.clone(),
        owed_answer: owed_answer.clone(),
        subscriptions: subscriptions.clone(),
        queued: queued.clone(),
    }
}

/// A request record with the rung of the 2xx ladder it holds left out.
fn compared_record(record: &RequestRecord) -> RequestRecord {
    let mut out = record.clone();
    if let Some(exchange) = out.invite.as_mut() {
        compare_exchange(exchange);
    }
    out
}

fn compare_exchange(exchange: &mut InviteExchange) {
    if let Some(Unacked2xx { emission, .. }) = exchange.answered_2xx.as_mut() {
        *emission = emission.at_rest();
    }
}

/// A reliable provisional with the rung of its shown ladder left out.
fn compared_provisional(provisional: &Provisional) -> Provisional {
    let mut out = provisional.clone();
    if let Some(Shown { ladder: crate::model::Ladder::Repeating(emission), .. }) =
        out.shown.as_mut()
    {
        *emission = emission.at_rest();
    }
    out
}
