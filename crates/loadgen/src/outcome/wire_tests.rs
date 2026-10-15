use super::*;

/// One message: `first` line, then the headers every message here carries.
pub(super) fn msg(first: &str, cid: &str, cseq: &str, to_tag: &str, extra: &str) -> Vec<u8> {
    let to = if to_tag.is_empty() { String::new() } else { format!(";tag={to_tag}") };
    format!(
        "{first}\r\nVia: SIP/2.0/UDP 127.0.0.1:6000;branch=z9hG4bK-1\r\nCall-ID: {cid}\r\n\
         From: <sip:a@h>;tag=f1\r\nTo: <sip:b@h>{to}\r\nCSeq: {cseq}\r\n{extra}\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// An 18x carrying a session description.
pub(super) fn early_media(cid: &str) -> Vec<u8> {
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";
    format!(
        "SIP/2.0 183 Session Progress\r\nVia: SIP/2.0/UDP 127.0.0.1:6000;branch=z9hG4bK-1\r\n\
         Call-ID: {cid}\r\nFrom: <sip:a@h>;tag=f1\r\nTo: <sip:b@h>;tag=t1\r\nCSeq: 1 INVITE\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    )
    .into_bytes()
}

fn feed(wire: &CallWire, events: &[(Side<'_>, Dir, Vec<u8>)]) -> WireFacts {
    let tap = wire.tap();
    for (side, dir, raw) in events {
        tap(*side, *dir, raw);
    }
    wire.facts()
}

/// The caller's setup facts: its first INVITE, the first provisional, early
/// media, the final with the cause key of its Reason; a retransmitted
/// INVITE and a later provisional change none of them.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn the_setup_facts_are_read_from_the_caller_side() {
    let clock = Clock::test_at(1_000);
    let wire = CallWire::new(clock.clone());
    let tap = wire.tap();
    tap(Side::Caller, Dir::Out, &msg("INVITE sip:b@h SIP/2.0", "c1", "1 INVITE", "", ""));
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    tap(Side::Caller, Dir::Out, &msg("INVITE sip:b@h SIP/2.0", "c1", "1 INVITE", "", ""));
    tap(Side::Caller, Dir::In, &msg("SIP/2.0 100 Trying", "c1", "1 INVITE", "", ""));
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    tap(Side::Caller, Dir::In, &early_media("c1"));
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    let busy = msg(
        "SIP/2.0 486 Busy Here",
        "c1",
        "1 INVITE",
        "t1",
        "Reason: Q.850;cause=017;text=\"busy\"\r\n",
    );
    tap(Side::Caller, Dir::In, &busy);
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    tap(Side::Caller, Dir::In, &busy);
    let f = wire.facts();
    assert_eq!(f.call_id.as_deref(), Some("c1"));
    assert_eq!(f.started_at_ms, Some(1_000));
    assert_eq!(f.first_provisional_at_ms, Some(1_010));
    assert!(f.early_media);
    assert_eq!((f.setup_ended_at_ms, f.final_code), (Some(1_030), Some(486)));
    assert_eq!(f.final_cause.as_deref(), Some("q.850;cause=17"));
    assert_eq!(f.answered_at_ms, None);
}

/// An in-dialog request answered non-2xx is a failure with its method and
/// code; a 481 marks the dialog gone; the BYE's final is kept.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn in_dialog_finals_are_failures_and_the_bye_is_kept() {
    let wire = CallWire::new(Clock::test_at(0));
    let f = feed(
        &wire,
        &[
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c2", "1 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c2", "1 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, msg("ACK sip:b@h SIP/2.0", "c2", "1 ACK", "t1", "")),
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c2", "2 INVITE", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 481 No Dialog", "c2", "2 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, msg("BYE sip:b@h SIP/2.0", "c2", "3 BYE", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c2", "3 BYE", "t1", "")),
        ],
    );
    assert_eq!(f.answered_at_ms, Some(0));
    assert_eq!(
        f.failures,
        vec![WireFailure { at_ms: 0, method: "INVITE".into(), code: Some(481) }]
    );
    assert!(f.saw_481);
    assert_eq!(f.bye, Some(Some(200)));
}

