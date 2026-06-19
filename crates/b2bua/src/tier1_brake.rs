//! Tier-1 overload brake — the `preIngress` POLICY half of the source
//! `UdpTransport` facade (`src/sip/UdpTransport.ts`).
//!
//! ## What this is
//!
//! The arrival-time [`PreIngressHook`] seam already exists in `sip-net`
//! ([`sip_net::types`], honoured by both `real.rs`'s recv loop and
//! `simulated.rs`'s `deliver`). The byte-level decision helpers
//! ([`is_invite_request_buffer`], [`buffer_has_emergency_marker`],
//! [`build_stateless_reject_503_buffer`], [`jittered_retry_after`]) are ported in
//! `sip-message`. **Missing until now: the actual brake policy** — the closure
//! that, at queue depth `>= floor(queue_max * tier1_pct / 100)`, replies a
//! *stateless* 503 to new, non-emergency INVITEs and bumps the brake counters.
//! No production caller built or wired one ([`b2bua_runner`] bound with a bare
//! `BindUdpOpts::new`), so the Tier-1 brake was absent in production. This module
//! is that policy, factored so the runner can `.with_pre_ingress(hook)` it onto
//! the worker socket and surface its counters on `/metrics`.
//!
//! ## Faithful to the TS hook
//!
//! The source hook (`UdpTransport.ts`) is:
//!
//! ```text
//! preIngress = (raw, _rinfo, depth) => {
//!   if (depth >= tier1Threshold && isInviteRequestBuffer(raw) && !bufferHasEmergencyMarker(raw)) {
//!     const retryAfter = jitteredRetryAfter(retryAfterBase, retryAfterJitter)
//!     const respBuf = buildStatelessReject503Buffer(raw, retryAfter)
//!     if (respBuf !== null) { dropsTier1Brake++; tier1RejectSent++; return reply(respBuf) }
//!     // templating failed (malformed buffer) — accept, let the normal pipeline reject
//!   }
//!   return accept()
//! }
//! ```
//!
//! Ported verbatim. Two deliberate Rust-idiom adaptations:
//!
//!   - **Counters are `Arc<AtomicU64>`, not closure-captured `let`s.** A
//!     [`PreIngressHook`] is `Arc<dyn Fn + Send + Sync>` — an *immutable* `Fn`
//!     shared across the recv tasks (and, on the real impl with recv-sharding,
//!     potentially several). The TS `let dropsTier1Brake++` would be a data race
//!     here; atomics are the correct shared-counter shape and keep the read path
//!     ([`Tier1BrakeCounters`]) lock-free for the `/metrics` scrape. They mirror
//!     the `UdpTransportMetrics.dropsTier1Brake` / `tier1RejectSent` surface.
//!
//!   - **`Retry-After` jitter randomness is injected** (a `roll: impl Fn() ->
//!     u64`), because [`jittered_retry_after`] is a pure function in a pure crate
//!     and cannot reach a global RNG. Production passes [`entropy_roll`] (a
//!     dependency-free per-process xorshift64*, the same idiom as
//!     `sip_txn::IdGen::from_entropy`); tests pass a fixed roll (and pin
//!     `retry_after_jitter_sec = 0`, the brake tests' config, so the roll is
//!     never consulted — `base_sec` passes straight through).
//!
//! Note the two brake counters always move in lockstep (the TS increments both on
//! the same line). They are kept distinct to preserve the source's
//! `UdpTransportMetrics` shape (and the three ported tests read both): `drops`
//! is "packets the brake shed", `reject_sent` is "503s the brake emitted" — equal
//! here because every shed emits exactly one reply, but separable should the
//! policy ever grow a silent-drop branch.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sip_message::message_helpers::{
    build_stateless_reject_503_buffer, buffer_has_emergency_marker, is_invite_request_buffer,
    jittered_retry_after,
};
use sip_net::types::{PreIngressAction, PreIngressHook};

