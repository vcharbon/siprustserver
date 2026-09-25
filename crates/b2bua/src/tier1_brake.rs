//! Tier-1 overload brake: the arrival-time [`PreIngressHook`] installed on the
//! worker's UDP bind.
//!
//! Its one goal is to **reject new non-emergency calls when the ingress queue
//! is saturated**, before the datagram is queued and before any transaction or
//! call state exists. Below the threshold it is a single integer compare and
//! the packet is accepted untouched.
//!
//! At or above `floor(queue_max * tier1_threshold_pct / 100)` the datagram is
//! classified:
//!
//!   - anything whose first bytes are not an `INVITE ` request line — a
//!     response, any other method, garbage — → accept unparsed (the normal
//!     pipeline owns it). The hook runs inline on the socket's drain loop, so
//!     the classes the brake always admits must never cost a parse;
//!   - an `INVITE ` the pipeline's own parser cannot read → accept;
//!   - INVITE carrying a `To`-tag (in-dialog / re-INVITE) → accept — the brake
//!     never touches an existing call;
//!   - initial INVITE that [`is_emergency_request`] marks emergency → accept,
//!     counted on [`Tier1BrakeCounters::emergency_bypassed`];
//!   - initial non-emergency INVITE → reply with the shared
//!     [`build_reject_new_call_503`], counted on
//!     [`Tier1BrakeCounters::drops_tier1_brake`] /
//!     [`tier1_reject_sent`](Tier1BrakeCounters::tier1_reject_sent).
//!
//! The reject carries no transaction state, so this tier is a stateless UAS:
//! its `To`-tag and `Retry-After` jitter are derived from the request through
//! [`StatelessRejectTagger`], and a retransmitted INVITE is answered with the
//! identical 503 (RFC 3261 §8.2.7).
//!
//! Tier-3 ([`crate::overload`]) sheds the same class of traffic with the same
//! response once the message has reached the router; this tier exists to shed
//! it a queue earlier, without paying for transaction state.
//!
//! Counters are `Arc<AtomicU64>` because a [`PreIngressHook`] is an immutable
//! `Fn` shared across the recv task(s); the read side stays lock-free for the
//! `/metrics` scrape.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sip_message::emergency::is_emergency_request;
use sip_message::preparse::is_invite_request_buffer;
use sip_message::{serialize, CustomParser, SipMessage, SipParser};
use sip_net::types::{PreIngressAction, PreIngressHook};
use sip_txn::IdGen;

use crate::overload::{build_reject_new_call_503, jittered_retry_after, StatelessRejectTagger};

/// Tunables for the Tier-1 brake. Cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier1BrakeConfig {
    /// The bound on the inbound queue this brake fronts. The brake's
    /// [`threshold`](Self::threshold) is a percentage of it.
    pub queue_max: usize,
    /// Activation threshold as a **percent** of [`queue_max`](Self::queue_max).
    /// The brake engages once the live queue depth reaches
    /// `floor(queue_max * pct / 100)`.
    pub tier1_threshold_pct: u32,
    /// `Retry-After` base seconds stamped on the reject.
    pub retry_after_base_sec: u32,
    /// `Retry-After` jitter span seconds; `0` pins every reject to the base.
    /// Otherwise the offset is request-derived, so it spreads a shed fleet
    /// without varying between retransmissions of one call.
    pub retry_after_jitter_sec: u32,
}

impl Tier1BrakeConfig {
    /// The absolute queue depth at/above which the brake engages:
    /// `floor(queue_max * tier1_threshold_pct / 100)`. `u64` widens the product
    /// before the floor-divide so no realistic queue bound can overflow.
    pub fn threshold(&self) -> usize {
        let product = self.queue_max as u64 * u64::from(self.tier1_threshold_pct);
        (product / 100) as usize
    }
}

