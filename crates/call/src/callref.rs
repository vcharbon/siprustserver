//! `callRef` derivation/parsing + Redis index-key computation — port of the
//! `deriveCallRef` / `parseCallRef` / `callIndexKeys` / `callIndexKeysFromUnknown`
//! helpers in `src/call/CallModel.ts`.
//!
//! Pure over the [`Call`] shape; ported now (ahead of the stateful CallState
//! slice) so the persistence write-path and the recovery read-path stay in
//! lock-step, exactly as the source intends.

use crate::model::Call;

/// The three segments of a `callRef` produced by [`derive_call_ref`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCallRef {
    pub primary: String,
    pub call_id: String,
    pub from_tag: String,
}

/// Derive a deterministic `callRef`: `{primaryOrdinal}|{aLegCallId}|{aLegFromTag}`.
/// Encoding the primary in the ref makes every ref self-describing (HA Option C).
pub fn derive_call_ref(primary_ordinal: &str, a_leg_call_id: &str, a_leg_from_tag: &str) -> String {
    format!("{primary_ordinal}|{a_leg_call_id}|{a_leg_from_tag}")
}

/// Split a `callRef` into its three **borrowed** segments. Returns `None` on
/// malformed input — legacy two-segment refs (pre-HA) round-trip as `None` so
/// callers can detect and upgrade them at the boundary. The single source of the
/// `callRef` grammar; [`parse_call_ref`] and [`call_ref_primary`] both delegate.
fn split_call_ref(reference: &str) -> Option<(&str, &str, &str)> {
    let i1 = reference.find('|')?;
    if i1 == 0 {
        return None;
    }
    let rest = i1 + 1;
    let i2 = rest + reference[rest..].find('|')?;
    // Need ≥1 char between the pipes and ≥1 char after the second pipe.
    if i2 <= i1 + 1 || i2 >= reference.len() - 1 {
        return None;
    }
    Some((&reference[..i1], &reference[i1 + 1..i2], &reference[i2 + 1..]))
}

/// Parse a `callRef` back into its three owned segments. Returns `None` on
/// malformed input — legacy two-segment refs (pre-HA) round-trip as `None` so
/// callers can detect and upgrade them at the boundary.
pub fn parse_call_ref(reference: &str) -> Option<ParsedCallRef> {
    let (primary, call_id, from_tag) = split_call_ref(reference)?;
    Some(ParsedCallRef {
        primary: primary.to_string(),
        call_id: call_id.to_string(),
        from_tag: from_tag.to_string(),
    })
}

/// The primary-ordinal segment of a `callRef`, **borrowed** (no allocation), or
/// `None` on a malformed/legacy ref. For hot-path callers that only need the
/// partition role and not the call-id / from-tag — see `b2bua`'s `role_of`.
pub fn call_ref_primary(reference: &str) -> Option<&str> {
    split_call_ref(reference).map(|(primary, _, _)| primary)
}

/// Compute the flat list of index keys for a call: every leg-tag pair, every
/// b-leg call-id, every dialog's remote tag, and the optional callback context.
pub fn call_index_keys(call: &Call) -> Vec<String> {
    let mut keys = vec![format!("leg:{}|{}", call.a_leg.call_id, call.a_leg.from_tag)];
    for b in &call.b_legs {
        keys.push(format!("leg:{}|{}", b.call_id, b.from_tag));
        keys.push(format!("leg:{}", b.call_id));
        for d in &b.dialogs {
            let b_tag = &d.sip.remote_tag;
            if !b_tag.is_empty() {
                keys.push(format!("leg:{}|{}", b.call_id, b_tag));
            }
        }
    }
    if let Some(ctx) = &call.callback_context {
        keys.push(format!("ctx:{ctx}"));
    }
    keys
}

/// Best-effort structural extraction of [`call_index_keys`] from a schema-tolerant
/// [`serde_json::Value`] projection of a call (e.g. `serde_json::to_value(&call)`).
/// Walks the same field path as [`call_index_keys`]; missing/wrong-typed fields
/// are skipped. A correctly-shaped value always yields the identical key set.
///
/// Field names are the crate's serde names (snake_case) — the msgpack body
/// itself is positional and carries no names, so the schema-tolerant walk only
/// makes sense over this JSON projection (the source's replication puller does
/// the analogous walk over its JSON `bak:` body).
pub fn call_index_keys_from_unknown(state: &serde_json::Value) -> Vec<String> {
    let obj = match state.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let str_field = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let mut keys = Vec::new();

    if let Some(a_leg) = obj.get("a_leg") {
        if let (Some(call_id), Some(from_tag)) =
            (str_field(a_leg, "call_id"), str_field(a_leg, "from_tag"))
        {
            keys.push(format!("leg:{call_id}|{from_tag}"));
        }
    }

    if let Some(b_legs) = obj.get("b_legs").and_then(|v| v.as_array()) {
        for b in b_legs {
            let b_call_id = str_field(b, "call_id");
            if let (Some(cid), Some(ft)) = (&b_call_id, str_field(b, "from_tag")) {
                keys.push(format!("leg:{cid}|{ft}"));
            }
            if let Some(cid) = &b_call_id {
                keys.push(format!("leg:{cid}"));
            }
            if let (Some(cid), Some(dialogs)) =
                (&b_call_id, b.get("dialogs").and_then(|v| v.as_array()))
            {
                for d in dialogs {
                    if let Some(remote_tag) = d
                        .get("sip")
                        .and_then(|s| s.get("remote_tag"))
                        .and_then(|t| t.as_str())
                    {
                        if !remote_tag.is_empty() {
                            keys.push(format!("leg:{cid}|{remote_tag}"));
                        }
                    }
                }
            }
        }
    }

    if let Some(ctx) = obj.get("callback_context").and_then(|v| v.as_str()) {
        keys.push(format!("ctx:{ctx}"));
    }
    keys
}
