//! Port of `tests/harness/rules/rfc/_dialog-model.ts` plus the per-dialog
//! projector from `tests/harness/projections.ts:projectPerDialog`.
//!
//! [`DialogModel`] is a light per-agent dialog state (UAC/UAS role, route set,
//! dialog URIs, INVITE branches). Each cross-message rule walks the ordered
//! (sent + received) event stream for one agent slot and feeds each message
//! through [`advance_dialog_model`] before / after its own check.
//!
//! [`project_per_dialog`] slices the flat recorded channel into per-`(bind,
//! Call-ID, unordered tag-pair)` [`AgentSlot`]s. The RFC 3261 §12 dialog id is
//! direction-independent (local/remote tags swap with the viewpoint), so a
//! mid-dialog request initiated by the callee side — a UAS hold re-INVITE or a
//! UAS give-up BYE, whose From/To tags are REVERSED relative to the
//! dialog-establishing INVITE — lands in the same bucket as the INVITE and its
//! confirming 2xx (newkahneed-029: a From-tag-oriented key split those into a
//! bucket with no 200, misreading a confirmed-dialog BYE as early-dialog).
//! Forked early dialogs that share a Call-ID + caller tag but differ on the
//! callee tag are distinct pairs and stay distinct slices; messages recorded
//! before the callee's tag exists (INVITE, 100) land in a single-tag pending
//! bucket and migrate into the pair-keyed bucket when the second tag appears.
//! Each [`DialogSlice`] reports its tags in **establishing orientation**
//! (`from_tag` = the tag that appeared alone first, i.e. the caller's).

use std::collections::HashMap;
use std::net::SocketAddr;

use layer_harness::{LaneKey, Stamped};
use sip_message::message_helpers::{get_headers, parse_via_params};
use sip_message::{SipHeader, SipMessage, SipParser, SipRequest};

use crate::contracts::SignalingNetworkEvent;

// ---------------------------------------------------------------------------
// Generic message accessors (typed where possible, lenient otherwise)
// ---------------------------------------------------------------------------

/// The raw header list, for the few multi-value lookups (Record-Route, Route).
pub fn msg_headers(m: &SipMessage) -> &[SipHeader] {
    m.headers()
}

/// `From` tag, in either direction (the originator's tag rides From on requests
/// and the responses to them).
pub fn from_tag(m: &SipMessage) -> Option<&str> {
    match m {
        SipMessage::Request(r) => r.from.tag.as_deref(),
        SipMessage::Response(r) => r.from.tag.as_deref(),
    }
}

/// `To` tag (the answerer's dialog tag; absent on the dialog-creating request
/// and on 100 Trying).
pub fn to_tag(m: &SipMessage) -> Option<&str> {
    match m {
        SipMessage::Request(r) => r.to.tag.as_deref(),
        SipMessage::Response(r) => r.to.tag.as_deref(),
    }
}

/// `From` URI (name-addr, no params).
pub fn from_uri(m: &SipMessage) -> &str {
    match m {
        SipMessage::Request(r) => &r.from.uri,
        SipMessage::Response(r) => &r.from.uri,
    }
}

/// `To` URI (name-addr, no params).
pub fn to_uri(m: &SipMessage) -> &str {
    match m {
        SipMessage::Request(r) => &r.to.uri,
        SipMessage::Response(r) => &r.to.uri,
    }
}

pub fn call_id(m: &SipMessage) -> &str {
    match m {
        SipMessage::Request(r) => &r.call_id,
        SipMessage::Response(r) => &r.call_id,
    }
}

/// CSeq method token (`INVITE`, `BYE`, …).
pub fn cseq_method(m: &SipMessage) -> &str {
    match m {
        SipMessage::Request(r) => r.cseq.method.as_str(),
        SipMessage::Response(r) => r.cseq.method.as_str(),
    }
}

pub fn cseq_seq(m: &SipMessage) -> u32 {
    match m {
        SipMessage::Request(r) => r.cseq.seq,
        SipMessage::Response(r) => r.cseq.seq,
    }
}

