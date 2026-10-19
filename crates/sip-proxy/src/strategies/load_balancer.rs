//! [`LoadBalancerStrategy`] — port of `strategies/LoadBalancer.ts`, on a
//! worker's two readiness axes (ADR-0050): liveness routes in-dialog
//! requests, acceptance places new calls.
//!
//! - `select_for_new_dialog`: Call-ID → snapshot the registry → the `Alive`
//!   workers that accept new calls, or every `Alive` one when none accepts (a
//!   refusing worker answers the call itself and writes its record, §6) → for
//!   non-emergency new dialogs, drop `above_critical`-band workers →
//!   `rendezvous_select` → spend one token from the winner's per-worker AIMD
//!   bucket → address. `NoTarget` when the candidate set is empty;
//!   `RateCapExhausted` when the winner's bucket is empty (→ 503 + Retry-After).
//!   Emergency / in-dialog requests bypass the bucket (they still record the
//!   admit for the `share` signal); an unobserved worker is admitted
//!   (bootstrap-friendly). Port of TS LoadBalancer.ts:335-348.
//! - `encode_stickiness`: recover the target's `WorkerId` (= `w_pri`) by
//!   address, a departed-address tombstone included, pick the HRW winner among
//!   the other `Alive` workers that accept new calls, else among the other
//!   `Alive` ones (= `w_bak`, §7), and sign
//!   `v=3|w_pri=…|w_bak=…|e=<0|1>|c=<callId>` (HMAC-SHA256 truncated to 128
//!   bits, base64url) into `{w_pri,w_bak,e,v,kid,sig}`. A dialog a departed
//!   worker originates is thus pinned to its ordinal, and its INVITE responses
//!   and in-dialog requests take `w_bak` until the ordinal rejoins, for the
//!   dialog's life: the core carries the cookie on its own Via too, so it
//!   outlives the tombstone (ADR-0031 D4). An address neither live nor
//!   tombstoned gets no cookie.
//! - `decode_stickiness`: verify the MAC, then `w_pri` while it is `Alive`,
//!   else `w_bak` while it is `Alive`, else `Unroutable` (§4, §5); acceptance
//!   is never read. It resolves the primary by ORDINAL, so a departed-address
//!   tombstone (address-keyed, always `Dead` — see `registry::tombstone`)
//!   never reaches it: an ordinal that left membership resolves to nothing and
//!   takes the backup.

use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sip_clock::Clock;
use sip_message::SipMessage;

use crate::addr::ProxyAddr;
use crate::load_observer::{EluBand, WorkerLoadObserver};
use crate::observability::metrics::ProxyMetrics;
use crate::registry::{WorkerEntry, WorkerRegistry};
use crate::security::hmac::{HmacKeyProvider, TRUNCATED_MAC_BYTES};
use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};

use super::rendezvous::{rendezvous_select, RendezvousCandidate};

mod cookie;

const COOKIE_VERSION: &str = "3";

/// Emergency RPH classification (RFC 4412) — delegates to the single
/// implementation in [`sip_message::emergency`].
fn is_emergency_invite(msg: &SipMessage) -> bool {
    match msg {
        SipMessage::Request(r) => sip_message::emergency::is_emergency_request(r),
        SipMessage::Response(_) => false,
    }
}

/// An in-dialog request carries a non-empty To-tag (AIMD/admission is a
/// new-call knob — never applied to in-dialog traffic).
fn is_in_dialog(msg: &SipMessage) -> bool {
    match msg {
        SipMessage::Request(r) => r.to().tag().is_some_and(|t| !t.is_empty()),
        SipMessage::Response(_) => false,
    }
}

fn call_id_of(msg: &SipMessage) -> Option<&str> {
    let id = match msg {
        SipMessage::Request(r) => r.call_id().as_str(),
        SipMessage::Response(r) => r.call_id().as_str(),
    };
    (!id.is_empty()).then_some(id)
}

/// Adapter so a `WorkerEntry` (by id) can feed `rendezvous_select`.
struct WorkerKey(String);
impl RendezvousCandidate for WorkerKey {
    fn id(&self) -> &str {
        &self.0
    }
}

fn build_stickiness_input(
    primary_id: &str,
    backup_id: &str,
    emergency: &str,
    call_id: &str,
) -> Vec<u8> {
    format!("v={COOKIE_VERSION}|w_pri={primary_id}|w_bak={backup_id}|e={emergency}|c={call_id}")
        .into_bytes()
}

/// The workers a new call may be placed on: those `Alive` and accepting new
/// calls, or, when none accepts, every `Alive` one (ADR-0050 §6, §7).
fn placeable<'w>(workers: impl Iterator<Item = &'w WorkerEntry> + Clone) -> Vec<&'w WorkerEntry> {
    let accepting: Vec<_> = workers.clone().filter(|w| w.accepts_new_calls()).collect();
    if accepting.is_empty() {
        workers.filter(|w| w.is_alive()).collect()
    } else {
        accepting
    }
}

