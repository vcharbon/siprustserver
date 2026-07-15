//! [`LoadBalancerStrategy`] — port of `strategies/LoadBalancer.ts`.
//!
//! - `select_for_new_dialog`: Call-ID → snapshot the registry → filter `Alive`
//!   (and, for non-emergency new dialogs, drop `above_critical`-band workers) →
//!   `rendezvous_select` → spend one token from the winner's per-worker AIMD
//!   bucket → address. `NoTarget` when the candidate set is empty;
//!   `RateCapExhausted` when the winner's bucket is empty (→ 503 + Retry-After).
//!   Emergency / in-dialog requests bypass the bucket (they still record the
//!   admit for the `share` signal); an unobserved worker is admitted
//!   (bootstrap-friendly). Port of TS LoadBalancer.ts:335-348.
//! - `encode_stickiness`: recover the target's `WorkerId` (= `w_pri`), pick the
//!   second-best HRW winner among the remaining alive workers (= `w_bak`), and
//!   sign `v=3|w_pri=…|w_bak=…|e=<0|1>|c=<callId>` (HMAC-SHA256 truncated to 128
//!   bits, base64url) into `{w_pri,w_bak,e,v,kid,sig}`.
//! - `decode_stickiness`: verify the MAC, then route over the live registry per
//!   the matrix below (alive / ACK-CANCEL exemption / fresh-pod guard / draining
//!   grace / dead-or-not-ready → backup).

use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sip_clock::Clock;
use sip_message::SipMessage;

use crate::addr::ProxyAddr;
use crate::load_observer::{EluBand, WorkerLoadObserver};
use crate::observability::metrics::{HmacFailureReason, ProxyMetrics};
use crate::registry::{WorkerHealth, WorkerRegistry};
use crate::security::hmac::{HmacKeyProvider, TRUNCATED_MAC_BYTES};
use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};

use super::rendezvous::{rendezvous_select, RendezvousCandidate};

const COOKIE_VERSION: &str = "3";
const DEFAULT_DRAIN_GRACE_MS: u64 = 5_000;
const DEFAULT_FRESH_POD_GUARD_MS: u64 = 20_000;

/// Emergency RPH classification (RFC 4412) — delegates to the single
/// implementation in [`sip_message::message_helpers::emergency`].
fn is_emergency_invite(msg: &SipMessage) -> bool {
    match msg {
        SipMessage::Request(r) => sip_message::message_helpers::is_emergency_request(r),
        SipMessage::Response(_) => false,
    }
}

/// An in-dialog request carries a non-empty To-tag (AIMD/admission is a
/// new-call knob — never applied to in-dialog traffic).
fn is_in_dialog(msg: &SipMessage) -> bool {
    match msg {
        SipMessage::Request(r) => r.to.tag.as_deref().is_some_and(|t| !t.is_empty()),
        SipMessage::Response(_) => false,
    }
}

fn is_ack_or_cancel(msg: &SipMessage) -> bool {
    matches!(msg, SipMessage::Request(r) if r.method == "ACK" || r.method == "CANCEL")
}

fn call_id_of(msg: &SipMessage) -> Option<&str> {
    let id = match msg {
        SipMessage::Request(r) => &r.call_id,
        SipMessage::Response(r) => &r.call_id,
    };
    (!id.is_empty()).then_some(id.as_str())
}

/// Adapter so a `WorkerEntry` (by id) can feed `rendezvous_select`.
struct WorkerKey(String);
impl RendezvousCandidate for WorkerKey {
    fn id(&self) -> &str {
        &self.0
    }
}

fn build_stickiness_input(primary_id: &str, backup_id: &str, emergency: &str, call_id: &str) -> Vec<u8> {
    format!("v={COOKIE_VERSION}|w_pri={primary_id}|w_bak={backup_id}|e={emergency}|c={call_id}").into_bytes()
}

/// Config for the LB (source defaults).
#[derive(Debug, Clone, Copy)]
pub struct LoadBalancerConfig {
    pub drain_grace_ms: u64,
    pub fresh_pod_guard_ms: u64,
}

impl Default for LoadBalancerConfig {
    fn default() -> Self {
        Self { drain_grace_ms: DEFAULT_DRAIN_GRACE_MS, fresh_pod_guard_ms: DEFAULT_FRESH_POD_GUARD_MS }
    }
}

/// The production routing strategy.
pub struct LoadBalancerStrategy {
    registry: Arc<dyn WorkerRegistry>,
    hmac: Arc<dyn HmacKeyProvider>,
    observer: Arc<WorkerLoadObserver>,
    metrics: Arc<ProxyMetrics>,
    clock: Clock,
    config: LoadBalancerConfig,
}