/// Response status, or `0` for a request (mirrors the TS `msg.status` guard
/// that only ever fires on responses).
pub fn status(m: &SipMessage) -> u16 {
    match m {
        SipMessage::Request(_) => 0,
        SipMessage::Response(r) => r.status,
    }
}

/// The top (first) `Via` `branch=` token, if present and non-empty.
pub fn top_via_branch(m: &SipMessage) -> Option<String> {
    let top = get_headers(msg_headers(m), "via").into_iter().next()?;
    parse_via_params(top).branch.filter(|b| !b.is_empty())
}

// ---------------------------------------------------------------------------
// Header utilities (TS `getHeaderValue` / `routeIsLoose` / `extractRouteUri`)
// ---------------------------------------------------------------------------

/// A Route value advertises loose routing iff it carries an `;lr` parameter
/// (followed by a delimiter / end). Mirrors the TS `/;lr(?=[;>,\s]|$)/i`.
pub fn route_is_loose(route_value: &str) -> bool {
    let lower = route_value.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while let Some(pos) = lower[i..].find(";lr") {
        let at = i + pos;
        let after = at + 3;
        match bytes.get(after) {
            None => return true,
            Some(&c) if c == b';' || c == b'>' || c == b',' || c.is_ascii_whitespace() => {
                return true
            }
            _ => i = after,
        }
    }
    false
}

/// Split a possibly comma-combined header value (Record-Route / Route) into its
/// individual entries, respecting angle brackets and quoted strings — RFC 3261
/// §7.3.1 lets a UA fold multiple rows into one comma-separated header, so a
/// route-set comparison must normalise both sides to individual routes or it
/// miscounts a folded header as one route (the harness `RecordRouteFold` makes
/// this routine load-bearing for the proxy/b2bua route-set checks).
pub fn split_header_list(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut in_angle, mut in_quote, mut start) = (false, false, 0usize);
    for (i, c) in value.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            '<' if !in_quote => in_angle = true,
            '>' if !in_quote => in_angle = false,
            ',' if !in_quote && !in_angle => {
                let piece = value[start..i].trim();
                if !piece.is_empty() {
                    out.push(piece.to_string());
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = value[start..].trim();
    if !last.is_empty() {
        out.push(last.to_string());
    }
    out
}

/// All individual Route/Record-Route entries on `m` for `name`, comma-folds split.
pub fn split_header_values(m: &SipMessage, name: &str) -> Vec<String> {
    get_headers(msg_headers(m), name)
        .into_iter()
        .flat_map(split_header_list)
        .collect()
}

/// The URI inside a `<...>` Route value, or the trimmed value if bare.
pub fn extract_route_uri(route_value: &str) -> String {
    let trimmed = route_value.trim();
    if let Some(rest) = trimmed.strip_prefix('<') {
        if let Some(end) = rest.find('>') {
            return rest[..end].to_string();
        }
    }
    trimmed.to_string()
}

// ---------------------------------------------------------------------------
// SDP origin parsing (TS `parseSdpOrigin`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSdpOrigin {
    pub username: String,
    pub session_id: String,
    pub session_version: i64,
    pub nettype: String,
    pub addrtype: String,
    pub unicast_address: String,
    pub raw_origin_line: String,
    /// The SDP body with the `o=` line removed — for "everything but origin
    /// changed" comparisons.
    pub body_digest_excluding_origin: String,
}

pub fn parse_sdp_origin(body: &[u8]) -> Option<ParsedSdpOrigin> {
    if body.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(body);
    if !text.starts_with("v=0") {
        return None;
    }
    let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let o_idx = lines.iter().position(|l| l.starts_with("o="))?;
    let o_line = lines[o_idx];
    let parts: Vec<&str> = o_line[2..].split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    let session_version = parts[2].parse::<i64>().ok()?;
    let others = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != o_idx)
        .map(|(_, l)| *l)
        .collect::<Vec<_>>()
        .join("\n");
    Some(ParsedSdpOrigin {
        username: parts[0].to_string(),
        session_id: parts[1].to_string(),
        session_version,
        nettype: parts[3].to_string(),
        addrtype: parts[4].to_string(),
        unicast_address: parts[5].to_string(),
        raw_origin_line: o_line.to_string(),
        body_digest_excluding_origin: others,
    })
}

