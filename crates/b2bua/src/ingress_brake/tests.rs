use super::*;
use sip_txn::IdGen;

/// Every refusal the brake answered: first refusals and copies.
fn answered(counters: &IngressBrakeCounters) -> u64 {
    Class::ALL.iter().map(|&c| counters.refused(c)).sum::<u64>() + counters.refused_copies()
}

// ---- threshold arithmetic (floor(queue_max * pct / 100)) ----

#[test]
fn threshold_is_floor_of_queue_max_times_pct() {
    // The brake test's config: queue_max=5, pct=40 → floor(200/100)=2.
    let cfg = IngressBrakeConfig { queue_max: 5, threshold_pct: 40 };
    assert_eq!(cfg.threshold(), 2);
    // Production-ish: queue_max=8192, pct=70 → floor(573440/100)=5734.
    let prod = IngressBrakeConfig { queue_max: 8192, threshold_pct: 70 };
    assert_eq!(prod.threshold(), 5734);
    // pct=0 would brake from the first INVITE — only ever set deliberately;
    // the floor is exact.
    assert_eq!(IngressBrakeConfig { threshold_pct: 0, ..cfg }.threshold(), 0);
    assert_eq!(IngressBrakeConfig { threshold_pct: 100, ..cfg }.threshold(), 5);
}

// ---- classification fixtures ----

const B2BUA_IP: &str = "127.0.0.1";
const B2BUA_PORT: u16 = 5060;
const FLOODER_IP: &str = "10.0.0.1";
const FLOODER_PORT: u16 = 5555;

/// An INVITE. `to_tag` makes it in-dialog (a re-INVITE); `emergency` adds
/// the canonical `Resource-Priority` the brake bypasses on.
fn invite_buf(i: u32, emergency: bool, to_tag: Option<&str>) -> Vec<u8> {
    let to_param = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    let mut s = format!(
        "INVITE sip:bob@{B2BUA_IP}:{B2BUA_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER_IP}:{FLOODER_PORT};branch=z9hG4bK-brake-{i}\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-{i}\r\n\
To: <sip:bob@b2bua.test>{to_param}\r\n\
Call-ID: brake-test-{i}@{FLOODER_IP}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{FLOODER_IP}:{FLOODER_PORT}>\r\n\
Max-Forwards: 70\r\n"
    );
    if emergency {
        s.push_str("Resource-Priority: esnet.0\r\n");
    }
    s.push_str("Content-Length: 0\r\n\r\n");
    s.into_bytes()
}

/// A new, non-emergency INVITE — the only class the brake sheds.
fn new_invite(i: u32) -> Vec<u8> {
    invite_buf(i, false, None)
}

