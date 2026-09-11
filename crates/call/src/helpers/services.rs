//! Accessors over the per-service typed runtime slices
//! ([`crate::model::services`]) and the opaque ext slots. The slice types
//! themselves do NOT live here — see [`crate::model::services`].

use crate::model::{Call, ExtMap};

use super::lens::update_leg;

// ── relayFirst18xTo180 ──────────────────────────────────────────────────────

/// The active `relayFirst18xTo180` strategy for this call, if any.
pub fn relay_first_18x_strategy(call: &Call) -> Option<crate::features::RelayFirst18xStrategy> {
    call.features.as_ref().and_then(|f| f.relay_first_18x_to_180.as_ref()).map(|r| r.strategy)
}

/// Whether the first 18x has already been relayed under the strategy.
pub fn relay_first_18x_first_relayed(call: &Call) -> bool {
    call.relay_first_18x.as_ref().map(|s| s.first_relayed).unwrap_or(false)
}

/// The active `relay18x.messages` policy (defaults to `FIRST` when the feature
/// or the field is absent).
pub fn relay_first_18x_messages(call: &Call) -> crate::features::Relay18xMessages {
    call.features
        .as_ref()
        .and_then(|f| f.relay_first_18x_to_180.as_ref())
        .map(|r| r.messages)
        .unwrap_or_default()
}

/// Whether an 18x with this *upstream* status value was already relayed
/// (the `ONE_PER_VALUE` dedupe test).
pub fn relay_first_18x_value_relayed(call: &Call, status: u16) -> bool {
    call.relay_first_18x.as_ref().map(|s| s.relayed_values.contains(&status)).unwrap_or(false)
}

/// Record an *upstream* 18x status value as relayed (the `ONE_PER_VALUE`
/// dedupe ledger; bounded — one entry per distinct 18x value).
pub fn record_relay_first_18x_value(mut call: Call, status: u16) -> Call {
    let s = call.relay_first_18x.get_or_insert_with(Default::default);
    if !s.relayed_values.contains(&status) {
        s.relayed_values.push(status);
    }
    call
}

/// The a-facing To-tag minted on the first 18x (reused on the 200 OK).
pub fn relay_first_18x_stored_a_tag(call: &Call) -> Option<&str> {
    call.relay_first_18x.as_ref().and_then(|s| s.stored_a_tag.as_deref())
}

/// Mark the first 18x relayed and record the minted a-facing tag. Preserves the
/// `ONE_PER_VALUE` dedupe ledger (a later relayed 18x re-runs this with the
/// same stored tag).
pub fn set_relay_first_18x_relayed(mut call: Call, stored_a_tag: &str) -> Call {
    let s = call.relay_first_18x.get_or_insert_with(Default::default);
    s.first_relayed = true;
    s.stored_a_tag = Some(stored_a_tag.to_string());
    call
}

// ── promote18xPemTo200 ──────────────────────────────────────────────────────

/// The current PEM runtime slice, if any.
pub fn promote_pem_state(call: &Call) -> Option<&crate::model::PromotePemState> {
    call.promote_pem.as_ref()
}

/// Whether the first 183+PEM has been promoted to a synthetic 200 OK.
pub fn promote_pem_promoted(call: &Call) -> bool {
    call.promote_pem.as_ref().map(|s| s.promoted).unwrap_or(false)
}

/// Whether the promotion window is open (Alice's in-dialog requests rejected).
pub fn promote_pem_window_open(call: &Call) -> bool {
    call.promote_pem.as_ref().map(|s| s.window_open).unwrap_or(false)
}

/// Overwrite the PEM runtime slice (`None` resets to the pre-promotion state).
pub fn set_promote_pem(mut call: Call, state: Option<crate::model::PromotePemState>) -> Call {
    call.promote_pem = state;
    call
}

// ── REFER transfer ──────────────────────────────────────────────────────────

/// Whether the routing decision directed LOCAL REFER processing for this call
/// (the [`crate::features::ReferFeature`] arm). Absent, the platform relays a
/// REFER to the peer leg like any other in-dialog method.
pub fn refer_processed_locally(call: &Call) -> bool {
    call.features.as_ref().is_some_and(|f| f.refer.is_some())
}

/// The current REFER transfer runtime slice, if any.
pub fn transfer_state(call: &Call) -> Option<&crate::model::TransferState> {
    call.transfer.as_ref()
}

/// Whether a transfer is active (the slice is present) — the service guard.
pub fn transfer_active(call: &Call) -> bool {
    call.transfer.is_some()
}

/// Overwrite the transfer runtime slice (`None` clears it — the terminal path).
pub fn set_transfer(mut call: Call, state: Option<crate::model::TransferState>) -> Call {
    call.transfer = state;
    call
}

// ── Established-call reroute ────────────────────────────────────────────────

/// Overwrite the established-call reroute slice (`None` clears it — the
/// completion / rollback path). Mirrors [`set_transfer`].
pub fn set_reroute(mut call: Call, state: Option<crate::model::RerouteState>) -> Call {
    call.reroute = state;
    call
}

// ── Opaque ext slots (ADR-0016) ─────────────────────────────────────────────

/// Write an encoded ext slice into `call.ext[serviceId]`; `None` drops the key.
pub fn set_call_ext(mut call: Call, service_id: &str, value: Option<serde_json::Value>) -> Call {
    match value {
        None => {
            if let Some(ext) = &mut call.ext {
                ext.remove(service_id);
            }
        }
        Some(v) => {
            call.ext.get_or_insert_with(ExtMap::new).insert(service_id.to_string(), v);
        }
    }
    call
}

/// Write an encoded ext slice into the named leg's `ext[serviceId]`.
pub fn set_leg_ext(call: Call, leg_id: &str, service_id: &str, value: serde_json::Value) -> Call {
    update_leg(call, leg_id, |leg| {
        leg.ext.get_or_insert_with(ExtMap::new).insert(service_id.to_string(), value);
    })
}
