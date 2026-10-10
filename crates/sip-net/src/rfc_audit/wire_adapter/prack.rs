//! The live surfacing of the RFC 3262 family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::emitter(RuleId::UnackedReliableProvisional, unacked_reliable_provisional),
    Live::emitter(RuleId::RackWithoutKnownInvite, rack_without_known_invite),
    Live::emitter(RuleId::NoOverlappingReliableProvisionals, no_overlapping_reliable_provisionals),
    Live::emitter(RuleId::NonContiguousRseq, non_contiguous_rseq),
    Live::emitter(RuleId::NoPrackOfOutOfOrderRseq, no_prack_of_out_of_order_rseq),
    Live::emitter(RuleId::RequireReliable1xxOnRequire, require_reliable_1xx_on_require)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::ReliableNeedsClientOptIn, reliable_needs_client_opt_in)
        .roles(&[UaRole::Uas])
        .advisory()
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::NoReliable1xxOnInDialog, no_reliable_1xx_on_in_dialog)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::UnmatchedPrackProxied, unmatched_prack_proxied)
        .roles(&[UaRole::Proxy])
        .advisory()
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::Prack2xxOr481, prack_2xx_or_481).skip(Skip::RelayLanes),
    Live::emitter(
        RuleId::Delay2xxOnUnackedReliable1xxWithSdp,
        delay_2xx_on_unacked_reliable_1xx_with_sdp,
    )
    .skip(Skip::RelayLanes),
    Live::emitter(RuleId::NoNewReliable1xxAfterFinal, no_new_reliable_1xx_after_final)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::NoPrackOf100Trying, no_prack_of_100_trying).skip(Skip::RelayLanes),
    // Gates: each offer is read within its own call and early dialog, so a B2BUA's two legs never
    // meet in it.
    Live::either_end(RuleId::PrackAnswers1xxOffer, prack_answers_1xx_offer)
        .roles(&[UaRole::Uac, UaRole::Uas])
        .skip(Skip::RelayLanes),
];

fn unacked_reliable_provisional(f: &rfc_rules::Finding, cid: &str) -> String {
    let (rseq, status) = match &f.decision {
        Decision::Violated(rfc_rules::Evidence::Unacked { rseq, status, .. }) => (*rseq, *status),
        _ => (0, 0),
    };
    format!(
        "Received reliable 1xx (status {status}, RSeq={rseq}, callId {cid}, CSeq \
         {cseq}) — UAC did not send a matching PRACK (RFC 3262 §4 / RFC3262-MUST-021)",
        cseq = f.cseq,
    )
}

