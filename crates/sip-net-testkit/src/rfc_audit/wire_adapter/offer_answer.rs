//! The live surfacing of the offer/answer family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::emitter(RuleId::AckBodyAfterCompleteOfferAnswer, ack_body_after_complete_offer_answer)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::Final2xxAnswersTheOffer, final_2xx_answers_the_offer)
        .skip(Skip::RelayLanes),
    Live::taker_else_emitter(RuleId::DelayedOfferAnsweredInAck, delayed_offer_answered_in_ack)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::SecondAnswerRepeatsTheFirst, second_answer_repeats_the_first)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::AnswerStreamMatchesOffer, answer_stream_matches_offer)
        .skip(Skip::RelayLanes),
    // Origin continuity is per originator: a proxy's stream interleaves both agents' origins.
    Live::emitter(RuleId::SdpOriginContinuity, sdp_origin_continuity)
        .roles(&[UaRole::Uac, UaRole::Uas])
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::NoNewOfferWhileOfferPending, no_new_offer_while_offer_pending)
        .advisory()
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::AnswerMLineCountMatchesOffer, answer_m_line_count_matches_offer)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::AnswerTLineEqualsOffer, answer_t_line_equals_offer)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::AnswerMediaTypeMatchesOffer, answer_media_type_matches_offer)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::DirectionPairValid, direction_pair_valid)
        .advisory()
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::RejectedStreamMinimalAnswer, rejected_stream_minimal_answer)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::ReOfferMLineCountMonotonic, re_offer_m_line_count_monotonic)
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::ZeroPortPropagation, zero_port_propagation)
        .advisory()
        .skip(Skip::RelayLanes),
    Live::either_end(RuleId::PayloadTypeMappingStable, payload_type_mapping_stable)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::SdpBodyParseable, sdp_body_parseable),
    Live::emitter(RuleId::C0PortNonZero, c0_port_non_zero).roles(&[UaRole::Uac]),
];

fn ack_body_after_complete_offer_answer(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AckBodyOnClosedRound { streams, .. }) = &f.decision
    else {
        return String::new();
    };
    let table = if streams.is_empty() { "no m= line".to_string() } else { streams.join(", ") };
    format!(
        "Sent an ACK (callId {cid}, CSeq {cseq}) carrying a session description \
         ({table}) although the offer/answer exchange it acknowledges was already \
         complete — expected no body on this ACK (Content-Length: 0), RFC 3261 \
         §13.2.2.4 / §13.2.1",
        cseq = f.cseq,
    )
}