/// The production routing strategy.
pub struct LoadBalancerStrategy {
    registry: Arc<dyn WorkerRegistry>,
    hmac: Arc<dyn HmacKeyProvider>,
    observer: Arc<WorkerLoadObserver>,
    metrics: Arc<ProxyMetrics>,
    clock: Clock,
}

impl LoadBalancerStrategy {
    pub fn new(
        registry: Arc<dyn WorkerRegistry>,
        hmac: Arc<dyn HmacKeyProvider>,
        observer: Arc<WorkerLoadObserver>,
        metrics: Arc<ProxyMetrics>,
        clock: Clock,
    ) -> Self {
        Self { registry, hmac, observer, metrics, clock }
    }

    /// The address of worker `id` while it is `Alive`.
    fn alive_address(&self, id: &str) -> Option<ProxyAddr> {
        if id.is_empty() {
            return None;
        }
        self.registry.resolve(id).filter(WorkerEntry::is_alive).map(|w| w.address)
    }
}

#[async_trait]
impl RoutingStrategy for LoadBalancerStrategy {
    fn name(&self) -> &str {
        "LoadBalancer"
    }

    async fn select_for_new_dialog(
        &self,
        msg: &SipMessage,
        opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError> {
        let call_id = call_id_of(msg).unwrap_or("");
        let is_emergency = opts.emergency_override || is_emergency_invite(msg);
        let in_dialog = is_in_dialog(msg) && !opts.admit_as_new;
        let snapshot = self.registry.snapshot();

        // The accepting workers, or every alive one when none accepts: a
        // refusing worker answers the call itself (ADR-0050 §6).
        let alive = placeable(snapshot.iter());
        let candidates: Vec<_> = if is_emergency || in_dialog {
            alive.clone()
        } else {
            alive
                .iter()
                .copied()
                .filter(|w| self.observer.band_for(&w.id) != Some(EluBand::AboveCritical))
                .collect()
        };

        let keys: Vec<WorkerKey> = candidates.iter().map(|w| WorkerKey(w.id.clone())).collect();
        let chosen = rendezvous_select(call_id, &keys)
            .and_then(|key| candidates.iter().copied().find(|w| w.id == key.0));
        let Some(winner) = chosen else {
            if !is_emergency && !alive.is_empty() {
                self.metrics.record_overload_rejection("no_target_critical_filtered");
            }
            let reason = if snapshot.is_empty() {
                "registry is empty".to_string()
            } else if is_emergency {
                format!("no alive workers among {} entries", snapshot.len())
            } else {
                format!(
                    "all alive workers in above_critical band (snapshot={}, alive={})",
                    snapshot.len(),
                    alive.len()
                )
            };
            return Err(SelectError::NoTarget { reason });
        };

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
        } else if let Err(retry_after_sec) = self.observer.try_consume_for(&winner.id, now_ms) {
            // The Retry-After is the bucket's own time to a token, read from
            // the failed take.
            self.metrics.record_overload_rejection("bucket_empty");
            return Err(SelectError::RateCapExhausted {
                worker_id: winner.id.clone(),
                retry_after_sec,
            });
        } else {
            self.observer.record_own_admitted(&winner.id);
        }
        Ok(winner.address.clone())
    }

    async fn decode_stickiness(&self, route_param: &RouteParams, msg: &SipMessage) -> DecodeResult {
        let (w_pri, w_bak, is_emergency) = match self.check(route_param, msg) {
            Ok(cookie) => (cookie.w_pri, cookie.w_bak, cookie.is_emergency),
            Err(fault) => {
                self.metrics.record_hmac_failure(fault.metric());
                return fault.decode_result();
            }
        };

        // MAC verified: liveness alone places it, never a fresh selection.
        if let Some(target) = self.alive_address(w_pri) {
            return DecodeResult::Forward { target, is_emergency };
        }
        match self.alive_address(w_bak) {
            Some(target) => DecodeResult::ForwardBackup { target, is_emergency },
            None => DecodeResult::Unroutable,
        }
    }

    fn encode_stickiness(&self, target: &ProxyAddr, msg: &SipMessage) -> Option<RouteParams> {
        let call_id = call_id_of(msg)?;
        // A departed-address tombstone answers as the ordinal last bound there.
        let primary = self.registry.lookup_by_address(target)?;
        let snapshot = self.registry.snapshot();
        let backup_keys: Vec<WorkerKey> = placeable(snapshot.iter().filter(|w| w.id != primary.id))
            .into_iter()
            .map(|w| WorkerKey(w.id.clone()))
            .collect();
        let backup_id =
            rendezvous_select(call_id, &backup_keys).map(|k| k.0.clone()).unwrap_or_default();
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

    fn stickiness_primary<'p>(&self, params: &'p RouteParams) -> Option<&'p str> {
        params.get("w_pri").map(String::as_str).filter(|w| !w.is_empty())
    }

    fn stickiness_backup<'p>(&self, params: &'p RouteParams) -> Option<&'p str> {
        params.get("w_bak").map(String::as_str).filter(|w| !w.is_empty())
    }

    fn verified_cookie(&self, params: &RouteParams, msg: &SipMessage) -> Option<RouteParams> {
        self.own_fields(params, msg)
    }
}