fn rack_without_known_invite(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::UnknownRack {
        rack_rseq,
        rack_cseq,
        known_cseqs,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    // One INVITE opened ⇒ the correct header is unambiguous: name it,
    // so the reader repairs the sender instead of re-deriving the rule.
    let expected = match known_cseqs.as_slice() {
        [only] => format!(", expected \"RAck: {rack_rseq} {only} INVITE\""),
        _ => String::new(),
    };
    let seen = known_cseqs.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
    format!(
        "Sent \"RAck: {rack_rseq} {rack_cseq} INVITE\" (callId {cid}) — its CSeq-num \
         field ({rack_cseq}) names no INVITE this bind opened (opened INVITE CSeq: \
         {seen}){expected} — RFC 3262 §7.2 (the RAck CSeq-num+method are copied from \
         the acknowledged 1xx's CSeq, NOT from the PRACK's own CSeq; this PRACK \
         matches no reliable provisional and settles nothing, so the 1xx keeps \
         retransmitting)"
    )
}

fn no_overlapping_reliable_provisionals(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::Overlapping { rseq, status, unacked_rseq, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent second reliable 1xx (status {status}, RSeq={rseq}, callId {cid}) before \
         prior RSeq {unacked_rseq} PRACKed — RFC 3262 §3 / RFC3262-MUST-012"
    )
}

fn non_contiguous_rseq(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RseqGap { rseq, prior_rseq, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent reliable 1xx RSeq={rseq} not contiguous with prior RSeq={prior_rseq} \
         (callId {cid}) — RFC 3262 §3 / RFC3262-MUST-013"
    )
}

fn no_prack_of_out_of_order_rseq(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::OutOfOrderRack {
        rack_rseq, expected_rseq, ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent PRACK for out-of-order RSeq={rack_rseq} (expected {expected_rseq}, callId \
         {cid}) — UAC must PRACK in order (RFC 3262 §4 / RFC3262-MUST-024)"
    )
}

fn require_reliable_1xx_on_require(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::Unreliable1xx { status, branch, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "INVITE required 100rel (callId {cid}, branch {branch}) but sent 1xx response \
         {status} lacks Require:100rel/RSeq and no 420 Unsupported:100rel was sent — RFC \
         3262 §3 / RFC3262-MUST-001/-002"
    )
}

fn reliable_needs_client_opt_in(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::UnsolicitedReliable1xx { status, branch, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent reliable 1xx (status {status}, callId {cid}, branch {branch}) — matching \
         INVITE neither Supported:100rel nor Require:100rel (RFC 3262 §3 / \
         RFC3262-MUST-004)"
    )
}

fn no_reliable_1xx_on_in_dialog(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::InDialogReliable1xx {
        status, method, branch, ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent reliable 1xx on a non-INVITE request (status {status}, callId {cid}, \
         branch {branch}, method {method}) — forbidden per RFC 3262 §3 / \
         RFC3262-MUST-005"
    )
}

fn unmatched_prack_proxied(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::PrackAbsorbed {
        rack_rseq,
        rack_cseq,
        rack_method,
        known_rseqs,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let known = match known_rseqs.as_slice() {
        [] => "none".to_string(),
        seqs => seqs.iter().map(u64::to_string).collect::<Vec<_>>().join(", "),
    };
    format!(
        "Received PRACK with RAck={rack_rseq} {rack_cseq} {rack_method} on proxy bind \
         (callId {cid}) — no matching reliable 1xx taken on this bind (RSeq taken: \
         {known}) AND no outgoing PRACK observed (proxy must forward unmatched PRACKs, \
         not absorb) — RFC 3262 §3 / RFC3262-MUST-006"
    )
}

fn prack_2xx_or_481(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::PrackAnsweredWrongly {
        status,
        rack_rseq,
        rack_matched,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    match rack_matched {
        true => format!(
            "Received PRACK with RAck.response-num {rack_rseq} names a reliable 1xx this \
             agent sent (RSeq, INVITE CSeq and method all match), but agent responded \
             {status} instead of 2xx (callId {cid}) — RFC 3262 §3 / RFC3262-MUST-009"
        ),
        false => format!(
            "Received PRACK with RAck.response-num {rack_rseq} names NO reliable 1xx this \
             agent sent (RSeq, INVITE CSeq or method differ), but agent responded \
             {status} instead of 481 (callId {cid}) — RFC 3262 §3 / RFC3262-MUST-010"
        ),
    }
}

fn delay_2xx_on_unacked_reliable_1xx_with_sdp(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AnsweredOverUnackedOffer {
        unacked_rseqs,
        branch,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let rseqs = unacked_rseqs.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
    format!(
        "Sent 2xx INVITE response while reliable 1xx (RSeq={rseqs}) with SDP still \
             unacked (callId {cid}, branch {branch}) — RFC 3262 §3 / RFC3262-MUST-014"
    )
}

fn no_new_reliable_1xx_after_final(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::Reliable1xxAfterFinal { rseq, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent new reliable 1xx (RSeq={rseq}, callId {cid}) after final INVITE response — \
         forbidden per RFC 3262 §3 / RFC3262-MUST-016"
    )
}

fn no_prack_of_100_trying(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::PrackedTrying { rseq, .. }) = &f.decision else {
        return String::new();
    };
    format!(
        "Sent PRACK references RSeq {rseq} from a received 100 Trying carrying \
         Require:100rel (callId {cid}) — UAC MUST ignore 100rel on 100 Trying (RFC 3262 \
         §4 / RFC3262-MUST-019)"
    )
}

fn prack_answers_1xx_offer(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::PrackWithoutAnswer { rack_rseq, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "PRACK for reliable-1xx-with-offer (RSeq={rack_rseq}, callId {cid}) carries no \
         body — RFC 3262 §5 / RFC3262-MUST-025"
    )
}