fn final_2xx_answers_the_offer(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::OfferLeftUnanswered {
        status,
        offered_streams,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let table = if offered_streams.is_empty() {
        "no m= line".to_string()
    } else {
        offered_streams.join(", ")
    };
    format!(
        "Sent a {status} (callId {cid}, CSeq {cseq}) carrying no session \
         description although the request it answers offered one ({table}) and no \
         reliable message had answered it — expected the answer in this final, RFC \
         3261 §13.2.1 / RFC 3264 §6",
        cseq = f.cseq,
    )
}

fn delayed_offer_answered_in_ack(f: &rfc_rules::Finding, cid: &str) -> String {
    if !f.violated() {
        return String::new();
    }
    format!(
        "Sent an ACK (callId {cid}, CSeq {cseq}) carrying no session description \
         although the 2xx it acknowledges carried the offer (the INVITE had none) — \
         the ACK MUST carry the answer, RFC 3261 §13.2.2.4 / §13.2.1",
        cseq = f.cseq,
    )
}

fn second_answer_repeats_the_first(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::SecondAnswerDiverged {
        first_plan,
        second_plan,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent a second session description on one dialog (callId {cid}, CSeq \
         {cseq}) stating a transport plan its first answer did not — first \
         [{first}], second [{second}]; the peer takes the first answer and ignores \
         every later description on that dialog (RFC 3261 §13.2.1)",
        cseq = f.cseq,
        first = first_plan.join(", "),
        second = second_plan.join(", "),
    )
}

fn answer_stream_matches_offer(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AnswerStreamRetyped {
        stream_indexes,
        offered,
        answered,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    // Every re-typed stream is named once, on this one finding.
    let rows: Vec<String> = stream_indexes
        .iter()
        .zip(offered.iter().zip(answered.iter()))
        .map(|(i, (o, a))| format!("m=[{i}] offered \"{o}\", answered \"{a}\""))
        .collect();
    format!(
        "Answered the offer on CSeq {cseq} re-typing {n} stream(s) — {rows} (callId \
         {cid}); an answer keeps each stream's media type and proto (port 0 to reject \
         it) — RFC 3264 §6",
        cseq = f.cseq,
        n = stream_indexes.len(),
        rows = rows.join("; "),
    )
}

fn sdp_origin_continuity(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::SdpOriginDiverged {
        origin_line,
        prior_origin_line,
        same_session,
        body_changed,
        session_version,
        prior_session_version,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let offence = if !same_session {
        "it describes a different session — a later description keeps the o= identity \
         (username, sess-id, nettype, addrtype, address) and moves only sess-version"
            .to_string()
    } else if *body_changed {
        format!(
            "the description changed but sess-version went {prior_session_version} → \
             {session_version} (expected exactly {expected})",
            expected = prior_session_version.saturating_add(1),
        )
    } else {
        format!(
            "the description is byte-identical but sess-version went \
             {prior_session_version} → {session_version} (expected it unchanged or +1)"
        )
    };
    format!(
        "Sent \"{origin_line}\" after \"{prior_origin_line}\" on the same dialog (callId \
         {cid}, CSeq {cseq}) — {offence} — RFC 4566 §5.2 / RFC 3264 §8",
        cseq = f.cseq,
    )
}

/// The `m=[i] …` rows a collapsed per-stream finding names, one per offending
/// stream position, rendered by `row(i, a, b)` over three parallel vectors.
fn stream_rows(
    indexes: &[usize],
    left: &[String],
    right: &[String],
    row: impl Fn(usize, &str, &str) -> String,
) -> String {
    indexes
        .iter()
        .zip(left.iter().zip(right.iter()))
        .map(|(i, (l, r))| row(*i, l, r))
        .collect::<Vec<_>>()
        .join("; ")
}

fn no_new_offer_while_offer_pending(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::OfferWhilePending { pending_offer_cseq, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent an SDP offer (callId {cid}, CSeq {cseq}) while its own offer on CSeq \
         {pending_offer_cseq} was still unanswered — an offer is answered before the \
         next one goes out (RFC 3264 §5 / RFC3264-MUST-002)",
        cseq = f.cseq,
    )
}

fn answer_m_line_count_matches_offer(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AnswerMLineCountDiffers {
        offer_m_lines,
        answer_m_lines,
        offered_streams,
        answered_streams,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Answer SDP m= count ({answer_m_lines}) differs from offer m= count \
         ({offer_m_lines}) (callId {cid}, CSeq {cseq}) — offered [{offered}], answered \
         [{answered}]; a rejected stream keeps its slot at port 0 (RFC 3264 §6 / \
         RFC3264-MUST-018)",
        cseq = f.cseq,
        offered = offered_streams.join(", "),
        answered = answered_streams.join(", "),
    )
}

fn answer_t_line_equals_offer(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AnswerTLineDiffers {
        offer_t_line,
        answer_t_line,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Answer SDP t= line ('{answer_t_line}') differs from offer t= line \
         ('{offer_t_line}') (callId {cid}, CSeq {cseq}) — RFC 3264 §6 / \
         RFC3264-MUST-019",
        cseq = f.cseq,
    )
}

fn answer_media_type_matches_offer(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AnswerMediaTypeMismatched {
        stream_indexes,
        offered_types,
        answered_types,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    // Every mis-typed position is named once, on this one finding.
    let rows = stream_rows(stream_indexes, offered_types, answered_types, |i, o, a| {
        format!("m=[{i}] offered '{o}', answered '{a}'")
    });
    format!(
        "Answer media type does not match the offer's at {n} stream position(s) — \
         {rows} (callId {cid}, CSeq {cseq}) — RFC 3264 §6.1 / RFC3264-MUST-022",
        n = stream_indexes.len(),
        cseq = f.cseq,
    )
}

fn direction_pair_valid(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::DirectionPairInvalid {
        stream_indexes,
        offered_directions,
        answered_directions,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let rows = stream_rows(stream_indexes, offered_directions, answered_directions, |i, o, a| {
        format!("m=[{i}] answer '{a}' invalid for offer '{o}'")
    });
    format!(
        "Answer direction invalid for the offer's at {n} stream position(s) — {rows} \
         (callId {cid}, CSeq {cseq}) — RFC 3264 §6.1 / RFC3264-MUST-023",
        n = stream_indexes.len(),
        cseq = f.cseq,
    )
}

fn rejected_stream_minimal_answer(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RejectedStreamWithoutFormat {
        stream_indexes,
        rejected_rows,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let rows = stream_indexes
        .iter()
        .zip(rejected_rows.iter())
        .map(|(i, r)| format!("m=[{i}] \"{r}\""))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Rejected stream(s) (port=0) carry no media format token — {rows} (callId \
         {cid}, CSeq {cseq}) — RFC 3264 §6 / RFC3264-MUST-021",
        cseq = f.cseq,
    )
}

fn re_offer_m_line_count_monotonic(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ReOfferStreamsDropped {
        m_lines,
        prior_m_lines,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Re-offer m= count {m_lines} decreased from prior offer {prior_m_lines} — \
             streams must keep their slot (port=0) (callId {cid}, CSeq {cseq}) — RFC 3264 \
             §8 / RFC3264-MUST-042",
        cseq = f.cseq,
    )
}

fn zero_port_propagation(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ZeroPortResurrected {
        stream_indexes,
        answered_ports,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let rows = stream_indexes
        .iter()
        .zip(answered_ports.iter())
        .map(|(i, p)| format!("m=[{i}] answered port={p}"))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Offer disabled {n} stream(s) with port=0 that the answer re-enabled — {rows} \
         (callId {cid}, CSeq {cseq}) — RFC 3264 §8 / RFC3264-MUST-044",
        n = stream_indexes.len(),
        cseq = f.cseq,
    )
}

fn payload_type_mapping_stable(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::PayloadTypeRemapped {
        payload_types,
        prior_encodings,
        encodings,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let rows = payload_types
        .iter()
        .zip(prior_encodings.iter().zip(encodings.iter()))
        .map(|(pt, (prev, now))| format!("payload-type {pt} was '{prev}' now '{now}'"))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Remapped {n} payload-type binding(s) it had already stated on this call — \
         {rows} (callId {cid}, CSeq {cseq}); the peer decodes by the cached mapping — \
         RFC 3264 §8.3.2 / RFC3264-MUST-047",
        n = payload_types.len(),
        cseq = f.cseq,
    )
}

fn sdp_body_parseable(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::SdpBodyRejected { sdp_failure, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent an SDP body that fails the RFC 3264 / RFC 4566 grammar check: \
         {sdp_failure} (callId {cid})",
    )
}

fn c0_port_non_zero(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::HeldAndRejectedStreams {
        held_stream_indexes,
        held_streams,
        held_c_lines,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "SDP body holds and rejects the same stream(s) — {rows} (callId {cid}) — RFC 3264 \
         §6 / §8.4: an unspecified c= says \"hold\", port 0 says \"rejected\"",
        rows = stream_rows(held_stream_indexes, held_streams, held_c_lines, |i, m, c| {
            format!("m=[{i}] {m} under c={c}")
        }),
    )
}
