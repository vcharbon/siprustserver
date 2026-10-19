//! `callRef` derivation and parsing: the a-leg identity a call is keyed by.

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
/// malformed input — two-segment refs (no ordinal) round-trip as `None` so
/// callers can detect and upgrade them at the boundary. The single source of the
/// `callRef` grammar; [`parse_call_ref`] and [`call_ref_primary`] both delegate.
fn split_call_ref(reference: &str) -> Option<(&str, &str, &str)> {
    let (primary, rest) = reference.split_once('|')?;
    let (call_id, from_tag) = rest.split_once('|')?;
    // Every segment holds at least one char.
    if primary.is_empty() || call_id.is_empty() || from_tag.is_empty() {
        return None;
    }
    Some((primary, call_id, from_tag))
}

/// Parse a `callRef` back into its three owned segments. Returns `None` on
/// malformed input — two-segment refs (no ordinal) round-trip as `None` so
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