impl LoadBalancerStrategy {
    pub fn new(
        registry: Arc<dyn WorkerRegistry>,
        hmac: Arc<dyn HmacKeyProvider>,
        observer: Arc<WorkerLoadObserver>,
        metrics: Arc<ProxyMetrics>,
        clock: Clock,
        config: LoadBalancerConfig,
    ) -> Self {
        Self { registry, hmac, observer, metrics, clock, config }
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    /// Resolve `w_bak` → `ForwardBackup` when the named backup is alive;
    /// otherwise `Unknown` (core falls back to a fresh selection).
    fn try_backup(&self, backup_id: &str, is_emergency: bool) -> DecodeResult {
        if backup_id.is_empty() {
            return DecodeResult::Unknown { is_emergency };
        }
        match self.registry.resolve(backup_id) {
            Some(bak) if bak.health == WorkerHealth::Alive => {
                DecodeResult::ForwardBackup { target: bak.address, is_emergency }
            }
            _ => DecodeResult::Unknown { is_emergency },
        }
    }
}

#[async_trait]
impl RoutingStrategy for LoadBalancerStrategy {
    fn name(&self) -> &str {
        "LoadBalancer"
    }

    async fn select_for_new_dialog(&self, msg: &SipMessage, opts: SelectOpts) -> Result<ProxyAddr, SelectError> {
        let call_id = call_id_of(msg).unwrap_or("");
        let is_emergency = opts.emergency_override || is_emergency_invite(msg);
        let in_dialog = is_in_dialog(msg);
        let snapshot = self.registry.snapshot();

        let alive: Vec<_> = snapshot.iter().filter(|w| w.health == WorkerHealth::Alive).collect();
        let candidates: Vec<_> = if is_emergency || in_dialog {
            alive.clone()
        } else {
            alive.iter().copied().filter(|w| self.observer.band_for(&w.id) != Some(EluBand::AboveCritical)).collect()
        };

        if candidates.is_empty() {
            if !is_emergency && !alive.is_empty() {
                self.metrics.record_overload_rejection("no_target_critical_filtered");
            }
            let reason = if snapshot.is_empty() {
                "registry is empty".to_string()
            } else if is_emergency {
                format!("no alive workers among {} entries", snapshot.len())
            } else {
                format!("all alive workers in above_critical band (snapshot={}, alive={})", snapshot.len(), alive.len())
            };
            return Err(SelectError::NoTarget { reason });
        }

        let keys: Vec<WorkerKey> = candidates.iter().map(|w| WorkerKey(w.id.clone())).collect();
        let winner_idx = {
            let winner = rendezvous_select(call_id, &keys).expect("non-empty candidates");
            keys.iter().position(|k| k.0 == winner.0).expect("winner is in keys")
        };
        let winner = candidates[winner_idx];

        // Per-worker AIMD token bucket (port of TS LoadBalancer.ts:335-348).
        // Emergency and in-dialog requests bypass the bucket entirely —
        // in-dialog: the call is already admitted, AIMD is a new-call knob; the
        // bypass still records the admit so `own_admitted_rate`/`share` stays
        // truthful. Otherwise spend a token: an empty bucket is the LB rate-capping
        // this (alive, non-critical) worker so it can shed in-flight load → 503 +
        // Retry-After. An unobserved worker has no bucket and is admitted
        // (`try_consume_for` is bootstrap-friendly).
        let now_ms = self.clock.now_ms();
        if is_emergency || in_dialog {
            // Emergency bypasses BOTH the above_critical critical-filter (above)
            // and this AIMD bucket — count emergency new-dialog bypasses so the
            // skip is observable under flood (in-dialog is not a new-call admit).
            if is_emergency && !in_dialog {
                self.metrics.record_lb_emergency_bypass();
            }
            self.observer.record_own_admitted(&winner.id);
        } else if !self.observer.try_consume_for(&winner.id, now_ms) {
            self.metrics.record_overload_rejection("bucket_empty");
            // `retry_after_sec_for` gives a real per-bucket Retry-After (seconds
            // until ≥1 token refills) — a deliberate refinement over the TS
            // constant `retryAfterSec: 1` (LoadBalancer.ts:345), since the bucket
            // already knows its own cap/fill rate. See load_observer.rs.
            let retry_after_sec = self.observer.retry_after_sec_for(&winner.id, now_ms).max(1);
            return Err(SelectError::RateCapExhausted { worker_id: winner.id.clone(), retry_after_sec });
        } else {
            self.observer.record_own_admitted(&winner.id);
        }
        Ok(winner.address.clone())
    }