/// Tunables for the Tier-1 brake (the subset of `AppConfig` the source hook
/// reads: `udpQueueMax`, `udpQueueTier1ThresholdPct`, `retryAfterBaseSec`,
/// `retryAfterJitterSec`). Cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier1BrakeConfig {
    /// The bound on the inbound queue this brake fronts (`udpQueueMax`). The
    /// brake's [`threshold`](Self::threshold) is a percentage of it.
    pub queue_max: usize,
    /// Activation threshold as a **percent** of [`queue_max`](Self::queue_max)
    /// (`udpQueueTier1ThresholdPct`). The brake engages once the live queue depth
    /// reaches `floor(queue_max * pct / 100)`.
    pub tier1_threshold_pct: u32,
    /// `Retry-After` base seconds stamped on the stateless 503
    /// (`retryAfterBaseSec`).
    pub retry_after_base_sec: u32,
    /// `Retry-After` jitter span seconds (`retryAfterJitterSec`); `0` disables
    /// jitter and the injected roll is never consulted.
    pub retry_after_jitter_sec: u32,
}

impl Tier1BrakeConfig {
    /// The absolute queue depth at/above which the brake engages:
    /// `floor(queue_max * tier1_threshold_pct / 100)` — the exact TS
    /// `Math.floor((queueMax * tier1Pct) / 100)`. Integer arithmetic in `usize`
    /// (the multiply cannot realistically overflow for any sane queue bound, but
    /// `u64` widens the product defensively before the floor-divide).
    pub fn threshold(&self) -> usize {
        let product = self.queue_max as u64 * u64::from(self.tier1_threshold_pct);
        (product / 100) as usize
    }
}

/// The brake's observability surface — the `UdpTransportMetrics.dropsTier1Brake`
/// / `tier1RejectSent` counters, as shareable lock-free atomics. One instance is
/// captured by the [`PreIngressHook`] (write side) and retained by the runner
/// (read side, for the `/metrics` scrape). Clone shares the same atomics.
#[derive(Debug, Clone, Default)]
pub struct Tier1BrakeCounters {
    drops_tier1_brake: Arc<AtomicU64>,
    tier1_reject_sent: Arc<AtomicU64>,
}

impl Tier1BrakeCounters {
    /// Fresh counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// New, non-emergency INVITEs the brake shed (port of
    /// `UdpTransportMetrics.dropsTier1Brake`).
    pub fn drops_tier1_brake(&self) -> u64 {
        self.drops_tier1_brake.load(Ordering::Relaxed)
    }

    /// Stateless 503s the brake emitted back to the source (port of
    /// `UdpTransportMetrics.tier1RejectSent`). Equal to
    /// [`drops_tier1_brake`](Self::drops_tier1_brake) — both move on the same shed.
    pub fn tier1_reject_sent(&self) -> u64 {
        self.tier1_reject_sent.load(Ordering::Relaxed)
    }

    /// Record one brake shed (the TS `dropsTier1Brake++; tier1RejectSent++`).
    /// `pub` so the `UdpTransportMetrics` shape's tests can drive the counters
    /// directly (and a future non-`preIngress` shed path could too); the
    /// production write site remains [`build_tier1_brake_hook`].
    pub fn record_shed(&self) {
        self.drops_tier1_brake.fetch_add(1, Ordering::Relaxed);
        self.tier1_reject_sent.fetch_add(1, Ordering::Relaxed);
    }
}