// ---------------------------------------------------------------------------
// Light dialog model (TS `DialogModel` / `advanceDialogModel`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct DialogModel {
    pub call_id: String,
    pub local_tag: String,
    pub remote_tag: String,
    pub dialog_local_uri: String,
    pub dialog_remote_uri: String,
    /// Record-Route values in route-set order (reversed for the UAC).
    pub route_set: Vec<String>,
    pub is_uac: bool,
    pub is_uas: bool,
    pub initial_invite_sent_branch: String,
    pub initial_invite_received_branch: String,
}

impl DialogModel {
    pub fn empty() -> Self {
        Self::default()
    }
}

/// One element of an agent slot's merged timeline.
#[derive(Clone, Debug)]
pub struct OrderedEvent {
    pub kind: EventKind,
    /// Global capture position (post-sort) — used only for stable ordering.
    pub idx: usize,
    pub msg: SipMessage,
    /// For `Sent`: the wire destination (`SendCalled.to`). For `Received`: the
    /// wire source (`RecvItem.packet.src`).
    pub wire_peer: Option<SocketAddr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Sent,
    Received,
}

/// Rebuild dialog state incrementally, exactly as the TS `advanceDialogModel`.
///
/// Only a **dialog-establishing** INVITE (no To-tag) can seed the UAC/UAS role
/// and the `initial_invite_*` branches: with unordered tag-pair bucketing a
/// UAS-initiated re-INVITE (both tags, reversed orientation) shares the slice
/// with the establishing INVITE, and mistaking it for the initial INVITE would
/// flip the slot's role and corrupt the route set / dialog URIs.
pub fn advance_dialog_model(m: &mut DialogModel, ev: &OrderedEvent) {
    let msg = &ev.msg;
    fn set_if_empty(slot: &mut String, val: &str) {
        if slot.is_empty() && !val.is_empty() {
            *slot = val.to_string();
        }
    }
    fn is_establishing_invite(req: &SipRequest) -> bool {
        req.method.as_str() == "INVITE" && req.to.tag.as_deref().is_none_or(str::is_empty)
    }

    if ev.kind == EventKind::Sent {
        if let SipMessage::Request(req) = msg {
            if is_establishing_invite(req) && m.initial_invite_sent_branch.is_empty() {
                m.is_uac = true;
                m.initial_invite_sent_branch = top_via_branch(msg).unwrap_or_default();
                set_if_empty(&mut m.call_id, call_id(msg));
                if let Some(ft) = from_tag(msg) {
                    set_if_empty(&mut m.local_tag, ft);
                }
                set_if_empty(&mut m.dialog_local_uri, from_uri(msg));
                set_if_empty(&mut m.dialog_remote_uri, to_uri(msg));
            }
            return;
        }
        // Sent response.
        if cseq_method(msg) == "INVITE" && status(msg) > 100 && m.local_tag.is_empty() {
            if let Some(tt) = to_tag(msg) {
                m.local_tag = tt.to_string();
            }
        }
        return;
    }

    // Received.
    if let SipMessage::Request(req) = msg {
        if is_establishing_invite(req) && m.initial_invite_received_branch.is_empty() {
            m.is_uas = true;
            m.initial_invite_received_branch = top_via_branch(msg).unwrap_or_default();
            set_if_empty(&mut m.call_id, call_id(msg));
            set_if_empty(&mut m.dialog_local_uri, to_uri(msg));
            set_if_empty(&mut m.dialog_remote_uri, from_uri(msg));
            if let Some(ft) = from_tag(msg) {
                set_if_empty(&mut m.remote_tag, ft);
            }
            if m.route_set.is_empty() {
                let rr = split_header_values(msg, "record-route");
                if !rr.is_empty() {
                    m.route_set = rr;
                }
            }
        }
        return;
    }

    // Received response.
    if cseq_method(msg) == "INVITE" && status(msg) > 100 {
        if let Some(tt) = to_tag(msg) {
            set_if_empty(&mut m.remote_tag, tt);
        }
    }
    if m.is_uac && m.route_set.is_empty() {
        let st = status(msg);
        let is_dialog_creating =
            (200..300).contains(&st) || (st > 100 && st < 200 && to_tag(msg).is_some());
        if is_dialog_creating && cseq_method(msg) == "INVITE" {
            let mut rr = split_header_values(msg, "record-route");
            if !rr.is_empty() {
                rr.reverse();
                m.route_set = rr;
            }
        }
    }
}

