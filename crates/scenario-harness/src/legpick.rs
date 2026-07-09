//! R-URI leg-picker — the shared multi-callee routing primitive.
//!
//! When several callee-side UAs of ONE call share a single bound socket (the
//! B2BUA egresses every callee leg to one ROUTE target, so Bob / Charlie / David
//! all land on one address), something has to say *which logical receiver owns a
//! freshly-arrived out-of-dialog leg*. That decision is the [`LegPicker`]: a
//! pure function of a [`LegInfo`] view over the raw datagram, returning the label
//! (the logical agent's routing key) that should own the leg.
//!
//! This lives in `scenario-harness` — the shared home — so BOTH consumers use
//! the one mechanism: the functional/e2e [`crate::agent::Harness`] multi-callee
//! facility (`callee_group`, which dispatches inbound INVITEs to distinct
//! logical [`Agent`](crate::Agent)s by this picker) and the load generator's
//! socket mux (`loadgen::mux`, which uses it as its second demux tier once the
//! per-call correlation token has already selected the call instance). Neither
//! re-implements a bespoke picker.
//!
//! The picker owns *no* SIP state — the scenario owns the meaning of the fields.
//! [`prefix_leg_picker`] is the ready-made policy (longest R-URI-user prefix);
//! a scenario can supply any `Fn(&LegInfo) -> String` for a custom key (To-user,
//! `X-Api-Call`, …).

use std::sync::Arc;

/// A read-only view of an inbound datagram handed to a [`LegPicker`]. It exists
/// purely so a scenario can disambiguate which of its receivers should own a new
/// leg, keying on whatever it likes (R-URI, To, `X-Api-Call`, a custom header) —
/// the picker, not this view, owns the meaning of these fields.
pub struct LegInfo<'a> {
    raw: &'a [u8],
}

impl<'a> LegInfo<'a> {
    /// View the raw datagram bytes for picking. The bytes are borrowed — the
    /// view never owns or mutates them.
    pub fn new(raw: &'a [u8]) -> Self {
        Self { raw }
    }

    /// The raw datagram bytes.
    pub fn raw(&self) -> &[u8] {
        self.raw
    }
    /// Value of header `name` (case-insensitive), or `None`.
    pub fn header(&self, name: &str) -> Option<String> {
        header_value(self.raw, name)
    }
    /// The Request-URI (the full 2nd token of the request line).
    pub fn ruri(&self) -> Option<String> {
        ruri(self.raw)
    }
    /// The request method (first token of the start line), or `None` for a
    /// response — lets a dispatcher tell an out-of-dialog INVITE (which the
    /// picker routes) from an in-dialog request or a response (which follow the
    /// dialog's owner).
    pub fn method(&self) -> Option<String> {
        let line = first_line(self.raw);
        if line.starts_with("SIP/2.0") {
            return None; // response
        }
        line.split_whitespace().next().map(str::to_string)
    }
    /// The Request-URI user-part (e.g. `dave` from `sip:dave@host`).
    pub fn ruri_user(&self) -> Option<String> {
        self.ruri().as_deref().and_then(uri_user)
    }
    /// The To header user-part.
    pub fn to_user(&self) -> Option<String> {
        self.header("to").or_else(|| self.header("t")).as_deref().and_then(uri_user)
    }
}

/// A scenario-owned callback that picks which receiver (by its label) should own
/// a freshly-arrived leg when **more than one** receiver shares a single socket
/// for the same call. Returning a label that matches no receiver drops the leg
/// as a no-route orphan (observable, never mis-delivered).
///
/// Contract: the picker may be called while the dispatcher holds a lock, so it
/// MUST be pure-ish — it must not re-enter the dispatcher. A panic is contained
/// (the leg becomes a no-route orphan), not propagated.
pub type LegPicker = Arc<dyn Fn(&LegInfo) -> String + Send + Sync>;

