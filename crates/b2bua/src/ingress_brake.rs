//! The ingress brake: the arrival-time [`PreIngressHook`] installed on the
//! worker's UDP bind, the first rung of the admission ladder
//! ([`crate::admission`]).
//!
//! Its one goal is to **refuse new normal calls when the ingress queue is
//! saturated**, before the datagram is queued and before any transaction or
//! call state exists. Below the threshold, and while the worker remembers no
//! refused INVITE, it is an integer compare and an atomic load, and the packet
//! is accepted untouched.
//!
//! At or above `floor(queue_max * threshold_pct / 100)` the datagram is
//! classified:
//!
//!   - anything whose first bytes are not an `INVITE ` request line — a
//!     response, any other method, garbage — → accept unparsed (the normal
//!     pipeline owns it). The hook runs inline on the socket's drain loop, so
//!     the classes the brake always admits must never cost a parse;
//!   - an `INVITE ` the pipeline's own parser cannot read → accept;
//!   - a copy of an initial INVITE the worker refused in the last 64·T1 — at
//!     this brake or at the transaction layer's deferred backlog, whatever
//!     its class → that refusal again, counted as a copy;
//!   - otherwise the ladder's brake rung judges it in its class
//!     ([`class_of`]): an INVITE carrying a `To`-tag is admitted (the brake
//!     never touches an existing call), an emergency one too, counted on
//!     [`IngressBrakeCounters::emergency_bypassed`]; a normal one is refused
//!     with the worker's one refusal ([`Refusals`]) unless a live server
//!     transaction holds a copy of it (that transaction answers it), its
//!     first refusal counted as a new call refused in its class.
//!
//! Below the threshold, an initial INVITE the worker refused in the last
//! 64·T1 — at this brake or at the transaction layer's deferred backlog —
//! draws that refusal again, so a refused call is never admitted behind its
//! caller's back. Every other datagram below the threshold is accepted.
//!
//! The brake shares the worker's one refusal memo and answer with the
//! transaction layer: every copy of one INVITE draws the same bytes, the
//! stateless-UAS rule of RFC 3261 §8.2.7, whichever stage refused it.
//!
//! Counters are `Arc<AtomicU64>` because a [`PreIngressHook`] is an immutable
//! `Fn` shared across the recv task(s); the read side stays lock-free for the
//! `/metrics` scrape. The memo's lock is taken only on an INVITE above the
//! threshold or, while a refusal is remembered, on an INVITE below it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sip_message::preparse::is_invite_request_buffer;
use sip_message::{serialize, CustomParser, SipMessage, SipParser};
use sip_net::types::{PreIngressAction, PreIngressHook};
use sip_txn::{InviteRefusals, Verdict as Memo};

use crate::admission::{class_of, judge, Class, Refusals, Rung, Verdict};
use crate::new_calls::StatelessCounts;

/// Tunables for the ingress brake. Cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngressBrakeConfig {
    /// The bound on the inbound queue this brake fronts. The brake's
    /// [`threshold`](Self::threshold) is a percentage of it.
    pub queue_max: usize,
    /// Activation threshold as a **percent** of [`queue_max`](Self::queue_max).
    /// The brake engages once the live queue depth reaches
    /// `floor(queue_max * pct / 100)`.
    pub threshold_pct: u32,
}

impl IngressBrakeConfig {
    /// The absolute queue depth at/above which the brake engages:
    /// `floor(queue_max * threshold_pct / 100)`. `u64` widens the product
    /// before the floor-divide so no realistic queue bound can overflow.
    pub fn threshold(&self) -> usize {
        let product = self.queue_max as u64 * u64::from(self.threshold_pct);
        (product / 100) as usize
    }

    /// The most refused identities the worker's memo should hold behind this
    /// brake: a full queue of INVITEs every second for 64·T1, and never fewer
    /// than [`sip_txn::REFUSED_MEMO_MAX`].
    pub fn memo_capacity(&self) -> usize {
        self.queue_max.saturating_mul(32).max(sip_txn::REFUSED_MEMO_MAX)
    }
}