    async fn decode_stickiness(&self, route_param: &RouteParams, msg: &SipMessage) -> DecodeResult {
        let w_pri = route_param.get("w_pri").map(String::as_str);
        let w_bak = route_param.get("w_bak").map(String::as_str);
        let e = route_param.get("e").map(String::as_str);
        let v = route_param.get("v").map(String::as_str);
        let kid = route_param.get("kid").map(String::as_str);
        let sig = route_param.get("sig").map(String::as_str);

        // `w_bak` may legitimately be present-but-empty; `w_pri`/`kid`/`sig` must
        // be non-empty; `e` must be "0" or "1".
        let (Some(w_pri), Some(w_bak), Some(e), Some(v), Some(kid), Some(sig)) = (w_pri, w_bak, e, v, kid, sig)
        else {
            self.metrics.record_hmac_failure(HmacFailureReason::Missing);
            return DecodeResult::Unknown { is_emergency: false };
        };
        if (e != "0" && e != "1") || w_pri.is_empty() || kid.is_empty() || sig.is_empty() {
            self.metrics.record_hmac_failure(HmacFailureReason::Missing);
            return DecodeResult::Unknown { is_emergency: false };
        }
        if v != COOKIE_VERSION {
            self.metrics.record_hmac_failure(HmacFailureReason::Decode);
            return DecodeResult::Reject { status: 403, reason: format!("unsupported stickiness cookie version \"{v}\"") };
        }
        let Some(call_id) = call_id_of(msg) else {
            self.metrics.record_hmac_failure(HmacFailureReason::Decode);
            return DecodeResult::Reject { status: 403, reason: "missing Call-ID for stickiness verify".to_string() };
        };
        let Ok(decoded) = URL_SAFE_NO_PAD.decode(sig) else {
            self.metrics.record_hmac_failure(HmacFailureReason::Decode);
            return DecodeResult::Reject { status: 403, reason: "malformed stickiness signature".to_string() };
        };
        if decoded.len() != TRUNCATED_MAC_BYTES {
            self.metrics.record_hmac_failure(HmacFailureReason::Decode);
            return DecodeResult::Reject {
                status: 403,
                reason: format!("malformed stickiness signature (length={})", decoded.len()),
            };
        }

        let is_emergency = e == "1";
        let input = build_stickiness_input(w_pri, w_bak, e, call_id);
        if !self.hmac.verify_truncated(&input, kid, &decoded) {
            self.metrics.record_hmac_failure(HmacFailureReason::Mismatch);
            return DecodeResult::Reject { status: 403, reason: "stickiness signature mismatch".to_string() };
        }

        // MAC verified — resolve the primary.
        let Some(primary) = self.registry.resolve(w_pri) else {
            return self.try_backup(w_bak, is_emergency);
        };

        // ACK/CANCEL exemption: an alive primary owns its in-flight UAS state.
        if primary.health == WorkerHealth::Alive && is_ack_or_cancel(msg) {
            return DecodeResult::Forward { target: primary.address, is_emergency };
        }
        if primary.health == WorkerHealth::Alive {
            // Fresh-pod guard: a respawned pod whose Ready raced ahead of its
            // first OPTIONS round-trip routes to the backup that still holds the
            // call.
            if let Some(first_seen) = primary.first_seen_at_ms {
                let age = self.now_ms().saturating_sub(first_seen);
                if age < self.config.fresh_pod_guard_ms {
                    let promoted = self.try_backup(w_bak, is_emergency);
                    if matches!(promoted, DecodeResult::ForwardBackup { .. }) {
                        self.metrics.record_decode_forward_promoted("unobserved-fresh-pod");
                        return promoted;
                    }
                    // No usable backup — fall through to the (likely-empty) primary.
                }
            }
            return DecodeResult::Forward { target: primary.address, is_emergency };
        }
        if primary.health == WorkerHealth::Draining {
            if let Some(since) = primary.draining_since {
                if self.now_ms().saturating_sub(since) <= self.config.drain_grace_ms {
                    // Pre-grace: in-flight re-INVITE/UPDATE/INFO completes on the primary.
                    return DecodeResult::Forward { target: primary.address, is_emergency };
                }
            }
            return self.try_backup(w_bak, is_emergency);
        }
        // dead / unknown / not-ready → backup.
        let promoted = self.try_backup(w_bak, is_emergency);
        if primary.health == WorkerHealth::NotReady && matches!(promoted, DecodeResult::ForwardBackup { .. }) {
            self.metrics.record_decode_forward_promoted("not-ready");
        }
        promoted
    }

    fn encode_stickiness(&self, target: &ProxyAddr, msg: &SipMessage) -> Option<RouteParams> {
        let call_id = call_id_of(msg)?;
        let snapshot = self.registry.snapshot();
        let primary = snapshot.iter().find(|w| &w.address == target)?;
        let backup_keys: Vec<WorkerKey> = snapshot
            .iter()
            .filter(|w| w.health == WorkerHealth::Alive && w.id != primary.id)
            .map(|w| WorkerKey(w.id.clone()))
            .collect();
        let backup_id = rendezvous_select(call_id, &backup_keys).map(|k| k.0.clone()).unwrap_or_default();
        let emergency_flag = if is_emergency_invite(msg) { "1" } else { "0" };
        let input = build_stickiness_input(&primary.id, &backup_id, emergency_flag, call_id);
        let signed = self.hmac.sign(&input);
        let sig = URL_SAFE_NO_PAD.encode(&signed.mac[..TRUNCATED_MAC_BYTES]);

        let mut params = RouteParams::new();
        params.insert("w_pri".into(), primary.id.clone());
        params.insert("w_bak".into(), backup_id);
        params.insert("e".into(), emergency_flag.to_string());
        params.insert("v".into(), COOKIE_VERSION.to_string());
        params.insert("kid".into(), signed.kid);
        params.insert("sig".into(), sig);
        Some(params)
    }
}
