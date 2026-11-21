//! Per-call loss injection, composed of two deliberately separate pieces:
//! - **random loss** — a fabric property, so its model is the network layer's
//!   [`sip_net_testkit::RandomLoss`]; this module only holds a per-endpoint instance.
//! - **targeted loss** ([`TargetedDrop`]) — a deterministic, message-precise
//!   drop for reproducing a specific lost-message scenario. Pure test
//!   machinery, so it lives here in the loadgen, NOT in the network layer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use sip_message::sniff::Identity;
use sip_net_testkit::RandomLoss;

/// Which direction a [`TargetedDrop`] fires on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropDir {
    /// Discard on the INBOUND path — a datagram this call's endpoint receives
    /// (models a network loss just before the app reads it). Recovery then
    /// depends on the SENDER re-emitting (e.g. the SUT retransmitting its own
    /// request — which this in-process SUT does NOT do for a fire-and-forget
    /// NOTIFY, making that an UNRECOVERABLE loss → the settle give-up path).
    Inbound,
    /// Discard on the OUTBOUND path — a datagram this call's endpoint sends,
    /// before it hits the wire. When the sending endpoint runs the loadgen
    /// retransmit engine (`--auto-retransmit`), its Timer A/E re-sends the
    /// request, so a one-shot outbound drop is RECOVERED by the harness itself.
    Outbound,
}

/// A deterministic, targeted drop for one call: discard the `nth` DISTINCT (by
/// CSeq number) **request** whose CSeq method is `method` (with `response`,
/// the final response to it instead), on the `dir` path.
/// `permanent: false` drops only its FIRST occurrence — a re-emission gets
/// through, proving loss RECOVERY; `permanent: true` drops every occurrence
/// including re-emissions — proving the bounded give-up path (the actor settle
/// barrier's `Fail`, naming the open obligation). Applied above the retransmit
/// engine, exactly like the random rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetedDrop {
    /// The CSeq method to match (e.g. `"NOTIFY"`, `"BYE"`).
    pub method: &'static str,
    /// Match the final responses (status 200 and above) whose CSeq method is
    /// `method`, not the requests.
    pub response: bool,
    /// With `response`, match the provisionals above 100 instead of the
    /// finals.
    pub provisional: bool,
    /// Which distinct occurrence (1-based, by CSeq number, in arrival order).
    pub nth: u32,
    /// Drop every occurrence (incl. re-emissions) instead of just the first.
    pub permanent: bool,
    /// Which direction to fire on (see [`DropDir`]).
    pub dir: DropDir,
    /// Restrict the drop to ONE callee leg's endpoint (its declared routing
    /// label, e.g. `"bob"`). `None` arms the matcher on EVERY endpoint of the
    /// call — note the arrival counters are PER ENDPOINT, so a multi-leg shape
    /// then drops the nth match on each leg independently. Target the leg when
    /// the repro needs exactly one victim.
    pub leg: Option<&'static str>,
}

impl TargetedDrop {
    /// The first copy of the `nth` distinct `method` request on `dir`, on
    /// every endpoint of the call.
    pub const fn request(method: &'static str, nth: u32, dir: DropDir) -> Self {
        Self { method, response: false, provisional: false, nth, permanent: false, dir, leg: None }
    }

    /// The first copy of the final response to the `nth` distinct `method`
    /// request on `dir`, on every endpoint of the call.
    pub const fn final_response(method: &'static str, nth: u32, dir: DropDir) -> Self {
        Self { response: true, ..Self::request(method, nth, dir) }
    }

    /// The first provisional above 100 to the `nth` distinct `method` request
    /// on `dir`, on every endpoint of the call.
    pub const fn provisional(method: &'static str, nth: u32, dir: DropDir) -> Self {
        Self { provisional: true, ..Self::final_response(method, nth, dir) }
    }

    /// Every copy, re-emissions included.
    pub const fn permanent(self) -> Self {
        Self { permanent: true, ..self }
    }

    /// On the endpoint of the callee leg `leg` only.
    pub const fn on_leg(self, leg: &'static str) -> Self {
        Self { leg: Some(leg), ..self }
    }
}

/// The targeted drop's arrival bookkeeping (per endpoint, like the loss RNG).
#[derive(Default)]
struct TargetState {
    /// Distinct CSeq numbers of matching requests, in first-arrival order.
    seen: Vec<u32>,
    /// The CSeq of the nth distinct match, once identified.
    target_cseq: Option<u32>,
    /// Whether the one-shot (non-permanent) drop already fired.
    fired: bool,
}