/// Is `req` an **in-dialog** request given the dialog state walked so far? (Both
/// tags present, and — for a re-INVITE — not the initial INVITE by branch.)
pub fn is_in_dialog_request(req: &SipRequest, m: &DialogModel) -> bool {
    let ft = req.from.tag.as_deref().unwrap_or("");
    let tt = req.to.tag.as_deref().unwrap_or("");
    if ft.is_empty() || tt.is_empty() {
        return false;
    }
    if req.method.as_str() == "INVITE" {
        let branch = req.via.first().branch.clone().unwrap_or_default();
        if !branch.is_empty() && branch == m.initial_invite_sent_branch {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Per-dialog projector (TS `projectPerDialog`)
// ---------------------------------------------------------------------------

/// One agent's view of one dialog: the merged, time-ordered (sent + received)
/// event stream the cross rules walk.
#[derive(Clone, Debug)]
pub struct AgentSlot {
    pub bind_key: LaneKey,
    pub ordered: Vec<OrderedEvent>,
}

/// True if this slot both **sent** and **received** an initial INVITE for the
/// dialog — i.e. the bind is a transparent (record-routing) proxy relaying both
/// directions of one Call-ID, not a dialog endpoint. Per-UA dialog invariants
/// (mid-dialog URI / Route stability, Contact, …) do NOT apply to a relay: it
/// legitimately carries the originator's From/To/Route through unchanged, so a
/// per-UA rule that walks one dialog model over a relay's mixed-direction stream
/// would false-positive. B2BUA legs are NOT relays here — each leg is a distinct
/// Call-ID, so its slot only ever sends OR receives the leg's INVITE. Per-UA
/// cross rules should `continue` past a relay slot.
///
/// Only the **dialog-establishing** INVITE (no To-tag) counts: with unordered
/// tag-pair bucketing a UAS-initiated re-INVITE (both tags present) lands in
/// the same slice as the establishing INVITE, and an endpoint that sent the
/// initial INVITE and later *received* such a re-INVITE must not be
/// misclassified as a relay. A true relay forwards the establishing INVITE in
/// both directions, so it still qualifies.
pub fn slot_is_relay(slot: &AgentSlot) -> bool {
    let mut sent_invite = false;
    let mut recv_invite = false;
    for ev in &slot.ordered {
        if let SipMessage::Request(r) = &ev.msg {
            if r.method.as_str() == "INVITE" && r.to.tag.as_deref().is_none_or(str::is_empty) {
                match ev.kind {
                    EventKind::Sent => sent_invite = true,
                    EventKind::Received => recv_invite = true,
                }
            }
        }
    }
    sent_invite && recv_invite
}

/// All agent slots that share a `(Call-ID, unordered tag-pair)` dialog
/// identity (RFC 3261 §12 — the dialog id is direction-independent). The tags
/// are reported in **establishing orientation**: `from_tag` is the caller's
/// tag (the tag that appeared alone, before the callee's tag existed) even
/// when the slice also carries callee-initiated mid-dialog requests whose
/// wire From/To are reversed. `to_tag = None` is a pending (never-confirmed)
/// early slice keyed by its single tag.
#[derive(Clone, Debug)]
pub struct DialogSlice {
    pub call_id: String,
    pub from_tag: String,
    pub to_tag: Option<String>,
    pub per_agent: Vec<AgentSlot>,
}

struct Bucket {
    call_id: String,
    /// Establishing orientation as known to THIS bucket (the pending bucket's
    /// single tag stays `from_tag` through migration). The slice recomputes
    /// orientation across all its buckets; these are the per-bucket seed.
    from_tag: String,
    to_tag: Option<String>,
    bind_key: LaneKey,
    ordered: Vec<OrderedEvent>,
}

/// Key for a confirmed dialog bucket: the **unordered** tag pair, so both
/// From/To orientations of one dialog map to one bucket.
fn confirmed_key(bind: &str, call_id: &str, tag_a: &str, tag_b: &str) -> String {
    let (lo, hi) = if tag_a <= tag_b { (tag_a, tag_b) } else { (tag_b, tag_a) };
    format!("{bind}\x00{call_id}\x00{lo}\x00{hi}")
}

/// Key for a pending (single-tag) bucket — messages recorded before the
/// callee's tag exists (INVITE, 100 Trying). Distinct from every confirmed
/// key: a confirmed key always carries two non-empty tags.
fn pending_key(bind: &str, call_id: &str, tag: &str) -> String {
    format!("{bind}\x00{call_id}\x00{tag}\x00")
}

/// Group the recorded channel into per-`(bind, Call-ID, unordered tag-pair)`
/// agent slots. One-tag messages sit in a pending bucket keyed by their single
/// tag and migrate into the pair-keyed bucket when the second tag is observed
/// (forked dialogs: the migration happens once, into the first pair observed;
/// later pairs sharing the caller tag start fresh — distinct slices). Slots
/// that share a `(Call-ID, tag-pair)` are gathered under one [`DialogSlice`]
/// whose tags are normalised to establishing orientation.
pub fn project_per_dialog(events: &[Stamped<SignalingNetworkEvent>]) -> Vec<DialogSlice> {
    let parser = super::lenient_parser();

    struct Entry {
        kind: EventKind,
        bind_key: LaneKey,
        at_ms: u64,
        seq: u64,
        msg: SipMessage,
        wire_peer: SocketAddr,
    }
    let mut ordered: Vec<Entry> = Vec::new();
    for s in events {
        match &s.event {
            SignalingNetworkEvent::SendCalled { bind_key, to, msg } => {
                if let Ok(m) = parser.parse(msg) {
                    ordered.push(Entry {
                        kind: EventKind::Sent,
                        bind_key: bind_key.clone(),
                        at_ms: s.at_ms,
                        seq: s.seq,
                        msg: m,
                        wire_peer: *to,
                    });
                }
            }
            SignalingNetworkEvent::RecvItem { bind_key, packet, .. } => {
                if let Ok(m) = parser.parse(&packet.raw) {
                    ordered.push(Entry {
                        kind: EventKind::Received,
                        bind_key: bind_key.clone(),
                        at_ms: s.at_ms,
                        seq: s.seq,
                        msg: m,
                        wire_peer: packet.src,
                    });
                }
            }
            _ => {}
        }
    }
    ordered.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then(a.seq.cmp(&b.seq)));

    let mut buckets: HashMap<String, Bucket> = HashMap::new();
    for (position, e) in ordered.into_iter().enumerate() {
        let cid = call_id(&e.msg);
        if cid.is_empty() {
            continue;
        }
        let ft = match from_tag(&e.msg) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => continue,
        };
        let tt = to_tag(&e.msg).map(str::to_string).filter(|t| !t.is_empty());

        // Resolve the bucket, migrating a pending (single-tag) bucket if this
        // is the first message to carry the dialog's second tag. The pending
        // bucket is normally keyed on the message's From-tag (same orientation
        // as the establishing INVITE), but a callee-initiated first two-tag
        // message carries the caller's tag in **To** — probe both.
        let key = if let Some(ref tag) = tt {
            let confirmed = confirmed_key(&e.bind_key, cid, &ft, tag);
            if !buckets.contains_key(&confirmed) {
                let migrated = buckets
                    .remove(&pending_key(&e.bind_key, cid, &ft))
                    .or_else(|| buckets.remove(&pending_key(&e.bind_key, cid, tag)));
                if let Some(mut b) = migrated {
                    // Establishing orientation: the pending bucket's single tag
                    // is the caller's; the OTHER tag of the pair is the callee's.
                    b.to_tag = Some(if b.from_tag == ft { tag.clone() } else { ft.clone() });
                    buckets.insert(confirmed.clone(), b);
                }
            }
            confirmed
        } else {
            pending_key(&e.bind_key, cid, &ft)
        };

        let bucket = buckets.entry(key).or_insert_with(|| Bucket {
            call_id: cid.to_string(),
            from_tag: ft.clone(),
            to_tag: tt.clone(),
            bind_key: e.bind_key.clone(),
            ordered: Vec::new(),
        });
        bucket.ordered.push(OrderedEvent {
            kind: e.kind,
            idx: position,
            msg: e.msg,
            wire_peer: Some(e.wire_peer),
        });
    }

    let mut slices: HashMap<String, DialogSlice> = HashMap::new();
    for b in buckets.into_values() {
        // Cross-bind grouping key: unordered pair for confirmed dialogs (both
        // orientations of one dialog gather under one slice), single tag for
        // pending ones.
        let k = match &b.to_tag {
            Some(t) => {
                let (lo, hi) = if b.from_tag.as_str() <= t.as_str() {
                    (b.from_tag.as_str(), t.as_str())
                } else {
                    (t.as_str(), b.from_tag.as_str())
                };
                format!("{}\x00{lo}\x00{hi}", b.call_id)
            }
            None => format!("{}\x00{}\x00", b.call_id, b.from_tag),
        };
        let slice = slices.entry(k).or_insert_with(|| DialogSlice {
            call_id: b.call_id.clone(),
            from_tag: b.from_tag.clone(),
            to_tag: b.to_tag.clone(),
            per_agent: Vec::new(),
        });
        let mut ordered = b.ordered;
        ordered.sort_by_key(|o| o.idx);
        slice.per_agent.push(AgentSlot {
            bind_key: b.bind_key,
            ordered,
        });
    }
    for slice in slices.values_mut() {
        normalise_slice_orientation(slice);
    }

    let mut out: Vec<DialogSlice> = slices.into_values().collect();
    out.sort_by(|a, b| {
        a.call_id
            .cmp(&b.call_id)
            .then(a.from_tag.cmp(&b.from_tag))
            .then(a.to_tag.cmp(&b.to_tag))
    });
    out
}

/// Report a confirmed slice's tag pair in **establishing orientation**:
/// `from_tag` = the caller's tag. The caller's tag is the one that appeared
/// with no partner tag — the dialog-establishing INVITE / 100 Trying carry
/// only the From-tag — identified by the earliest (global capture order)
/// single-tag event across all the slice's slots. When no single-tag message
/// was recorded (a slice observed only mid-dialog), the earliest event's own
/// From/To orientation stands. Also makes the seed orientation deterministic:
/// the slice was seeded from an arbitrary bucket (`HashMap` iteration order).
fn normalise_slice_orientation(slice: &mut DialogSlice) {
    let Some(cur_to) = slice.to_tag.clone() else {
        return; // pending slice: single tag, nothing to orient
    };
    let mut earliest_any: Option<(usize, String)> = None;
    let mut earliest_single: Option<(usize, String)> = None;
    for slot in &slice.per_agent {
        for ev in &slot.ordered {
            let ft = from_tag(&ev.msg).unwrap_or("");
            if ft.is_empty() {
                continue;
            }
            if earliest_any.as_ref().is_none_or(|(i, _)| ev.idx < *i) {
                earliest_any = Some((ev.idx, ft.to_string()));
            }
            let tt = to_tag(&ev.msg).unwrap_or("");
            if tt.is_empty() && earliest_single.as_ref().is_none_or(|(i, _)| ev.idx < *i) {
                earliest_single = Some((ev.idx, ft.to_string()));
            }
        }
    }
    let Some((_, caller)) = earliest_single.or(earliest_any) else {
        return;
    };
    if caller == cur_to && caller != slice.from_tag {
        slice.to_tag = Some(std::mem::replace(&mut slice.from_tag, cur_to));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_is_loose_detects_lr() {
        assert!(route_is_loose("<sip:p@host;lr>"));
        assert!(route_is_loose("<sip:p@host;lr;x=1>"));
        assert!(route_is_loose("<sip:p@host;lr>, <sip:q@h2>"));
        assert!(!route_is_loose("<sip:p@host>"));
        assert!(!route_is_loose("<sip:p@host;lrx>"));
    }

    #[test]
    fn extract_route_uri_unwraps_angle_brackets() {
        assert_eq!(extract_route_uri("<sip:p@host;lr>"), "sip:p@host;lr");
        assert_eq!(extract_route_uri("  sip:p@host  "), "sip:p@host");
    }

    #[test]
    fn parse_sdp_origin_lifts_o_line() {
        let body = b"v=0\r\no=alice 111 222 IN IP4 10.0.0.1\r\ns=-\r\nt=0 0\r\n";
        let o = parse_sdp_origin(body).expect("origin");
        assert_eq!(o.username, "alice");
        assert_eq!(o.session_id, "111");
        assert_eq!(o.session_version, 222);
        assert_eq!(o.unicast_address, "10.0.0.1");
        assert!(!o.body_digest_excluding_origin.contains("o="));
    }

    #[test]
    fn parse_sdp_origin_rejects_non_sdp() {
        assert!(parse_sdp_origin(b"").is_none());
        assert!(parse_sdp_origin(b"not sdp").is_none());
    }

    // ── project_per_dialog: unordered tag-pair keying (newkahneed-029) ──────

    use crate::types::UdpPacket;

    fn raw_req(method: &str, branch: &str, cseq: u32, ftag: &str, ttag: Option<&str>) -> Vec<u8> {
        let to = match ttag {
            Some(t) => format!("<sip:peer@127.0.0.1>;tag={t}"),
            None => "<sip:peer@127.0.0.1>".to_string(),
        };
        format!(
            "{method} sip:peer@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:orig@127.0.0.1>;tag={ftag}\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn raw_resp(status: u16, cseq: u32, method: &str, branch: &str, ftag: &str, ttag: Option<&str>) -> Vec<u8> {
        let to = match ttag {
            Some(t) => format!("<sip:peer@127.0.0.1>;tag={t}"),
            None => "<sip:peer@127.0.0.1>".to_string(),
        };
        format!(
            "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:orig@127.0.0.1>;tag={ftag}\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn sent(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::SendCalled {
                bind_key: bind.to_string(),
                to: "127.0.0.1:5070".parse().unwrap(),
                msg: raw,
            },
            seq,
            at_ms: seq,
        }
    }

    fn recv(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                disposition: crate::types::RecvDisposition::Delivered,
                packet: UdpPacket {
                    raw,
                    src: "127.0.0.1:5070".parse().unwrap(),
                    arrival_ms: seq,
                },
            },
            seq,
            at_ms: seq,
        }
    }

    #[test]
    fn reversed_mid_dialog_request_joins_the_establishing_slice() {
        // The UAS-side re-INVITE / BYE reverse From/To relative to the
        // establishing INVITE. The unordered-pair key must map both
        // orientations onto ONE dialog slice, reported in establishing
        // orientation (from = caller's tag "at").
        let evs = vec![
            recv("bob", raw_req("INVITE", "z9hG4bK-i", 1, "at", None), 0),
            sent("bob", raw_resp(200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt")), 1),
            recv("bob", raw_req("ACK", "z9hG4bK-k", 1, "at", Some("bt")), 2),
            // UAS-initiated re-INVITE toward the caller: tags reversed.
            sent("bob", raw_req("INVITE", "z9hG4bK-r", 1, "bt", Some("at")), 3),
            recv("bob", raw_resp(500, 1, "INVITE", "z9hG4bK-r", "bt", Some("at")), 4),
            sent("bob", raw_req("ACK", "z9hG4bK-r", 1, "bt", Some("at")), 5),
            sent("bob", raw_req("BYE", "z9hG4bK-b", 2, "bt", Some("at")), 6),
            recv("bob", raw_resp(200, 2, "BYE", "z9hG4bK-b", "bt", Some("at")), 7),
        ];
        let slices = project_per_dialog(&evs);
        assert_eq!(slices.len(), 1, "one dialog, one slice: {slices:?}");
        let s = &slices[0];
        assert_eq!(s.from_tag, "at", "establishing orientation: caller's tag first");
        assert_eq!(s.to_tag.as_deref(), Some("bt"));
        assert_eq!(s.per_agent.len(), 1);
        assert_eq!(s.per_agent[0].ordered.len(), 8, "all 8 events in the one slot");
    }

    #[test]
    fn one_tag_events_migrate_into_the_pair_keyed_bucket() {
        // INVITE and 100 Trying exist before the callee's tag does — they sit
        // in the single-tag pending bucket and must migrate into the pair
        // bucket when the tagged 180/200 arrives.
        let evs = vec![
            sent("alice", raw_req("INVITE", "z9hG4bK-i", 1, "at", None), 0),
            recv("alice", raw_resp(100, 1, "INVITE", "z9hG4bK-i", "at", None), 1),
            recv("alice", raw_resp(180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt")), 2),
            recv("alice", raw_resp(200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt")), 3),
        ];
        let slices = project_per_dialog(&evs);
        assert_eq!(slices.len(), 1, "{slices:?}");
        assert_eq!(slices[0].from_tag, "at");
        assert_eq!(slices[0].to_tag.as_deref(), Some("bt"));
        assert_eq!(slices[0].per_agent[0].ordered.len(), 4, "INVITE + 100 migrated");
    }

    #[test]
    fn forked_to_tags_stay_distinct_slices() {
        // Two callee tags forked off one INVITE are two dialogs (two distinct
        // tag pairs). The pending bucket migrates into the FIRST pair; the
        // second fork starts its own slice.
        let evs = vec![
            sent("alice", raw_req("INVITE", "z9hG4bK-i", 1, "at", None), 0),
            recv("alice", raw_resp(180, 1, "INVITE", "z9hG4bK-i", "at", Some("b1")), 1),
            recv("alice", raw_resp(200, 1, "INVITE", "z9hG4bK-i", "at", Some("b2")), 2),
        ];
        let slices = project_per_dialog(&evs);
        assert_eq!(slices.len(), 2, "{slices:?}");
        let fork1 = slices.iter().find(|s| s.to_tag.as_deref() == Some("b1")).unwrap();
        let fork2 = slices.iter().find(|s| s.to_tag.as_deref() == Some("b2")).unwrap();
        assert_eq!(fork1.from_tag, "at");
        assert_eq!(fork2.from_tag, "at");
        assert_eq!(fork1.per_agent[0].ordered.len(), 2, "INVITE migrated with the first fork");
        assert_eq!(fork2.per_agent[0].ordered.len(), 1);
    }

    #[test]
    fn endpoint_receiving_reversed_reinvite_is_not_a_relay() {
        // The caller sent the establishing INVITE and later RECEIVED the
        // UAS-initiated re-INVITE (both tags). Now that both live in one slot,
        // only the To-tag-less INVITE may count toward relay detection — the
        // endpoint must not be misclassified.
        let evs = vec![
            sent("alice", raw_req("INVITE", "z9hG4bK-i", 1, "at", None), 0),
            recv("alice", raw_resp(200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt")), 1),
            recv("alice", raw_req("INVITE", "z9hG4bK-r", 1, "bt", Some("at")), 2),
        ];
        let slices = project_per_dialog(&evs);
        assert_eq!(slices.len(), 1, "{slices:?}");
        assert!(!slot_is_relay(&slices[0].per_agent[0]), "endpoint misread as relay");
    }

    #[test]
    fn transparent_relay_is_still_a_relay() {
        // A relay forwards the establishing INVITE in both directions — the
        // initial-INVITE-only gate must keep classifying it as a relay.
        let evs = vec![
            recv("lb", raw_req("INVITE", "z9hG4bK-i", 1, "at", None), 0),
            sent("lb", raw_req("INVITE", "z9hG4bK-i2", 1, "at", None), 1),
        ];
        let slices = project_per_dialog(&evs);
        assert_eq!(slices.len(), 1, "{slices:?}");
        assert!(slot_is_relay(&slices[0].per_agent[0]));
    }
}