/// The brake's observability surface, as shareable lock-free atomics. One
/// instance is captured by the [`PreIngressHook`] (write side) and retained by
/// the runner (read side, for the `/metrics` scrape). Clone shares the same
/// atomics.
#[derive(Debug, Clone, Default)]
pub struct IngressBrakeCounters {
    /// Initial emergency INVITEs that crossed the threshold and were admitted
    /// anyway — non-zero under flood means the brake is correctly letting
    /// emergency calls through while refusing the rest.
    emergency_bypassed: Arc<AtomicU64>,
    /// INVITEs refused here first, by [`Class`].
    refused: Arc<[AtomicU64; 3]>,
    /// Copies of a refused INVITE refused again here.
    copies: Arc<AtomicU64>,
}

impl IngressBrakeCounters {
    /// Fresh counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Initial emergency INVITEs that crossed the threshold and bypassed the
    /// brake.
    pub fn emergency_bypassed(&self) -> u64 {
        self.emergency_bypassed.load(Ordering::Relaxed)
    }

    /// Record one emergency INVITE that crossed the threshold but bypassed the
    /// brake.
    pub fn record_emergency_bypass(&self) {
        self.emergency_bypassed.fetch_add(1, Ordering::Relaxed);
    }

    /// INVITEs of `class` this brake refused first: each once, however many
    /// of its copies were answered.
    pub fn refused(&self, class: Class) -> u64 {
        self.refused[class.index()].load(Ordering::Relaxed)
    }

    /// Copies of a refused INVITE — refused here or at the backlog — this
    /// brake answered with that refusal again.
    pub fn refused_copies(&self) -> u64 {
        self.copies.load(Ordering::Relaxed)
    }

    /// The brake's first refusals and copies, for the new-call count
    /// ([`crate::new_calls::NewCallCounts::read`]).
    pub fn counts(&self) -> StatelessCounts {
        StatelessCounts {
            refused: Class::ALL.map(|c| self.refused(c)),
            copies: self.refused_copies(),
        }
    }

    fn record_refused(&self, class: Class) {
        self.refused[class.index()].fetch_add(1, Ordering::Relaxed);
    }

    fn record_copy(&self) {
        self.copies.fetch_add(1, Ordering::Relaxed);
    }
}

/// Build the ingress brake [`PreIngressHook`] for the worker's UDP bind.
///
/// The returned closure runs at arrival time for every datagram, with the live
/// inbound-queue `depth`, and applies the classification in the module doc.
/// `refusals` is the worker's one instance, whose memo the transaction layer
/// holds too ([`sip_txn::TransactionConfig::invite_refusals`]).
pub fn build_ingress_brake_hook(
    config: IngressBrakeConfig,
    counters: IngressBrakeCounters,
    refusals: Refusals,
) -> PreIngressHook {
    let threshold = config.threshold();
    let parser = CustomParser::new();
    let memo = refusals.memo().clone();
    Arc::new(move |raw: &[u8], _src, depth: usize| {
        let above = depth >= threshold;
        if !above && !memo.remembers() {
            return PreIngressAction::Accept;
        }
        // Seven bytes decide every class the brake always admits — responses,
        // other methods, garbage — before any parse touches the drain loop.
        if !is_invite_request_buffer(raw) {
            return PreIngressAction::Accept;
        }
        // Anything the pipeline itself could not read is the pipeline's problem,
        // not the brake's: accept and let the normal path answer it.
        let Ok(SipMessage::Request(req)) = parser.parse(raw) else {
            return PreIngressAction::Accept;
        };
        let class = class_of(&req);
        // A copy of an INVITE the worker refused draws that refusal again, on
        // either side of the threshold and whatever its class. A held identity
        // is never refused (see `InviteRefusals`), so a live call is spared.
        if class != Class::InDialog && memo.remembers() && memo.refused(&req) {
            counters.record_copy();
            return reply(&memo, &req);
        }
        match judge(Rung::Brake { depth, threshold }, class) {
            Verdict::Admit => {
                if above && class == Class::Emergency {
                    counters.record_emergency_bypass();
                }
                PreIngressAction::Accept
            }
            Verdict::Refuse(_) => match memo.refuse(&req) {
                Memo::Held => PreIngressAction::Accept,
                Memo::Refused { first } => {
                    if first {
                        counters.record_refused(class);
                    } else {
                        counters.record_copy();
                    }
                    reply(&memo, &req)
                }
            },
        }
    })
}

/// The worker's refusal of `req`, as the datagram the hook sends back.
fn reply(memo: &InviteRefusals, req: &sip_message::SipRequest) -> PreIngressAction {
    PreIngressAction::Reply(serialize(&SipMessage::Response(memo.answer(req))))
}

#[cfg(test)]
mod tests {
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
}