/// What the simulated network does to a datagram past losing it (RFC 3261
/// §18: UDP may duplicate and reorder): each sent datagram is, with
/// `dup_rate`, sent again within `late_max`, and each sent response or ACK,
/// with `late_rate`, held back up to `late_max` so later datagrams overtake
/// it. With `echo_rate`, an endpoint is an echo peer, which sends its message
/// again on every reaction to it. Off by default.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NetFaults {
    pub dup_rate: f64,
    pub late_rate: f64,
    pub late_max: Duration,
    pub echo_rate: f64,
}

/// The per-endpoint draws of [`NetFaults`], from their own generator.
struct FaultDraws {
    faults: NetFaults,
    dup: RandomLoss,
    late: RandomLoss,
    echo: RandomLoss,
    span: AtomicU64,
}

impl FaultDraws {
    fn new(faults: NetFaults, seed: u64) -> Self {
        let salt = |k: u64| (seed ^ k.wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1;
        Self {
            faults,
            dup: RandomLoss::new(faults.dup_rate, salt(1)),
            late: RandomLoss::new(faults.late_rate, salt(2)),
            echo: RandomLoss::new(faults.echo_rate, salt(4)),
            span: AtomicU64::new(salt(3)),
        }
    }

    /// A delay in `[1 ms, late_max]`.
    fn delay(&self) -> Duration {
        let mut x = self.span.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.span.store(x, Ordering::Relaxed);
        let max = u64::try_from(self.faults.late_max.as_millis()).unwrap_or(u64::MAX).max(1);
        Duration::from_millis(1 + x % max)
    }
}

/// One call endpoint's loss verdicts: the network-layer random model (applied
/// on `send_to` and on inbound route, each datagram independent with
/// probability `rate` — default 1/1000 when enabled, so P(3 consecutive
/// drops)=1e-9) plus the optional deterministic [`TargetedDrop`].
pub(super) struct DropModel {
    random: RandomLoss,
    targeted: Option<(TargetedDrop, Mutex<TargetState>)>,
    faults: Option<FaultDraws>,
}

impl DropModel {
    pub(super) fn new(rate: f64, seed: u64, targeted: Option<TargetedDrop>) -> Self {
        Self {
            random: RandomLoss::new(rate, seed),
            targeted: targeted.map(|t| (t, Mutex::new(TargetState::default()))),
            faults: None,
        }
    }

    /// The same model, sent datagrams also duplicated and held back as
    /// `faults` states, drawn from `seed`.
    pub(super) fn with_faults(mut self, faults: NetFaults, seed: u64) -> Self {
        if faults != NetFaults::default() {
            self.faults = Some(FaultDraws::new(faults, seed));
        }
        self
    }

    /// When a second copy of the datagram being sent goes out, if one does.
    pub(super) fn copy_after(&self) -> Option<Duration> {
        let f = self.faults.as_ref()?;
        f.dup.drops().then(|| f.delay())
    }

    /// Whether this endpoint is an echo peer, drawn once at its bind, and the
    /// pace of its echoes.
    pub(super) fn echo_pace(&self) -> Option<Duration> {
        let f = self.faults.as_ref()?;
        let paces = super::retransmit::ECHO_PACES;
        f.echo.drops().then(|| paces[usize::from(f.delay().as_millis() % 2 == 1)])
    }

    /// How long the datagram being sent is held back, if it is.
    pub(super) fn held_for(&self) -> Option<Duration> {
        let f = self.faults.as_ref()?;
        f.late.drops().then(|| f.delay())
    }

    /// Whether THIS datagram is dropped by the random model. `false` (no RNG
    /// advance) when disabled.
    pub(super) fn drops(&self) -> bool {
        self.random.drops()
    }

    /// Inbound-path drop verdict: the probabilistic model, plus the targeted
    /// matcher when it fires on the inbound direction.
    pub(super) fn drops_inbound(&self, id: &Identity<'_>) -> bool {
        self.drops() || self.targeted_hit(id, DropDir::Inbound)
    }

    /// Whether the targeted matcher fires on the datagram of identity `id` in
    /// direction `dir`.
    pub(super) fn targeted_hit(&self, id: &Identity<'_>, dir: DropDir) -> bool {
        let Some((td, state)) = &self.targeted else { return false };
        let response = id.status.is_some();
        if td.dir != dir || response != td.response || id.cseq_method != Some(td.method) {
            return false;
        }
        let wanted = if td.provisional { 101..200 } else { 200..u16::MAX };
        if td.response && !id.status.is_some_and(|s| wanted.contains(&s)) {
            return false;
        }
        let Some(cseq) = id.cseq else { return false };
        let mut st = state.lock().unwrap();
        if st.target_cseq.is_none() && !st.seen.contains(&cseq) {
            st.seen.push(cseq);
            if st.seen.len() as u32 == td.nth {
                st.target_cseq = Some(cseq);
            }
        }
        if st.target_cseq != Some(cseq) {
            return false;
        }
        if td.permanent {
            return true;
        }
        if st.fired {
            return false;
        }
        st.fired = true;
        true
    }
}
