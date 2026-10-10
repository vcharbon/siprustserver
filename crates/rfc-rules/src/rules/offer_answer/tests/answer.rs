//! The answer against its offer.

use super::*;

// ---- answer-stream-matches-offer -----------------------------------

#[test]
fn answer_keeping_media_type_and_proto_is_clean() {
    // Judged from the ANSWERER: it takes the offer and sends the answer.
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(T38_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(T38_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let out = run(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
    assert_eq!(out[0].emitter, BOB, "the answerer is charged");
}

#[test]
fn answer_re_typing_the_stream_is_flagged() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(T38_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::AnswerStreamRetyped {
        stream_indexes, offered, answered, ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(stream_indexes, &vec![0]);
    assert_eq!(offered, &vec!["image 27500 udptl".to_string()]);
    assert_eq!(answered, &vec!["audio 60788 RTP/AVP".to_string()]);
}

#[test]
fn answer_re_transporting_the_stream_is_flagged() {
    const SAVP_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 60788 RTP/SAVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(SAVP_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::AnswerStreamRetyped { offered, .. }) = &f[0].decision else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(offered, &vec!["audio 27500 RTP/AVP".to_string()]);
}

#[test]
fn rejected_stream_keeps_type_and_proto_at_port_zero() {
    const REJECTED: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(REJECTED)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    assert!(violations(&AnswerStreamMatchesOffer, &msgs).is_empty());
}

#[test]
fn every_re_typed_stream_of_one_answer_rides_one_finding() {
    const TWO_STREAM_OFFER: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n\
m=video 27502 RTP/AVP 96\r\n";
    const TWO_STREAM_ANSWER: &str = "v=0\r\n\
o=bob 2 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=image 60788 udptl t38\r\n\
m=audio 60790 RTP/SAVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(TWO_STREAM_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(TWO_STREAM_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(f.len(), 1, "one answer is one occasion: {f:?}");
    let Decision::Violated(Evidence::AnswerStreamRetyped { stream_indexes, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(stream_indexes, &vec![0, 1], "both streams named on the one finding");
}

#[test]
fn a_second_round_is_judged_like_the_first() {
    const RETYPED_T38_ANSWER: &str = "v=0\r\n\
o=bob 117241 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(T38_OFFER)),
        resp(5, BOB, ALICE, 200, 3, "INVITE", Some(RETYPED_T38_ANSWER)),
        req(6, ALICE, BOB, "ACK", 3, Some("bt"), None),
    ];
    let f = violations(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].cseq, 3, "the second round's offer transaction");
}

// ---- answer-m-line-count-matches-offer ------------------------------

#[test]
fn an_answer_with_the_offers_stream_count_is_clean() {
    let out = run(&AnswerMLineCountMatchesOffer, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].emitter, BOB, "the answerer is charged");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn an_answer_adding_a_stream_is_flagged() {
    let f = violations(&AnswerMLineCountMatchesOffer, &one_round(OFFER_1AUDIO, ANSWER_2MEDIA));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::AnswerMLineCountDiffers {
        offer_m_lines,
        answer_m_lines,
        answered_streams,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!((*offer_m_lines, *answer_m_lines), (1, 2));
    assert_eq!(answered_streams, &vec!["audio/RTP/AVP".to_string(), "video/RTP/AVP".to_string()]);
}

/// Two forks are two answers to ONE offer (the To tag partitions the
/// round's closures), so each fork's answer is judged on its own dialog and
/// a bad one behind a clean fork is not hidden by it.
#[test]
fn each_fork_answers_the_one_offer_on_its_own_dialog() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(OFFER_1AUDIO)),
        resp_fork(2, 180, 1, "b1", None),
        resp_fork(3, 200, 1, "b2", Some(ANSWER_2MEDIA)),
        resp_fork(4, 200, 1, "b1", Some(ANSWER_1AUDIO)),
    ];
    let out = run(&AnswerMLineCountMatchesOffer, &msgs);
    assert_eq!(out.len(), 2, "one occasion per fork's answer: {out:?}");
    let f: Vec<&Finding> = out.iter().filter(|f| f.violated()).collect();
    assert_eq!(f.len(), 1, "only the fork that answered wrongly: {f:?}");
    assert_eq!(f[0].anchor, 2, "the two-stream answer on fork b2");
}

// ---- answer-t-line-equals-offer -------------------------------------

#[test]
fn an_answer_repeating_the_t_line_is_clean() {
    let out = run(&AnswerTLineEqualsOffer, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn an_answer_rewriting_the_t_line_is_flagged() {
    let answer = ANSWER_1AUDIO.replace("t=0 0", "t=100 200");
    let f = violations(&AnswerTLineEqualsOffer, &one_round(OFFER_1AUDIO, &answer));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::AnswerTLineDiffers { offer_t_line, answer_t_line, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!((offer_t_line.as_str(), answer_t_line.as_str()), ("0 0", "100 200"));
}

// ---- answer-media-type-matches-offer --------------------------------

#[test]
fn an_answer_keeping_the_media_type_at_each_index_is_clean() {
    let out = run(&AnswerMediaTypeMatchesOffer, &one_round(OFFER_2MEDIA, ANSWER_2MEDIA));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn every_index_one_answer_swapped_rides_one_finding() {
    let swapped = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 50000 RTP/AVP 96\r\n\
a=sendrecv\r\n\
m=audio 50002 RTP/AVP 0\r\n\
a=sendrecv\r\n";
    let f = violations(&AnswerMediaTypeMatchesOffer, &one_round(OFFER_2MEDIA, swapped));
    assert_eq!(f.len(), 1, "one answer is one occasion: {f:?}");
    let Decision::Violated(Evidence::AnswerMediaTypeMismatched {
        stream_indexes,
        offered_types,
        answered_types,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(stream_indexes, &vec![0, 1], "both positions named on the one finding");
    assert_eq!(offered_types, &vec!["audio".to_string(), "video".to_string()]);
    assert_eq!(answered_types, &vec!["video".to_string(), "audio".to_string()]);
}

// ---- direction-pair-valid -------------------------------------------

#[test]
fn a_sendrecv_pair_is_clean() {
    let out = run(&DirectionPairValid, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn an_answer_of_sendonly_to_sendonly_is_flagged() {
    let offer = OFFER_1AUDIO.replace("a=sendrecv", "a=sendonly");
    let answer = ANSWER_1AUDIO.replace("a=sendrecv", "a=sendonly");
    let f = violations(&DirectionPairValid, &one_round(&offer, &answer));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::DirectionPairInvalid {
        offered_directions,
        answered_directions,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(offered_directions, &vec!["sendonly".to_string()]);
    assert_eq!(answered_directions, &vec!["sendonly".to_string()]);
}

/// An `m=` block with no direction attribute states `sendrecv` (§6.1), so
/// an offer of `inactive` answered by a bare block is a real mis-pairing.
#[test]
fn a_bare_answer_block_reads_as_sendrecv() {
    let offer = OFFER_1AUDIO.replace("a=sendrecv", "a=inactive");
    let answer = ANSWER_1AUDIO.replace("a=sendrecv\r\n", "");
    let f = violations(&DirectionPairValid, &one_round(&offer, &answer));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::DirectionPairInvalid { answered_directions, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(answered_directions, &vec!["sendrecv".to_string()]);
}

// ---- rejected-stream-minimal-answer ---------------------------------

#[test]
fn a_rejection_keeping_a_format_token_is_clean() {
    let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0");
    let out = run(&RejectedStreamMinimalAnswer, &one_round(OFFER_1AUDIO, &answer));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn a_bare_rejection_is_flagged() {
    let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP");
    let f = violations(&RejectedStreamMinimalAnswer, &one_round(OFFER_1AUDIO, &answer));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::RejectedStreamWithoutFormat {
        stream_indexes,
        rejected_rows,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(stream_indexes, &vec![0]);
    assert_eq!(rejected_rows, &vec!["audio 0 RTP/AVP".to_string()]);
}

// ---- zero-port-propagation ------------------------------------------

#[test]
fn an_answer_keeping_a_disabled_stream_disabled_is_clean() {
    let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    let out = run(&ZeroPortPropagation, &one_round(&offer, &answer));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn an_answer_reviving_a_disabled_stream_is_flagged() {
    let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    let f = violations(&ZeroPortPropagation, &one_round(&offer, ANSWER_1AUDIO));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::ZeroPortResurrected {
        stream_indexes, answered_ports, ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(
        (stream_indexes.as_slice(), answered_ports.as_slice()),
        ([0].as_slice(), [50000].as_slice())
    );
}

#[test]
fn an_answer_stating_no_readable_port_settles_nothing() {
    let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio - RTP/AVP 0 96");
    let out = run(&ZeroPortPropagation, &one_round(&offer, &answer));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(!out[0].decided(), "{out:?}");
}