/// A ready-made prefix-matching [`LegPicker`] — the leg-demux tier for the
/// several callee legs of ONE call that share a single socket (bob1 / bob2 /
/// charlie, "distinguished by prefix"; or their distinct R-URI user digits in a
/// transfer).
///
/// It returns the label whose value is the **longest prefix of the leg's
/// Request-URI user-part**. The egress addresses each callee by a distinct
/// user-part (`sip:bob2@…`, `sip:charlie@…`, or the transfer's per-role digits),
/// so the user-part names the leg; a per-call suffix (`bob2-<tag>`) still routes
/// by its label prefix, and longest-match keeps `bob` vs `bob2` (or `bob1` vs
/// `bob10`) unambiguous.
///
/// A leg whose R-URI user prefixes NONE of `labels` yields `""` — the caller
/// counts it a no-route orphan (observable, never mis-delivered).
pub fn prefix_leg_picker(labels: impl IntoIterator<Item = impl Into<String>>) -> LegPicker {
    labelled_prefix_leg_picker(labels.into_iter().map(|l| {
        let l = l.into();
        (l.clone(), l)
    }))
}

/// A [`prefix_leg_picker`] whose prefixes carry an EXPLICIT label — `entries`
/// are `(ruri_prefix, label)` pairs, so a leg's on-wire routing key need not
/// equal the receiver that owns it. That is the general case: an open-registry
/// load shape's callee legs arrive under **number-plan digits** (`+041…`,
/// `0491…`), never under the agent name, and several prefixes may select ONE
/// leg (a callee reachable under more than one number form).
///
/// The label of the **longest matching prefix** wins (nested number forms —
/// a full transfer number vs its `0650033033…` sibling prefix — stay
/// unambiguous, the same rule `callee_group` applies); a duplicate prefix
/// resolves to the first declared. No matching prefix yields `""` — the caller
/// counts it a no-route orphan.
pub fn labelled_prefix_leg_picker(
    entries: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
) -> LegPicker {
    let entries: Vec<(String, String)> =
        entries.into_iter().map(|(p, l)| (p.into(), l.into())).collect();
    Arc::new(move |leg: &LegInfo| {
        let user = leg.ruri_user().unwrap_or_default();
        let mut best: Option<&(String, String)> = None;
        for entry in &entries {
            if user.starts_with(entry.0.as_str())
                && best.is_none_or(|b| entry.0.len() > b.0.len())
            {
                best = Some(entry);
            }
        }
        best.map(|(_, label)| label.clone()).unwrap_or_default()
    })
}

// ---------------------------------------------------------------------------
// Byte-level header/URI scanners (header block is ASCII/UTF-8; the body — which
// may be binary — is never inspected by these).
// ---------------------------------------------------------------------------

fn as_str(raw: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(raw)
}

fn first_line(raw: &[u8]) -> String {
    as_str(raw).lines().next().unwrap_or("").trim().to_string()
}

/// The Request-URI (2nd token of the request line), or `None` for a response.
fn ruri(raw: &[u8]) -> Option<String> {
    let line = first_line(raw);
    if line.starts_with("SIP/2.0") {
        return None; // response
    }
    line.split_whitespace().nth(1).map(str::to_string)
}

/// The user-part of a SIP URI (handles `<sip:user@host>`, `sip:user@host`,
/// name-addr with a display name). A userless URI (`sip:host`) yields `None`.
fn uri_user(value: &str) -> Option<String> {
    let v = value.trim();
    let inner = match (v.find('<'), v.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => &v[a + 1..b],
        _ => v,
    };
    let no_scheme = inner
        .strip_prefix("sips:")
        .or_else(|| inner.strip_prefix("sip:"))
        .unwrap_or(inner);
    let (user, _host) = no_scheme.split_once('@')?;
    if user.is_empty() || user.contains(' ') {
        None
    } else {
        Some(user.to_string())
    }
}