/// The injected `Retry-After` jitter source — yields a fresh value in
/// `[0, u64::MAX]` per shed (the brake passes it to [`jittered_retry_after`]).
/// `Arc<dyn Fn>` so it clones into the hook closure; `Send + Sync` because the
/// hook runs on the recv task(s). Production: [`entropy_roll`]. Tests: a constant.
pub type RollFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A dependency-free, process-seeded `Retry-After` jitter source.
///
/// Mirrors `sip_txn::IdGen::from_entropy`: seed an xorshift64* once per process
/// from a `RandomState` hash (OS-seeded) folded with the wall clock + PID, then
/// step it per call behind an [`AtomicU64`] CAS loop so concurrent recv tasks
/// each draw a distinct value. This keeps the brake free of a `rand` dependency
/// while giving the 503 `Retry-After` a real spread — overload-protection
/// nondeterminism is explicitly out of scope for the seeded-`Random` plumbing
/// (per the source's `jitteredRetryAfter` note), so a per-process xorshift is the
/// right tool. When `retry_after_jitter_sec == 0` the brake never calls this.
pub fn entropy_roll() -> RollFn {
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678_9ABC_DEF0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(nanos);
    h.write_u64(std::process::id() as u64);
    let seed = h.finish() ^ 0xD1B5_4A32_D192_ED03;
    // Avoid the xorshift fixed point at 0.
    let state = Arc::new(AtomicU64::new(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed }));
    Arc::new(move || {
        loop {
            let cur = state.load(Ordering::Relaxed);
            let mut x = cur;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            if state
                .compare_exchange_weak(cur, x, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return x.wrapping_mul(0x2545_F491_4F6C_DD1D);
            }
        }
    })
}

