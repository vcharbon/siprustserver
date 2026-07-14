//! Per-call loss injection, composed of two deliberately separate pieces:
//! - **random loss** — a fabric property, so its model is the network layer's
//!   [`sip_net::RandomLoss`]; this module only holds a per-endpoint instance.
//! - **targeted loss** ([`TargetedDrop`]) — a deterministic, message-precise
//!   drop for reproducing a specific lost-message scenario. Pure test
//!   machinery, so it lives here in the loadgen, NOT in the network layer.

use std::sync::Mutex;

use sip_message::sniff::{cseq_method_label, cseq_number, is_response};
use sip_net::RandomLoss;

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
/// CSeq number) **request** whose CSeq method is `method`, on the `dir` path.
/// `permanent: false` drops only its FIRST occurrence — a re-emission gets
/// through, proving loss RECOVERY; `permanent: true` drops every occurrence
/// including re-emissions — proving the bounded give-up path (the actor settle
/// barrier's `Fail`, naming the open obligation). Applied above the retransmit
/// engine, exactly like the random rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetedDrop {
    /// The CSeq method to match (e.g. `"NOTIFY"`, `"BYE"`). Requests only.
    pub method: &'static str,
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

/// One call endpoint's loss verdicts: the network-layer random model (applied
/// on `send_to` and on inbound route, each datagram independent with
/// probability `rate` — default 1/1000 when enabled, so P(3 consecutive
/// drops)=1e-9) plus the optional deterministic [`TargetedDrop`].
pub(super) struct DropModel {
    random: RandomLoss,
    targeted: Option<(TargetedDrop, Mutex<TargetState>)>,
}

impl DropModel {
    pub(super) fn new(rate: f64, seed: u64, targeted: Option<TargetedDrop>) -> Self {
        Self {
            random: RandomLoss::new(rate, seed),
            targeted: targeted.map(|t| (t, Mutex::new(TargetState::default()))),
        }
    }

    /// Whether THIS datagram is dropped by the random model. `false` (no RNG
    /// advance) when disabled.
    pub(super) fn drops(&self) -> bool {
        self.random.drops()
    }

    /// Inbound-path drop verdict: the probabilistic model, plus the targeted
    /// matcher when it fires on the inbound direction.
    pub(super) fn drops_inbound(&self, raw: &[u8]) -> bool {
        self.drops() || self.targeted_hit(raw, DropDir::Inbound)
    }

    /// Whether the targeted matcher fires on `raw` in direction `dir`. `raw` is
    /// only scanned when a target for that direction is configured.
    pub(super) fn targeted_hit(&self, raw: &[u8], dir: DropDir) -> bool {
        let Some((td, state)) = &self.targeted else { return false };
        if td.dir != dir || is_response(raw) || cseq_method_label(raw) != td.method {
            return false;
        }
        let Some(cseq) = cseq_number(raw) else { return false };
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