fn options_buf(i: u32) -> Vec<u8> {
    format!(
        "OPTIONS sip:bob@{B2BUA_IP}:{B2BUA_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER_IP}:{FLOODER_PORT};branch=z9hG4bK-opts-{i}\r\n\
From: <sip:alice@flooder.test>;tag=opt-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: opts-{i}@{FLOODER_IP}\r\n\
CSeq: 1 OPTIONS\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn response_buf() -> Vec<u8> {
    format!(
        "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP {FLOODER_IP}:{FLOODER_PORT};branch=z9hG4bK-resp\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-r\r\n\
To: <sip:bob@b2bua.test>;tag=bob-tag-r\r\n\
Call-ID: resp@{FLOODER_IP}\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// Brake hook under test: threshold 2, refusing with `refusals`.
fn brake_over(refusals: Refusals) -> (PreIngressHook, IngressBrakeCounters) {
    let counters = IngressBrakeCounters::new();
    let cfg = IngressBrakeConfig { queue_max: 5, threshold_pct: 40 };
    (build_ingress_brake_hook(cfg, counters.clone(), refusals), counters)
}

/// The worker's refusals: `Retry-After` base 5, jitter `jitter_sec`.
fn refusals_with_jitter(jitter_sec: u32) -> Refusals {
    Refusals::new(5, jitter_sec, 64, &IdGen::seeded(1))
}

/// Brake hook under test: threshold 2, jitter `jitter_sec`.
fn brake_with_jitter(jitter_sec: u32) -> (PreIngressHook, IngressBrakeCounters) {
    brake_over(refusals_with_jitter(jitter_sec))
}

/// Brake hook under test: threshold 2, jitter 0 (so `Retry-After` is
/// exactly the base).
fn brake() -> (PreIngressHook, IngressBrakeCounters) {
    brake_with_jitter(0)
}

fn parsed(raw: &[u8]) -> sip_message::SipRequest {
    match CustomParser::new().parse(raw).expect("fixture parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

fn src() -> std::net::SocketAddr {
    format!("{FLOODER_IP}:{FLOODER_PORT}").parse().unwrap()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn status_line(buf: &[u8]) -> &[u8] {
    match buf.windows(2).position(|w| w == b"\r\n") {
        Some(end) => &buf[..end],
        None => buf,
    }
}

#[test]
fn new_non_emergency_invites_past_the_threshold_are_rejected() {
    let (hook, counters) = brake();
    let flood = 10usize;
    let mut rejects = 0usize;
    for i in 0..flood {
        // Undrained queue: depth equals the count already accepted (0,1,2,2…).
        let depth = i.min(2);
        match hook(&new_invite(i as u32), src(), depth) {
            PreIngressAction::Reply(resp) => {
                rejects += 1;
                assert_eq!(status_line(&resp), b"SIP/2.0 503 Service Unavailable");
                // jitter==0 → Retry-After is exactly the base (5).
                assert!(
                    find(&resp, b"Retry-After: 5\r\n").is_some(),
                    "reject must carry the base Retry-After; got {:?}",
                    String::from_utf8_lossy(&resp)
                );
                assert!(
                    find(&resp, b"Reason:").is_none(),
                    "a new-call reject carries no Reason; got {:?}",
                    String::from_utf8_lossy(&resp)
                );
            }
            PreIngressAction::Accept => {
                assert!(i < 2, "INVITE {i} below threshold must be accepted");
            }
            PreIngressAction::Drop => panic!("brake never silently drops"),
        }
    }
    assert_eq!(rejects, flood - 2);
    assert_eq!(counters.refused(Class::Normal), (flood - 2) as u64);
    assert_eq!(counters.refused_copies(), 0);
}

/// Every copy of a refused INVITE is answered, the first counted as a new
/// call refused, each later one as a copy.
#[test]
fn a_retransmitted_invite_is_one_new_call_shed() {
    let (hook, counters) = brake();
    for _ in 0..3 {
        assert!(matches!(hook(&new_invite(7), src(), 2), PreIngressAction::Reply(_)));
    }
    assert!(matches!(hook(&new_invite(8), src(), 2), PreIngressAction::Reply(_)));
    assert_eq!(answered(&counters), 4);
    assert_eq!(counters.refused(Class::Normal), 2);
    assert_eq!(counters.refused_copies(), 2, "each retransmission is a copy");
}

/// A copy of a shed INVITE that arrives below the threshold draws the same
/// 503 and is not a new call; any other INVITE below it passes.
#[test]
fn a_shed_invite_copy_below_the_threshold_is_shed_again() {
    let (hook, counters) = brake();
    let PreIngressAction::Reply(first) = hook(&new_invite(7), src(), 2) else {
        panic!("above the threshold a new INVITE is shed");
    };
    let PreIngressAction::Reply(again) = hook(&new_invite(7), src(), 0) else {
        panic!("a copy of a shed INVITE is shed below the threshold too");
    };
    assert_eq!(first, again, "the same request draws the same 503 (RFC 3261 §8.2.7)");
    assert!(matches!(hook(&new_invite(8), src(), 0), PreIngressAction::Accept));
    assert!(matches!(hook(&options_buf(8), src(), 0), PreIngressAction::Accept));
    assert_eq!(answered(&counters), 2);
    assert_eq!(counters.refused(Class::Normal), 1);
}

/// The shared reject-new-call primitive tags To (RFC 3261 §8.2.6.2), so the
/// brake's 503 does too.
#[test]
fn the_reject_carries_a_to_tag() {
    let (hook, _counters) = brake();
    let PreIngressAction::Reply(resp) = hook(&new_invite(0), src(), 2) else {
        panic!("a new non-emergency INVITE above threshold must be rejected");
    };
    let text = String::from_utf8(resp).expect("utf-8 reject");
    let to_line = text.lines().find(|l| l.starts_with("To:")).expect("a To line");
    assert!(to_line.contains(";tag="), "the brake's reject must tag To: {to_line}");
}

/// The brake answers without transaction state, so RFC 3261 §8.2.7 binds
/// it: a retransmitted INVITE draws the identical 503 — same To-tag, same
/// jittered `Retry-After` — never a freshly rolled one.
#[test]
fn a_retransmitted_invite_draws_the_identical_reject() {
    let (hook, counters) = brake_with_jitter(30);
    let invite = new_invite(4);
    let PreIngressAction::Reply(first) = hook(&invite, src(), 2) else {
        panic!("a new non-emergency INVITE above threshold must be rejected");
    };
    let PreIngressAction::Reply(again) = hook(&invite, src(), 2) else {
        panic!("the retransmission must be rejected too");
    };
    assert_eq!(first, again, "a retransmission must draw a byte-identical 503");
    assert_eq!(answered(&counters), 2);
    // Jitter is applied, and stays inside [base, base + jitter].
    let text = String::from_utf8(first).expect("utf-8 reject");
    let value: u32 = text
        .lines()
        .find_map(|l| l.strip_prefix("Retry-After: "))
        .expect("a Retry-After line")
        .parse()
        .expect("numeric Retry-After");
    assert!((5..=35).contains(&value), "Retry-After {value} outside [5, 35]");
}

#[test]
fn emergency_invites_bypass_the_brake_even_above_the_threshold() {
    let (hook, counters) = brake();
    assert_eq!(hook(&new_invite(0), src(), 0), PreIngressAction::Accept);
    assert_eq!(hook(&new_invite(1), src(), 1), PreIngressAction::Accept);
    // A non-emergency INVITE at depth 2 WOULD be rejected...
    assert!(matches!(hook(&new_invite(99), src(), 2), PreIngressAction::Reply(_)));
    // ...but the emergency INVITE at the same depth is admitted.
    assert_eq!(
        hook(&invite_buf(2, true, None), src(), 2),
        PreIngressAction::Accept,
        "emergency INVITE must bypass the brake"
    );
    assert_eq!(answered(&counters), 1);
    assert_eq!(answered(&counters), 1);
    assert_eq!(counters.emergency_bypassed(), 1);
}

/// A re-INVITE names an existing dialog by its `To`-tag; the brake sheds new
/// calls only, so an established call is never disturbed by overload.
#[test]
fn in_dialog_reinvites_are_never_braked() {
    let (hook, counters) = brake();
    assert_eq!(
        hook(&invite_buf(7, false, Some("bob-tag-7")), src(), 99),
        PreIngressAction::Accept,
        "a To-tagged INVITE is in-dialog and must not be braked"
    );
    assert_eq!(answered(&counters), 0);
    assert_eq!(counters.emergency_bypassed(), 0);
}

#[test]
fn non_invite_requests_are_not_rejected_by_the_brake() {
    let (hook, counters) = brake();
    for i in 2..5u32 {
        assert!(matches!(hook(&new_invite(i), src(), 2), PreIngressAction::Reply(_)));
    }
    assert_eq!(answered(&counters), 3);
    assert_eq!(
        hook(&options_buf(0), src(), 2),
        PreIngressAction::Accept,
        "non-INVITE must not be rejected"
    );
    assert_eq!(answered(&counters), 3);
    assert_eq!(answered(&counters), 3);
}

#[test]
fn responses_are_accepted() {
    let (hook, counters) = brake();
    assert_eq!(hook(&response_buf(), src(), 99), PreIngressAction::Accept);
    assert_eq!(answered(&counters), 0);
}

#[test]
fn a_malformed_datagram_above_threshold_is_accepted() {
    let (hook, counters) = brake();
    // Looks like an INVITE but does not parse — the normal pipeline owns it.
    let junk = b"INVITE sip:x SIP/2.0\r\nGarbage".to_vec();
    assert_eq!(hook(&junk, src(), 99), PreIngressAction::Accept);
    assert_eq!(answered(&counters), 0);
    assert_eq!(answered(&counters), 0);
}

/// Below the threshold the brake does not parse and never sheds — proved by
/// feeding it a buffer that would be rejected on sight above the threshold.
#[test]
fn below_the_threshold_nothing_is_braked() {
    let (hook, counters) = brake();
    assert_eq!(hook(&new_invite(0), src(), 0), PreIngressAction::Accept);
    assert_eq!(hook(&new_invite(1), src(), 1), PreIngressAction::Accept);
    assert_eq!(hook(&invite_buf(2, true, None), src(), 1), PreIngressAction::Accept);
    assert_eq!(answered(&counters), 0);
    assert_eq!(
        counters.emergency_bypassed(),
        0,
        "below threshold the brake does not classify, so nothing is counted"
    );
}

/// An INVITE another stage refused first draws the same bytes here, on
/// either side of the threshold, and is no new call shed by the brake.
#[test]
fn a_copy_refused_by_another_stage_draws_its_refusal() {
    let refusals = refusals_with_jitter(30);
    let (hook, counters) = brake_over(refusals.clone());
    let invite = new_invite(7);
    assert!(matches!(refusals.memo().refuse(&parsed(&invite)), Memo::Refused { first: true }));
    let expected = serialize(&SipMessage::Response(refusals.memo().answer(&parsed(&invite))));
    assert_eq!(hook(&invite, src(), 0), PreIngressAction::Reply(expected.clone()));
    assert_eq!(hook(&invite, src(), 2), PreIngressAction::Reply(expected));
    assert_eq!(answered(&counters), 2);
    assert_eq!(counters.refused(Class::Normal), 0);
}

/// A copy of an emergency INVITE another stage refused draws that refusal
/// above the threshold too: it is a repeat, not a bypass.
#[test]
fn a_copy_of_a_refused_emergency_invite_is_repeated_not_bypassed() {
    let refusals = refusals_with_jitter(0);
    let (hook, counters) = brake_over(refusals.clone());
    let invite = invite_buf(3, true, None);
    assert!(matches!(refusals.memo().refuse(&parsed(&invite)), Memo::Refused { first: true }));
    assert!(matches!(hook(&invite, src(), 2), PreIngressAction::Reply(_)));
    assert_eq!(counters.emergency_bypassed(), 0);
    assert_eq!(counters.refused(Class::Normal), 0);
}