/// A callee leg opens on its initial INVITE and keeps its first final; its
/// early media and a CANCEL it received are kept.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn each_leg_keeps_its_first_final() {
    let wire = CallWire::new(Clock::test_at(0));
    let f = feed(
        &wire,
        &[
            (
                Side::Leg("primary"),
                Dir::In,
                msg("INVITE sip:p@h SIP/2.0", "b1", "1 INVITE", "", ""),
            ),
            (
                Side::Leg("primary"),
                Dir::Out,
                msg("SIP/2.0 486 Busy Here", "b1", "1 INVITE", "p1", ""),
            ),
            (Side::Leg("primary"), Dir::Out, msg("SIP/2.0 500 Late", "b1", "1 INVITE", "p1", "")),
            (Side::Leg("alt"), Dir::In, msg("INVITE sip:q@h SIP/2.0", "b2", "1 INVITE", "", "")),
            (Side::Leg("alt"), Dir::In, msg("CANCEL sip:q@h SIP/2.0", "b2", "1 CANCEL", "", "")),
            (Side::Leg("alt"), Dir::Out, msg("SIP/2.0 487 Terminated", "b2", "1 INVITE", "q1", "")),
        ],
    );
    assert_eq!(f.legs.len(), 2);
    assert_eq!((f.legs[0].role.as_str(), f.legs[0].final_code), ("primary", Some(486)));
    assert_eq!((f.legs[1].role.as_str(), f.legs[1].final_code), ("alt", Some(487)));
    assert!(f.legs[1].cancelled);
}

/// Once the verdict resolved, the teardown's CANCEL, the 487 it draws and a
/// leg's late final change no setup fact; the BYE is still followed.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn sealed_setup_facts_ignore_the_teardown() {
    let wire = CallWire::new(Clock::test_at(0));
    let tap = wire.tap();
    tap(Side::Caller, Dir::Out, &msg("INVITE sip:b@h SIP/2.0", "c3", "1 INVITE", "", ""));
    tap(Side::Caller, Dir::In, &msg("SIP/2.0 100 Trying", "c3", "1 INVITE", "", ""));
    tap(Side::Leg("callee"), Dir::In, &msg("INVITE sip:q@h SIP/2.0", "b3", "1 INVITE", "", ""));
    wire.seal();
    tap(Side::Caller, Dir::Out, &msg("CANCEL sip:b@h SIP/2.0", "c3", "1 CANCEL", "", ""));
    tap(Side::Caller, Dir::In, &msg("SIP/2.0 487 Terminated", "c3", "1 INVITE", "t1", ""));
    tap(Side::Leg("callee"), Dir::Out, &msg("SIP/2.0 487 Terminated", "b3", "1 INVITE", "q1", ""));
    let f = wire.facts();
    assert!(!f.cancelled, "the teardown's CANCEL is not the caller's");
    assert_eq!((f.final_code, f.setup_ended_at_ms), (None, None));
    assert_eq!(f.legs[0].final_code, None);
}

/// A new initial INVITE on the call's Call-ID (a 407 retry) is the setup
/// again: the retried call answered and torn down reads `answered`.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_challenged_invite_retried_restarts_the_setup() {
    let wire = CallWire::new(Clock::test_at(0));
    let f = feed(
        &wire,
        &[
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c4", "1 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 407 Auth", "c4", "1 INVITE", "p1", "")),
            (Side::Caller, Dir::Out, msg("ACK sip:b@h SIP/2.0", "c4", "1 ACK", "p1", "")),
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c4", "2 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c4", "2 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, msg("ACK sip:b@h SIP/2.0", "c4", "2 ACK", "t1", "")),
            (Side::Caller, Dir::Out, msg("BYE sip:b@h SIP/2.0", "c4", "3 BYE", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c4", "3 BYE", "t1", "")),
        ],
    );
    let meta = crate::outcome::RecordMeta { run: "r", source: "s", stream: "m", shape: "x" };
    let ok = crate::CallOutcome::Ok { branch: "answered" };
    let r = crate::outcome::build(&meta, &f, &ok, 1).unwrap();
    assert_eq!((r["ending"].as_str(), r["final_code"].as_u64()), (Some("answered"), Some(200)));
    assert_eq!(r["failures"], serde_json::json!([]));
}