/// The brake's observability surface, as shareable lock-free atomics. One
/// instance is captured by the [`PreIngressHook`] (write side) and retained by
/// the runner (read side, for the `/metrics` scrape). Clone shares the same
/// atomics.
#[derive(Debug, Clone, Default)]
pub struct Tier1BrakeCounters {
    drops_tier1_brake: Arc<AtomicU64>,
    tier1_reject_sent: Arc<AtomicU64>,
    /// Initial emergency INVITEs that crossed the threshold and were admitted
    /// anyway — non-zero under flood means the brake is correctly letting
    /// emergency calls through while shedding the rest.
    emergency_bypassed: Arc<AtomicU64>,
}

impl Tier1BrakeCounters {
    /// Fresh counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Initial non-emergency INVITEs the brake shed.
    pub fn drops_tier1_brake(&self) -> u64 {
        self.drops_tier1_brake.load(Ordering::Relaxed)
    }

    /// Rejects the brake emitted back to the source. Equal to
    /// [`drops_tier1_brake`](Self::drops_tier1_brake) — both move on the same
    /// shed; kept distinct so a future silent-drop branch stays separable.
    pub fn tier1_reject_sent(&self) -> u64 {
        self.tier1_reject_sent.load(Ordering::Relaxed)
    }

    /// Initial emergency INVITEs that crossed the threshold and bypassed the
    /// brake.
    pub fn emergency_bypassed(&self) -> u64 {
        self.emergency_bypassed.load(Ordering::Relaxed)
    }