/// Value of header `name` (case-insensitive), scanning the header block only.
fn header_value(raw: &[u8], name: &str) -> Option<String> {
    let s = as_str(raw);
    for line in s.lines() {
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((h, v)) = line.split_once(':') {
            if h.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An INVITE with a specific Request-URI (+ matching To), for the leg-picker
    /// tests — a picker is handed a `LegInfo` over exactly these bytes.
    fn invite_ruri(ruri: &str) -> Vec<u8> {
        format!(
            "INVITE {ruri} SIP/2.0\r\nCall-ID: c1@h\r\nTo: <{ruri}>\r\n\
             From: <sip:a@h>;tag=1\r\nCSeq: 1 INVITE\r\n\r\n"
        )
        .into_bytes()
    }

    /// The picker routes each shared-socket leg to the receiver whose label is
    /// the LONGEST prefix of the R-URI user — bob / bob2 / charlie on one port,
    /// including a per-call-suffixed user, with longest-match resolving
    /// bob-vs-bob2; an unknown user (or a response with no R-URI) is a
    /// no-route (`""`).
    #[test]
    fn prefix_leg_picker_routes_by_longest_ruri_user_prefix() {
        let pick = prefix_leg_picker(["bob", "bob2", "charlie"]);
        let route = |ruri: &str| {
            let raw = invite_ruri(ruri);
            pick(&LegInfo::new(raw.as_slice()))
        };

        assert_eq!(route("sip:bob@10.0.0.1:5070"), "bob");
        // "bob2" is prefixed by both "bob" and "bob2" → longest wins.
        assert_eq!(route("sip:bob2@10.0.0.1:5070"), "bob2");
        assert_eq!(route("sip:charlie@10.0.0.1:5070"), "charlie");
        // A per-call suffix on the user still routes by its label prefix.
        assert_eq!(route("sip:bob2-lg99@10.0.0.1:5070"), "bob2");
        assert_eq!(route("sip:charlie.7f3a@10.0.0.1:5070"), "charlie");
        // No label prefixes the user → no route (a no_route orphan at the caller).
        assert_eq!(route("sip:dave@10.0.0.1:5070"), "");
        // A response (no Request-URI) is a no-route, never a panic.
        assert_eq!(pick(&LegInfo::new(&b"SIP/2.0 200 OK\r\n\r\n"[..])), "");
    }

    /// Longest-match disambiguates labels that are prefixes of each other even
    /// when the shorter also matches (`bob1` vs `bob10`).
    #[test]
    fn prefix_leg_picker_longest_match_disambiguates_numeric_siblings() {
        let pick = prefix_leg_picker(["bob1", "bob10"]);
        let route = |ruri: &str| {
            let raw = invite_ruri(ruri);
            pick(&LegInfo::new(raw.as_slice()))
        };
        assert_eq!(route("sip:bob10@h"), "bob10");
        assert_eq!(route("sip:bob1@h"), "bob1");
        assert_eq!(route("sip:bob10x@h"), "bob10");
    }

    /// The labelled picker routes number-plan prefixes to their ROLE: the leg's
    /// on-wire key (`+041…`, `0491…` — a Business-Layer number rewrite) never
    /// contains the receiver's name, several prefixes select one leg, and the
    /// longest matching prefix beats a nested sibling (the newkah transfer
    /// target vs its `0650033033…` prefix).
    #[test]
    fn labelled_prefix_leg_picker_routes_number_prefixes_to_roles() {
        let pick = labelled_prefix_leg_picker([
            // The routed callee is reachable under TWO number forms.
            ("+04", "bob"),
            ("0590", "bob"),
            // The MRF resource digits.
            ("0491", "mrf"),
            // Nested by construction: the full transfer number must beat its
            // sibling prefix.
            ("0650033033", "charlie"),
            ("0650033033231089055", "xfer"),
        ]);
        let route = |ruri: &str| {
            let raw = invite_ruri(ruri);
            pick(&LegInfo::new(raw.as_slice()))
        };

        assert_eq!(route("sip:+0415551234@10.0.0.1:5070"), "bob");
        assert_eq!(route("sip:059012345@10.0.0.1:5070"), "bob");
        assert_eq!(route("sip:04912@10.0.0.1:5070"), "mrf");
        assert_eq!(route("sip:065003303399@10.0.0.1:5070"), "charlie");
        // Longest matching prefix wins across legs, not first-declared.
        assert_eq!(route("sip:0650033033231089055@10.0.0.1:5070"), "xfer");
        // No prefix matches → no route (a no_route orphan at the caller).
        assert_eq!(route("sip:999@10.0.0.1:5070"), "");
    }

    /// `to_user` / `header` read the header block (name-addr and bare-URI To),
    /// the shape the load mux's To-user correlation relies on.
    #[test]
    fn leginfo_reads_to_user_and_headers() {
        let raw = invite_ruri("sip:charlie@10.0.0.1:5070");
        let leg = LegInfo::new(raw.as_slice());
        assert_eq!(leg.to_user().as_deref(), Some("charlie"));
        assert_eq!(leg.ruri_user().as_deref(), Some("charlie"));
        assert_eq!(leg.header("call-id").as_deref(), Some("c1@h"));
    }
}