/// Build the Tier-1 overload-brake [`PreIngressHook`] — the production policy the
/// source `UdpTransport.layer` installs on the worker's `bindUdp`.
///
/// The returned closure runs at arrival time for every datagram, with the live
/// inbound-queue `depth`. It sheds (replies a stateless 503) **only** when all
/// three hold, exactly as the TS hook:
///   1. `depth >= config.threshold()` — the queue has crossed the Tier-1 mark,
///   2. [`is_invite_request_buffer`] — a *new-INVITE request line* (not ACK/BYE/
///      OPTIONS/responses; in-dialog requests and non-INVITE methods are never
///      braked), AND
///   3. `!`[`buffer_has_emergency_marker`] — not an emergency call (`esnet.0` /
///      `wps.0` / `q735.0` Resource-Priority, or an admitted `;emerg=1`/`;em=1`
///      marker), which always bypasses the brake.
///
/// On a shed it bumps `counters` and returns [`PreIngressAction::Reply`] with the
/// byte-templated 503. If [`build_stateless_reject_503_buffer`] returns `None`
/// (a malformed buffer it cannot template), the hook *accepts* and lets the
/// normal pipeline reject — matching the TS `if (respBuf !== null)` fall-through
/// (and it does **not** bump the counters, since nothing was shed). Everything
/// else returns [`PreIngressAction::Accept`].
///
/// `roll` feeds the `Retry-After` jitter; pass [`entropy_roll`] in production.
pub fn build_tier1_brake_hook(
    config: Tier1BrakeConfig,
    counters: Tier1BrakeCounters,
    roll: RollFn,
) -> PreIngressHook {
    let threshold = config.threshold();
    let base = config.retry_after_base_sec;
    let jitter = config.retry_after_jitter_sec;
    Arc::new(move |raw: &[u8], _src, depth: usize| {
        if depth >= threshold
            && is_invite_request_buffer(raw)
            && !buffer_has_emergency_marker(raw)
        {
            let retry_after = jittered_retry_after(base, jitter, || roll());
            if let Some(resp) = build_stateless_reject_503_buffer(raw, retry_after) {
                counters.record_shed();
                return PreIngressAction::Reply(resp);
            }
            // Templating failed (malformed buffer) — accept; the normal pipeline
            // rejects. No counter bump (nothing was shed).
        }
        PreIngressAction::Accept
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- threshold arithmetic (floor(queue_max * pct / 100)) ----

    #[test]
    fn threshold_is_floor_of_queue_max_times_pct() {
        // The brake test's config: queueMax=5, pct=40 → floor(200/100)=2.
        let cfg = Tier1BrakeConfig {
            queue_max: 5,
            tier1_threshold_pct: 40,
            retry_after_base_sec: 5,
            retry_after_jitter_sec: 0,
        };
        assert_eq!(cfg.threshold(), 2);
        // Production-ish: queueMax=8192, pct=70 → floor(573440/100)=5734.
        let prod = Tier1BrakeConfig {
            queue_max: 8192,
            tier1_threshold_pct: 70,
            ..cfg
        };
        assert_eq!(prod.threshold(), 5734);
        // pct=0 disables (threshold 0 would brake from the first INVITE — but a
        // 0% threshold is only set deliberately; the floor is exact).
        assert_eq!(Tier1BrakeConfig { tier1_threshold_pct: 0, ..cfg }.threshold(), 0);
        assert_eq!(Tier1BrakeConfig { tier1_threshold_pct: 100, ..cfg }.threshold(), 5);
    }

    // Shared fixtures mirroring the TS brake test's buffer builders.
    const B2BUA_IP: &str = "127.0.0.1";
    const B2BUA_PORT: u16 = 5060;
    const FLOODER_IP: &str = "10.0.0.1";
    const FLOODER_PORT: u16 = 5555;

    fn invite_buf(i: u32, emergency: bool) -> Vec<u8> {
        let mut s = format!(
            "INVITE sip:bob@{B2BUA_IP}:{B2BUA_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER_IP}:{FLOODER_PORT};branch=z9hG4bK-brake-{i}\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
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

    /// Brake hook under test, with the TS brake test's config (threshold 2,
    /// jitter 0 so the roll never matters) and a panicking roll to PROVE jitter=0
    /// never consults it.
    fn brake() -> (PreIngressHook, Tier1BrakeCounters) {
        let counters = Tier1BrakeCounters::new();
        let cfg = Tier1BrakeConfig {
            queue_max: 5,
            tier1_threshold_pct: 40,
            retry_after_base_sec: 5,
            retry_after_jitter_sec: 0,
        };
        let roll: RollFn = Arc::new(|| panic!("jitter==0 must not draw the roll"));
        let hook = build_tier1_brake_hook(cfg, counters.clone(), roll);
        (hook, counters)
    }

    fn src() -> std::net::SocketAddr {
        format!("{FLOODER_IP}:{FLOODER_PORT}").parse().unwrap()
    }

    fn status_line(buf: &[u8]) -> &[u8] {
        match buf.windows(2).position(|w| w == b"\r\n") {
            Some(end) => &buf[..end],
            None => buf,
        }
    }

    // ---- the three TS cases, now through the WIRED hook + its counters ----

    /// Port of "non-emergency INVITEs past the threshold receive a stateless
    /// 503". The fabric passes depth=0,1 for the first two (accepted) and depth=2
    /// for every one after (>= threshold → reply 503). With 10 INVITEs flooded
    /// into an undrained queue (depth tracks accepted count, capped at 2 here),
    /// `floodCount - 2 = 8` are shed; both counters read 8 and every reply is a
    /// stateless 503.
    #[test]
    fn non_emergency_invites_past_the_threshold_receive_a_stateless_503() {
        let (hook, counters) = brake();
        let flood = 10usize;
        let mut rejects = 0usize;
        for i in 0..flood {
            // Undrained queue: depth equals the count already accepted (0,1,2,2…).
            let depth = i.min(2);
            match hook(&invite_buf(i as u32, false), src(), depth) {
                PreIngressAction::Reply(resp) => {
                    rejects += 1;
                    assert_eq!(status_line(&resp), b"SIP/2.0 503 Service Unavailable");
                    // jitter==0 → Retry-After is exactly the base (5).
                    assert!(
                        find(&resp, b"Retry-After: 5\r\n").is_some(),
                        "503 must carry the base Retry-After; got {:?}",
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
        // The UdpTransportMetrics brake counters: both == expectedRejects.
        assert_eq!(counters.drops_tier1_brake(), (flood - 2) as u64);
        assert_eq!(counters.tier1_reject_sent(), (flood - 2) as u64);
    }

    /// Port of "emergency INVITEs bypass the brake even when above the
    /// threshold". Two non-emergency INVITEs fill to threshold; an emergency
    /// INVITE at depth == threshold is accepted (NOT shed), and the counters stay
    /// at zero.
    #[test]
    fn emergency_invites_bypass_the_brake_even_above_the_threshold() {
        let (hook, counters) = brake();
        assert_eq!(hook(&invite_buf(0, false), src(), 0), PreIngressAction::Accept);
        assert_eq!(hook(&invite_buf(1, false), src(), 1), PreIngressAction::Accept);
        // A non-emergency INVITE at depth 2 WOULD be shed...
        assert!(matches!(
            hook(&invite_buf(99, false), src(), 2),
            PreIngressAction::Reply(_)
        ));
        // ...but the emergency INVITE at the same depth bypasses the brake.
        assert_eq!(
            hook(&invite_buf(2, true), src(), 2),
            PreIngressAction::Accept,
            "emergency INVITE must bypass the brake"
        );
        // The one non-emergency shed above is the only one counted; the two
        // accepted + the emergency contribute nothing.
        assert_eq!(counters.drops_tier1_brake(), 1);
        assert_eq!(counters.tier1_reject_sent(), 1);
    }

    /// Port of "non-INVITE requests are not 503'd by the brake". With the queue
    /// saturated (depth >= threshold), an OPTIONS is accepted — the brake targets
    /// only new INVITEs. Three saturating INVITEs are shed; the OPTIONS is not.
    #[test]
    fn non_invite_requests_are_not_503d_by_the_brake() {
        let (hook, counters) = brake();
        // Three INVITEs at/above threshold are shed (indexes 2..4 in the TS).
        for i in 2..5u32 {
            assert!(matches!(
                hook(&invite_buf(i, false), src(), 2),
                PreIngressAction::Reply(_)
            ));
        }
        assert_eq!(counters.tier1_reject_sent(), 3);
        // The OPTIONS at the same saturated depth is accepted — no extra reply,
        // counters unchanged.
        assert_eq!(
            hook(&options_buf(0), src(), 2),
            PreIngressAction::Accept,
            "non-INVITE must not be 503'd"
        );
        assert_eq!(counters.tier1_reject_sent(), 3);
        assert_eq!(counters.drops_tier1_brake(), 3);
    }

    /// A malformed buffer that crosses the threshold and looks like an INVITE
    /// (starts `INVITE `) but cannot be templated (no header terminator / missing
    /// required headers) is ACCEPTED, not shed — the TS `if (respBuf !== null)`
    /// fall-through — and the counters do NOT move.
    #[test]
    fn malformed_invite_above_threshold_falls_through_to_accept() {
        let (hook, counters) = brake();
        // `INVITE ` prefix (so is_invite_request_buffer is true) but no CRLFCRLF
        // terminator and none of the five required headers → template returns None.
        let junk = b"INVITE sip:x SIP/2.0\r\nGarbage".to_vec();
        assert_eq!(
            hook(&junk, src(), 99),
            PreIngressAction::Accept,
            "un-templatable INVITE must fall through to accept"
        );
        assert_eq!(counters.drops_tier1_brake(), 0);
        assert_eq!(counters.tier1_reject_sent(), 0);
    }

    /// Below the threshold, even a floodable non-emergency INVITE is accepted —
    /// the brake is depth-gated.
    #[test]
    fn invite_below_threshold_is_accepted() {
        let (hook, counters) = brake();
        assert_eq!(hook(&invite_buf(0, false), src(), 0), PreIngressAction::Accept);
        assert_eq!(hook(&invite_buf(1, false), src(), 1), PreIngressAction::Accept);
        assert_eq!(counters.tier1_reject_sent(), 0);
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }
}