    /// Record one brake shed. `pub` so the `UdpTransportMetrics` shape's tests
    /// can drive the counters directly; the production write site is
    /// [`build_tier1_brake_hook`].
    pub fn record_shed(&self) {
        self.drops_tier1_brake.fetch_add(1, Ordering::Relaxed);
        self.tier1_reject_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one emergency INVITE that crossed the threshold but bypassed the
    /// brake.
    pub fn record_emergency_bypass(&self) {
        self.emergency_bypassed.fetch_add(1, Ordering::Relaxed);
    }
}

/// Build the Tier-1 brake [`PreIngressHook`] for the worker's UDP bind.
///
/// The returned closure runs at arrival time for every datagram, with the live
/// inbound-queue `depth`, and applies the classification in the module doc.
/// `id_gen` seeds the [`StatelessRejectTagger`] the rejects are identified by;
/// it is consulted here, not per datagram.
pub fn build_tier1_brake_hook(
    config: Tier1BrakeConfig,
    counters: Tier1BrakeCounters,
    id_gen: &IdGen,
) -> PreIngressHook {
    let threshold = config.threshold();
    let base = config.retry_after_base_sec;
    let jitter = config.retry_after_jitter_sec;
    let parser = CustomParser::new();
    let tagger = StatelessRejectTagger::from_id_gen(id_gen);
    Arc::new(move |raw: &[u8], _src, depth: usize| {
        if depth < threshold {
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
        // The brake sheds NEW calls only: an INVITE that already names a dialog
        // (To-tag → in-dialog re-INVITE) is admitted.
        if req.to().tag().is_some() {
            return PreIngressAction::Accept;
        }
        if is_emergency_request(&req) {
            counters.record_emergency_bypass();
            return PreIngressAction::Accept;
        }
        let (to_tag, roll) = tagger.for_request(&req);
        let retry_after = jittered_retry_after(base, jitter, || roll);
        let resp = build_reject_new_call_503(to_tag, &req, retry_after);
        counters.record_shed();
        PreIngressAction::Reply(serialize(&SipMessage::Response(resp)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- threshold arithmetic (floor(queue_max * pct / 100)) ----

    #[test]
    fn threshold_is_floor_of_queue_max_times_pct() {
        // The brake test's config: queue_max=5, pct=40 → floor(200/100)=2.
        let cfg = Tier1BrakeConfig {
            queue_max: 5,
            tier1_threshold_pct: 40,
            retry_after_base_sec: 5,
            retry_after_jitter_sec: 0,
        };
        assert_eq!(cfg.threshold(), 2);
        // Production-ish: queue_max=8192, pct=70 → floor(573440/100)=5734.
        let prod = Tier1BrakeConfig { queue_max: 8192, tier1_threshold_pct: 70, ..cfg };
        assert_eq!(prod.threshold(), 5734);
        // pct=0 would brake from the first INVITE — only ever set deliberately;
        // the floor is exact.
        assert_eq!(Tier1BrakeConfig { tier1_threshold_pct: 0, ..cfg }.threshold(), 0);
        assert_eq!(Tier1BrakeConfig { tier1_threshold_pct: 100, ..cfg }.threshold(), 5);
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

    /// Brake hook under test: threshold 2, jitter `jitter_sec`.
    fn brake_with_jitter(jitter_sec: u32) -> (PreIngressHook, Tier1BrakeCounters) {
        let counters = Tier1BrakeCounters::new();
        let cfg = Tier1BrakeConfig {
            queue_max: 5,
            tier1_threshold_pct: 40,
            retry_after_base_sec: 5,
            retry_after_jitter_sec: jitter_sec,
        };
        let hook = build_tier1_brake_hook(cfg, counters.clone(), &IdGen::seeded(1));
        (hook, counters)
    }

    /// Brake hook under test: threshold 2, jitter 0 (so `Retry-After` is
    /// exactly the base).
    fn brake() -> (PreIngressHook, Tier1BrakeCounters) {
        brake_with_jitter(0)
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
                        find(&resp, b"Reason: SIP;cause=503;text=\"overload\"\r\n").is_some(),
                        "reject must carry the overload cause; got {:?}",
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
        assert_eq!(counters.drops_tier1_brake(), (flood - 2) as u64);
        assert_eq!(counters.tier1_reject_sent(), (flood - 2) as u64);
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
        assert!(to_line.contains(";tag="), "Tier-1 reject must tag To: {to_line}");
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
        assert_eq!(counters.tier1_reject_sent(), 2);
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
        assert_eq!(counters.drops_tier1_brake(), 1);
        assert_eq!(counters.tier1_reject_sent(), 1);
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
        assert_eq!(counters.drops_tier1_brake(), 0);
        assert_eq!(counters.emergency_bypassed(), 0);
    }

    #[test]
    fn non_invite_requests_are_not_rejected_by_the_brake() {
        let (hook, counters) = brake();
        for i in 2..5u32 {
            assert!(matches!(hook(&new_invite(i), src(), 2), PreIngressAction::Reply(_)));
        }
        assert_eq!(counters.tier1_reject_sent(), 3);
        assert_eq!(
            hook(&options_buf(0), src(), 2),
            PreIngressAction::Accept,
            "non-INVITE must not be rejected"
        );
        assert_eq!(counters.tier1_reject_sent(), 3);
        assert_eq!(counters.drops_tier1_brake(), 3);
    }

    #[test]
    fn responses_are_accepted() {
        let (hook, counters) = brake();
        assert_eq!(hook(&response_buf(), src(), 99), PreIngressAction::Accept);
        assert_eq!(counters.tier1_reject_sent(), 0);
    }

    #[test]
    fn a_malformed_datagram_above_threshold_is_accepted() {
        let (hook, counters) = brake();
        // Looks like an INVITE but does not parse — the normal pipeline owns it.
        let junk = b"INVITE sip:x SIP/2.0\r\nGarbage".to_vec();
        assert_eq!(hook(&junk, src(), 99), PreIngressAction::Accept);
        assert_eq!(counters.drops_tier1_brake(), 0);
        assert_eq!(counters.tier1_reject_sent(), 0);
    }

    /// Below the threshold the brake does not parse and never sheds — proved by
    /// feeding it a buffer that would be rejected on sight above the threshold.
    #[test]
    fn below_the_threshold_nothing_is_braked() {
        let (hook, counters) = brake();
        assert_eq!(hook(&new_invite(0), src(), 0), PreIngressAction::Accept);
        assert_eq!(hook(&new_invite(1), src(), 1), PreIngressAction::Accept);
        assert_eq!(hook(&invite_buf(2, true, None), src(), 1), PreIngressAction::Accept);
        assert_eq!(counters.tier1_reject_sent(), 0);
        assert_eq!(
            counters.emergency_bypassed(),
            0,
            "below threshold the brake does not classify, so nothing is counted"
        );
    }
}
