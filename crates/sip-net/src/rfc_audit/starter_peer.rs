//! Port of `tests/harness/rules/rfc/starter-peer-rules.ts` — generic
//! per-message peer validators. Each [`PeerAuditRule`] sees one bind's recorded
//! events (sent + received) and returns violation detail strings; the gate /
//! ledger attaches the bind + severity.
//!
//! Authoring pattern (copy this for the remaining starter rules): a unit struct
//! implementing [`PeerAuditRule`], a `name()` matching the TS `rfc.<x>` rule, a
//! `subject()` (default = all roles), and a `check()` that lenient-parses the
//! relevant direction's messages via [`CustomParser`] and flags violations.
//! Add the struct to [`peer_rules`].
//!
//! The TypeScript original runs every check by replaying a per-Call-ID dialog
//! state (`trackSent` for what the bind sent, `trackReceived` for what it got)
//! and then running one `runValidationChecks` filter over *received* messages.
//! Here each rule is its own struct. A rule whose subject the sender *mints* —
//! the presence/grammar of a header the originator writes — judges the bind's
//! **sent** messages (so the defect is attributed to its originator, per the
//! authoring spec). A rule that correlates a *received* message to the dialog
//! state this bind built (Via echo, response→request, CANCEL→INVITE, RAck→1xx,
//! in-dialog tags/URI/Call-ID/CSeq) replays this bind's ordered stream
//! [`replay`] and judges the received side, exactly as the TS validators do.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use layer_harness::Stamped;
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::contracts::{PeerAuditRule, SignalingNetworkEvent};
use crate::rfc_audit::dialog_model::{
    call_id, cseq_method, cseq_seq, from_tag, from_uri, msg_headers, to_tag, top_via_branch,
};
use crate::types::UaRole;

/// The RFC 3261 magic cookie every `Via` `branch=` MUST begin with (§8.1.1.7).
const MAGIC_COOKIE: &str = "z9hG4bK";

/// Iterate the messages this bind **sent** (`SendCalled`), lenient-parsed. The
/// sender mints the top-`Via` branch, so sent-direction is the right place to
/// attribute a branch/Via defect to its originator.
fn sent_messages<'a>(
    events: &'a [Stamped<SignalingNetworkEvent>],
    parser: &'a CustomParser,
) -> impl Iterator<Item = SipMessage> + 'a {
    events.iter().filter_map(move |s| match &s.event {
        SignalingNetworkEvent::SendCalled { msg, .. } => parser.parse(msg).ok(),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Ordered (sent + received) replay — the Rust port of the TS per-bind loop
// (`extractOrderedMessages` + `trackSent`/`trackReceived`/`correlateResponse`),
// partitioned per Call-ID so a bind that terminates many independent dialogs
// (a B2BUA worker socket carries two legs of every call with distinct
// Call-IDs) does not cross-contaminate dialog state.
// ---------------------------------------------------------------------------

/// One step of the merged (sent + received) timeline.
struct Step {
    sent: bool,
    msg: SipMessage,
}

/// Build the time-ordered, lenient-parsed (sent + received) timeline for one
/// bind. Mirrors the TS `extractOrderedMessages` (atMs, then seq).
fn ordered_steps(events: &[Stamped<SignalingNetworkEvent>], parser: &CustomParser) -> Vec<Step> {
    let mut idx: Vec<(u64, u64, bool, Vec<u8>)> = Vec::new();
    for s in events {
        match &s.event {
            SignalingNetworkEvent::SendCalled { msg, .. } => {
                idx.push((s.at_ms, s.seq, true, msg.clone()))
            }
            SignalingNetworkEvent::RecvItem { packet, .. } => {
                idx.push((s.at_ms, s.seq, false, packet.raw.clone()))
            }
            _ => {}
        }
    }
    idx.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    idx.into_iter()
        .filter_map(|(_, _, sent, raw)| parser.parse(&raw).ok().map(|msg| Step { sent, msg }))
        .collect()
}

/// The replayed per-Call-ID dialog state a bind builds as it sends and receives.
/// A faithful subset of the TS `AgentDialogState`: only the fields the ported
/// rules read.
#[derive(Default)]
struct PeerDialog {
    /// Tags this bind minted locally (From-tag on sent requests, To-tag on sent
    /// responses) — the set a received in-dialog message's tag must be drawn from.
    local_tags: HashSet<String>,
    /// `(method, cseq)` of every request this bind sent — for response→request
    /// correlation (`via`, `responseCorrelation`).
    sent_requests: Vec<(String, u32)>,
    /// CSeq numbers of INVITEs this bind received — for RAck correlation.
    received_invite_cseqs: Vec<u32>,
    /// Confirmed remote tag (set by the first received message carrying one).
    remote_tag: Option<String>,
    /// Request-URI and top-Via branch of the first received INVITE — for the
    /// CANCEL-matches-INVITE rules.
    received_invite_uri: Option<String>,
    received_invite_branch: Option<String>,
    /// From-URI established for the remote party (the initial received INVITE's
    /// From-URI) — for `dialogUri`.
    dialog_remote_uri: Option<String>,
    /// This bind both **sent** and **received** an INVITE for this Call-ID — it is
    /// a transparent relay (proxy) carrying both directions of one dialog, not a
    /// dialog endpoint. The per-bind UA-dialog rules (which assume received
    /// in-dialog traffic comes from one peer) MUST NOT judge a relay: it forwards
    /// requests from BOTH parties, so the "remote" From/tags legitimately alternate.
    sent_invite: bool,
    recv_invite: bool,
}

impl PeerDialog {
    /// A transparent relay for this dialog — see [`PeerDialog::sent_invite`].
    fn is_relay(&self) -> bool {
        self.sent_invite && self.recv_invite
    }
}

/// Apply the TS `trackSent`: record the From/To tag this bind minted and the
/// `(method, cseq)` of requests it sent.
fn track_sent(ds: &mut PeerDialog, msg: &SipMessage) {
    match msg {
        SipMessage::Request(req) => {
            ds.sent_requests.push((req.method.as_str().to_string(), req.cseq.seq));
            if req.method.as_str() == "INVITE" {
                ds.sent_invite = true;
            }
            if let Some(t) = from_tag(msg) {
                ds.local_tags.insert(t.to_string());
            }
        }
        SipMessage::Response(_) => {
            if let Some(t) = to_tag(msg) {
                ds.local_tags.insert(t.to_string());
            }
        }
    }
}

/// Apply the TS `trackReceived`: learn the remote tag, the received-INVITE
/// identity (URI / branch / remote From-URI), and the received-INVITE CSeqs.
fn track_received(ds: &mut PeerDialog, msg: &SipMessage) {
    if let SipMessage::Request(req) = msg {
        if req.method.as_str() == "INVITE" {
            ds.recv_invite = true;
            if ds.received_invite_uri.is_none() {
                ds.received_invite_uri = Some(req.uri.clone());
            }
            if ds.received_invite_branch.is_none() {
                ds.received_invite_branch = top_via_branch(msg);
            }
            if ds.dialog_remote_uri.is_none() {
                ds.dialog_remote_uri = Some(from_uri(msg).to_string());
            }
            ds.received_invite_cseqs.push(req.cseq.seq);
        }
    }
    let tag = match msg {
        SipMessage::Response(_) => to_tag(msg),
        SipMessage::Request(_) => from_tag(msg),
    };
    if let Some(t) = tag {
        if ds.remote_tag.is_none() {
            ds.remote_tag = Some(t.to_string());
        }
    }
}

/// Find the sent request a received response correlates to by `(cseq, method)`
/// — the TS `correlateResponse` (most-recent match).
fn correlated_request<'a>(resp: &SipMessage, ds: &'a PeerDialog) -> Option<&'a (String, u32)> {
    let seq = cseq_seq(resp);
    let method = cseq_method(resp);
    ds.sent_requests.iter().rev().find(|(m, s)| *s == seq && m.as_str() == method)
}

/// Run a per-Call-ID replay over the bind's ordered stream, invoking `on_recv`
/// for every received message with the dialog state built from everything
/// (sent + received) that preceded it. `on_recv` returns any violation strings.
/// Mirrors the TS loop body shape (`trackSent` on sent, run-check then
/// `trackReceived` on received).
fn replay<F>(events: &[Stamped<SignalingNetworkEvent>], mut on_recv: F) -> Vec<String>
where
    F: FnMut(&Step, &PeerDialog) -> Vec<String>,
{
    let parser = super::lenient_parser();
    let mut by_call: HashMap<String, PeerDialog> = HashMap::new();
    let mut out = Vec::new();
    for step in ordered_steps(events, &parser) {
        let cid = call_id(&step.msg);
        if cid.is_empty() {
            continue;
        }
        let ds = by_call.entry(cid.to_string()).or_default();
        if step.sent {
            track_sent(ds, &step.msg);
            continue;
        }
        // A transparent relay (proxy) carrying both directions of this dialog is
        // not a UA endpoint: the per-bind dialog rules would false-positive on the
        // legitimately-alternating From/tags. Still track state, but do not judge.
        if !ds.is_relay() {
            out.extend(on_recv(&step, ds));
        }
        track_received(ds, &step.msg);
    }
    out
}

/// **RFC 3261 §8.1.1.7 — the `Via` `branch` MUST begin with `z9hG4bK`.** Every
/// SIP/2.0-compliant client transaction prefixes its branch with the magic
/// cookie so a downstream element can tell an RFC-3261 branch from a legacy
/// (RFC 2543) one. A bind that emits a request whose top `Via` branch lacks the
/// cookie is non-compliant — a real downstream UAS/proxy may fail to match the
/// transaction.
pub struct BranchPrefixRule;

impl PeerAuditRule for BranchPrefixRule {
    fn name(&self) -> &'static str {
        "rfc3261.branchPrefix"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            // Only requests mint a branch; responses copy the request's verbatim.
            let SipMessage::Request(req) = &msg else {
                continue;
            };
            match top_via_branch(&msg) {
                Some(branch) if branch.starts_with(MAGIC_COOKIE) => {}
                Some(branch) => out.push(format!(
                    "{} top Via branch \"{branch}\" does not begin with the RFC 3261 magic \
                     cookie \"{MAGIC_COOKIE}\" (§8.1.1.7) — a downstream element cannot treat it \
                     as an RFC-3261 transaction id",
                    req.method.as_str(),
                )),
                None => out.push(format!(
                    "{} has no top Via branch parameter (RFC 3261 §8.1.1.7 requires a \
                     \"{MAGIC_COOKIE}\"-prefixed branch on every request)",
                    req.method.as_str(),
                )),
            }
        }
        out
    }
}

/// **RFC 3261 §8.1.1.6 — every request MUST carry Max-Forwards.** The header
/// caps the hop count so a routing loop self-terminates; a request without it,
/// or with a value outside `0..=255`, is malformed, and a value above 70 (the
/// recommended initial value) means a sender minted an out-of-range count rather
/// than the proxy/B2BUA decrement that legitimately lowers it. The sender mints
/// this header, so the bind's **sent** requests are judged.
pub struct MaxForwardsRule;

impl PeerAuditRule for MaxForwardsRule {
    fn name(&self) -> &'static str {
        "rfc3261.maxForwards"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            let SipMessage::Request(req) = &msg else {
                continue;
            };
            let method = req.method.as_str();
            match get_header(msg_headers(&msg), "max-forwards") {
                None => out.push(format!(
                    "{method} request is missing Max-Forwards — RFC 3261 §8.1.1.6 requires it on \
                     every request (a real downstream element cannot loop-protect this hop)"
                )),
                Some(raw) => match raw.trim().parse::<i64>() {
                    Ok(v) if (0..=255).contains(&v) => {
                        if v > 70 {
                            out.push(format!(
                                "{method} Max-Forwards is {v}, exceeds 70 — RFC 3261 §8.1.1.6 \
                                 (70 is the recommended initial value; a higher count was minted, \
                                 not decremented)"
                            ));
                        }
                    }
                    _ => out.push(format!(
                        "{method} has an invalid Max-Forwards value \"{raw}\" — RFC 3261 §8.1.1.6 \
                         requires an integer in 0..=255"
                    )),
                },
            }
        }
        out
    }
}

/// **RFC 3261 §20.14 — Content-Length MUST equal the body byte count.** A
/// declared length that disagrees with the bytes on the wire desyncs a stream
/// (TCP) or truncates a body (UDP) for a strict peer. The raw header/body split
/// is checked (not the parsed body, which is already sliced to the declared
/// length). The sender writes Content-Length, so the bind's **sent** messages
/// are judged.
pub struct ContentLengthRule;

impl ContentLengthRule {
    /// Mirror of the TS `checkRawContentLength`: compare the declared
    /// Content-Length against the actual post-`\r\n\r\n` (or `\n\n`) body length.
    fn check_raw(raw: &[u8]) -> Option<String> {
        let text = String::from_utf8_lossy(raw);
        let (sep, sep_len) = match text.find("\r\n\r\n") {
            Some(i) => (i, 4),
            None => (text.find("\n\n")?, 2),
        };
        let header_block = &text[..sep];
        let body = &text[sep + sep_len..];
        let declared = header_block.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })?;
        let actual = body.len();
        if declared != actual {
            Some(format!(
                "Content-Length mismatch: header says {declared} but body is {actual} bytes — \
                 RFC 3261 §20.14 (a strict peer truncates or desyncs on this)"
            ))
        } else {
            None
        }
    }
}

impl PeerAuditRule for ContentLengthRule {
    fn name(&self) -> &'static str {
        "rfc3261.contentLength"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        events
            .iter()
            .filter_map(|s| match &s.event {
                SignalingNetworkEvent::SendCalled { msg, .. } => Self::check_raw(msg),
                _ => None,
            })
            .collect()
    }
}

/// **RFC 3261 §7.4.1 — a body REQUIRES a Content-Type.** A message that carries
/// a body but no Content-Type leaves the peer unable to interpret it (SDP? DTMF?
/// ISUP?). The sender writes both, so the bind's **sent** messages are judged.
pub struct ContentTypeRule;

impl PeerAuditRule for ContentTypeRule {
    fn name(&self) -> &'static str {
        "rfc3261.contentType"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            let body = match &msg {
                SipMessage::Request(r) => &r.body,
                SipMessage::Response(r) => &r.body,
            };
            if body.is_empty() {
                continue;
            }
            if get_header(msg_headers(&msg), "content-type").is_none() {
                out.push(
                    "message carries a body but no Content-Type — RFC 3261 §7.4.1 requires \
                     Content-Type whenever a body is present (the peer cannot interpret the body)"
                        .to_string(),
                );
            }
        }
        out
    }
}

/// **RFC 3261 §8.1.1.8 — INVITE/SUBSCRIBE MUST carry Contact.** A
/// dialog-establishing request without a Contact gives the peer no target for
/// subsequent in-dialog requests (the dialog's remote target). The sender mints
/// Contact, so the bind's **sent** requests are judged.
pub struct ContactPresenceRule;

impl PeerAuditRule for ContactPresenceRule {
    fn name(&self) -> &'static str {
        "rfc3261.contactPresence"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            let SipMessage::Request(req) = &msg else {
                continue;
            };
            let method = req.method.as_str();
            if method != "INVITE" && method != "SUBSCRIBE" {
                continue;
            }
            if get_header(msg_headers(&msg), "contact").is_none() {
                out.push(format!(
                    "{method} request is missing Contact — RFC 3261 §8.1.1.8 requires it on \
                     dialog-establishing methods (the dialog's remote target is undefined)"
                ));
            }
        }
        out
    }
}

/// **RFC 3261 §15.1 — BYE MUST NOT carry Contact.** BYE terminates the dialog,
/// so target-refresh has no meaning; a Contact on BYE is a lazy builder that
/// pasted Contact on every method. The sender mints it, so the bind's **sent**
/// requests are judged.
pub struct NoContactOnByeRule;

impl PeerAuditRule for NoContactOnByeRule {
    fn name(&self) -> &'static str {
        "rfc3261.noContactOnBye"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            let SipMessage::Request(req) = &msg else {
                continue;
            };
            if req.method.as_str() != "BYE" {
                continue;
            }
            if get_header(msg_headers(&msg), "contact").is_some() {
                out.push(
                    "BYE carries Contact — RFC 3261 §15.1 (BYE terminates the dialog, so \
                     target-refresh has no meaning)"
                        .to_string(),
                );
            }
        }
        out
    }
}

/// **RFC 3261 §8.2.6.2 — a UAS MUST add a To-tag to every response > 100.** The
/// To-tag completes the dialog identifier; only `100 Trying` is exempt. A UAS
/// that ships a 180/200/4xx without one cannot be dialog-matched by the peer.
/// The responding UAS mints the To-tag, so the bind's **sent** responses are
/// judged.
///
/// Scanned over the **raw** sent bytes (not the parsed message): the custom
/// parser unconditionally rejects a `>100` response that lacks a To-tag (it is a
/// parse invariant, not merely a `wire_grammar` gate), so a parsed view would
/// never carry the defect — the raw scan is the only path that lets this rule
/// observe the violation a non-conformant UAS put on the wire.
pub struct ToTagPresenceRule;

impl ToTagPresenceRule {
    /// `(status, has_to_tag)` for a raw SIP **response**, or `None` for a
    /// request / unparsable start line. A lightweight scan: the status line and
    /// the `To`/`t` header value, looking for a non-empty `tag=` parameter.
    fn scan(raw: &[u8]) -> Option<(u16, bool)> {
        let text = String::from_utf8_lossy(raw);
        let mut lines = text.split('\n').map(|l| l.trim_end_matches('\r'));
        let start = lines.next()?;
        let rest = start.strip_prefix("SIP/2.0 ")?; // responses only
        let status: u16 = rest.split_whitespace().next()?.parse().ok()?;
        let mut has_tag = false;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let n = name.trim();
            if n.eq_ignore_ascii_case("to") || n.eq_ignore_ascii_case("t") {
                has_tag = sip_message::message_helpers::extract_tag(value.trim())
                    .is_some_and(|t| !t.is_empty());
                break;
            }
        }
        Some((status, has_tag))
    }
}

impl PeerAuditRule for ToTagPresenceRule {
    fn name(&self) -> &'static str {
        "rfc3261.toTagPresence"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let mut out = Vec::new();
        for s in events {
            let SignalingNetworkEvent::SendCalled { msg, .. } = &s.event else {
                continue;
            };
            let Some((status, has_tag)) = Self::scan(msg) else {
                continue;
            };
            if status > 100 && !has_tag {
                out.push(format!(
                    "{status} response is missing a To-tag — RFC 3261 §8.2.6.2 requires a UAS to \
                     add a To-tag to every response above 100 (the peer cannot dialog-match it)"
                ));
            }
        }
        out
    }
}

/// **RFC 3261 §16.6 — a B2BUA (a UA) MUST NOT Record-Route.** Record-Route is a
/// *proxy* mechanism: a UA that inserts it (here detected by the B2BUA markers
/// `callRef=` / `leg=` it stamps into the URI) is masquerading as a proxy in the
/// route set, corrupting the peer's dialog routing. Subject is narrowed to
/// `{Proxy}` per the TS — only a bind that may be mistaken for a proxy is judged
/// — and the **sent** requests carry the offending header.
pub struct RecordRouteRule;

impl PeerAuditRule for RecordRouteRule {
    fn name(&self) -> &'static str {
        "rfc3261.recordRoute"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Proxy])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            let SipMessage::Request(_) = &msg else {
                continue;
            };
            for rr in sip_message::message_helpers::get_headers(msg_headers(&msg), "record-route") {
                if rr.contains("callRef=") || rr.contains("leg=") {
                    out.push(format!(
                        "B2BUA inserted Record-Route in a request — a B2BUA is a UA and MUST NOT \
                         use Record-Route (RFC 3261 §16.6). Found: {rr}"
                    ));
                    break;
                }
            }
        }
        out
    }
}

/// **RFC 3261 §8.1.3 / §17.1.3 — a response's top Via MUST echo the request's.**
/// A UAC matches a response to its client transaction by the topmost `Via`
/// branch and copies the entire `Via` stack back unchanged; a response whose top
/// branch differs from the request this bind sent, or whose Via *count* differs,
/// cannot be correlated. Judged on **received** responses against this bind's
/// sent requests.
pub struct ViaRule;

impl PeerAuditRule for ViaRule {
    fn name(&self) -> &'static str {
        "rfc3261.via"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        // The Via echo check needs the *full* sent request, not just (method,
        // cseq); replay separately keeping the sent requests' Via stacks.
        let parser = super::lenient_parser();
        let mut by_call: HashMap<String, Vec<(u32, String, Vec<String>)>> = HashMap::new();
        let mut out = Vec::new();
        for step in ordered_steps(events, &parser) {
            let cid = call_id(&step.msg);
            if cid.is_empty() {
                continue;
            }
            if step.sent {
                if let SipMessage::Request(req) = &step.msg {
                    let vias: Vec<String> = sip_message::message_helpers::get_headers(
                        msg_headers(&step.msg),
                        "via",
                    )
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                    by_call.entry(cid.to_string()).or_default().push((
                        req.cseq.seq,
                        req.cseq.method.as_str().to_string(),
                        vias,
                    ));
                }
                continue;
            }
            let SipMessage::Response(_) = &step.msg else {
                continue;
            };
            let seq = cseq_seq(&step.msg);
            let method = cseq_method(&step.msg);
            let Some(sent) = by_call
                .get(cid)
                .and_then(|v| v.iter().rev().find(|(s, m, _)| *s == seq && m.as_str() == method))
            else {
                continue; // un-correlated response — cannot judge
            };
            let resp_vias = sip_message::message_helpers::get_headers(msg_headers(&step.msg), "via");
            let resp_branch = top_via_branch(&step.msg);
            let sent_branch = sent.2.first().and_then(|v| {
                sip_message::message_helpers::parse_via_params(v).branch.filter(|b| !b.is_empty())
            });
            if let (Some(rb), Some(sb)) = (&resp_branch, &sent_branch) {
                if rb != sb {
                    out.push(format!(
                        "response top Via branch \"{rb}\" differs from the branch this bind sent \
                         \"{sb}\" — RFC 3261 §8.1.3 (the response cannot be matched to its client \
                         transaction)"
                    ));
                }
            }
            if resp_vias.len() != sent.2.len() {
                out.push(format!(
                    "response carries {} Via header(s) but the sent request had {} — RFC 3261 \
                     §8.1.3 requires the response to echo the request's Via stack unchanged",
                    resp_vias.len(),
                    sent.2.len(),
                ));
            }
        }
        out
    }
}

/// **RFC 3261 §8.1.3.3 — a response's CSeq MUST echo a request we sent.** If a
/// received response carries a CSeq+method that no request this bind ever sent
/// produced, the peer is replying to a phantom — flagged only when the bind
/// *did* send at least one request of that method (otherwise the CSeq-method
/// mismatch is its own concern). Judged on **received** responses.
pub struct ResponseCorrelationRule;

impl PeerAuditRule for ResponseCorrelationRule {
    fn name(&self) -> &'static str {
        "rfc3261.responseCorrelation"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        replay(events, |step, ds| {
            let SipMessage::Response(_) = &step.msg else {
                return Vec::new();
            };
            if correlated_request(&step.msg, ds).is_some() {
                return Vec::new();
            }
            let seq = cseq_seq(&step.msg);
            let method = cseq_method(&step.msg);
            let same: Vec<u32> = ds
                .sent_requests
                .iter()
                .filter(|(m, _)| m.as_str() == method)
                .map(|(_, s)| *s)
                .collect();
            if same.is_empty() {
                return Vec::new();
            }
            let nums =
                same.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
            vec![format!(
                "response CSeq {seq} {method} does not echo any sent {method} CSeq [{nums}] — \
                 RFC 3261 §8.1.3.3 (the peer is responding to a phantom request)"
            )]
        })
    }
}

/// **RFC 3261 §12.2.1.1 — in-dialog tags identify the dialog.** A received
/// in-dialog request's To-tag must be one of the tags this bind minted locally,
/// and a received response's From-tag likewise — otherwise the message belongs
/// to a different dialog (a real UA 481s it). Judged on **received** messages
/// once a remote tag confirms the dialog. (Complementary to the cross-message
/// CSeq family — this is the per-bind tag-identity face of §12.)
pub struct TagsRule;

impl PeerAuditRule for TagsRule {
    fn name(&self) -> &'static str {
        "rfc3261.tags"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        replay(events, |step, ds| {
            match &step.msg {
                SipMessage::Request(_) => {
                    // Only judge once a dialog is confirmed (remote tag seen).
                    if ds.remote_tag.is_none() {
                        return Vec::new();
                    }
                    if let Some(tt) = to_tag(&step.msg) {
                        if !ds.local_tags.contains(tt) {
                            let expected =
                                ds.local_tags.iter().cloned().collect::<Vec<_>>().join(" | ");
                            return vec![format!(
                                "To-tag mismatch: in-dialog request carries To-tag \"{tt}\" but \
                                 this bind's local tags are [{expected}] — RFC 3261 §12.2.1.1"
                            )];
                        }
                    }
                }
                SipMessage::Response(_) => {
                    if let Some(ft) = from_tag(&step.msg) {
                        if !ds.local_tags.contains(ft) {
                            let expected =
                                ds.local_tags.iter().cloned().collect::<Vec<_>>().join(" | ");
                            return vec![format!(
                                "From-tag mismatch: response carries From-tag \"{ft}\" but this \
                                 bind's local tags are [{expected}] — RFC 3261 §12.2.1.1"
                            )];
                        }
                    }
                }
            }
            Vec::new()
        })
    }
}

/// **RFC 3261 §12.2.1.1 — the From URI of an in-dialog request is stable.** Once
/// a dialog is established, the remote party's From URI on subsequent in-dialog
/// requests must match the URI learned when the dialog was created (the first
/// received INVITE's From). A peer that rewrites it mid-dialog breaks dialog
/// matching. Judged on **received** requests once the dialog is confirmed.
pub struct DialogUriRule;

impl PeerAuditRule for DialogUriRule {
    fn name(&self) -> &'static str {
        "rfc3261.dialogUri"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        replay(events, |step, ds| {
            if ds.remote_tag.is_none() {
                return Vec::new();
            }
            let Some(remote_uri) = &ds.dialog_remote_uri else {
                return Vec::new();
            };
            let SipMessage::Request(_) = &step.msg else {
                return Vec::new();
            };
            let f = from_uri(&step.msg);
            if !f.is_empty() && f != remote_uri.as_str() {
                vec![format!(
                    "in-dialog From URI \"{f}\" differs from the dialog-established remote URI \
                     \"{remote_uri}\" — RFC 3261 §12.2.1.1"
                )]
            } else {
                Vec::new()
            }
        })
    }
}

/// **RFC 3261 §12.1 — Call-ID is immutable within a dialog.** Once this bind has
/// *confirmed* a Call-ID (the TS `callIdConfirmed`: set by the first received
/// INVITE — a B-side agent's locally-generated Call-ID is replaced then), every
/// later received message in that dialog MUST carry it; a changed Call-ID means
/// the peer mid-dialog re-identified the dialog, which a real UA cannot match.
///
/// Keyed by `(receiving bind, From-tag)` rather than the per-Call-ID partition
/// the other rules use — partitioning on Call-ID would make a Call-ID *change*
/// invisible (it would simply open a second partition), so this rule walks the
/// stream un-partitioned and watches one confirmed Call-ID per From-tag. Judged
/// on **received** messages.
pub struct CallIdRule;

impl PeerAuditRule for CallIdRule {
    fn name(&self) -> &'static str {
        "rfc3261.callId"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        // From-tag -> the Call-ID of the dialog that From-tag's **dialog-creating
        // INVITE** established. Seeding ONLY on a received dialog-creating INVITE
        // (method INVITE, no To-tag yet) is what keeps the rule from conflating
        // independent out-of-dialog transactions that merely reuse a From-tag/
        // To-tag — OPTIONS health probes against a responder that mints a constant
        // To-tag are the canonical false-positive (each probe is its own Call-ID,
        // none establishes a dialog, so none seeds). A later in-dialog message
        // (both tags) on that From-tag MUST reuse the dialog's Call-ID (§12.1).
        let mut dialog_call_id: HashMap<String, String> = HashMap::new();
        let mut out = Vec::new();
        for step in ordered_steps(events, &parser) {
            if step.sent {
                continue;
            }
            let Some(ft) = from_tag(&step.msg).map(str::to_string) else {
                continue;
            };
            let cid = call_id(&step.msg);
            let is_dialog_creating_invite = matches!(&step.msg, SipMessage::Request(r)
                if r.method.as_str() == "INVITE") && to_tag(&step.msg).is_none();
            match dialog_call_id.get(&ft) {
                Some(known) if known.as_str() != cid && to_tag(&step.msg).is_some() => {
                    out.push(format!(
                        "received in-dialog message Call-ID \"{cid}\" differs from the dialog's \
                         confirmed Call-ID \"{known}\" — RFC 3261 §12.1 (Call-ID is immutable \
                         within a dialog)"
                    ))
                }
                Some(_) => {}
                None if is_dialog_creating_invite => {
                    dialog_call_id.insert(ft, cid.to_string());
                }
                None => {}
            }
        }
        out
    }
}

/// **RFC 3261 §9.1 — CANCEL Request-URI MUST equal the INVITE's.** A CANCEL must
/// target exactly the request it cancels; a Request-URI differing from the
/// INVITE this bind received means the CANCEL cannot be matched to the INVITE
/// server transaction. Judged on **received** CANCELs against the received
/// INVITE.
pub struct CancelRequestUriRule;

impl PeerAuditRule for CancelRequestUriRule {
    fn name(&self) -> &'static str {
        "rfc3261.cancelRequestUri"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        replay(events, |step, ds| {
            let SipMessage::Request(req) = &step.msg else {
                return Vec::new();
            };
            if req.method.as_str() != "CANCEL" {
                return Vec::new();
            }
            let Some(invite_uri) = &ds.received_invite_uri else {
                return Vec::new(); // never saw the INVITE — cannot judge
            };
            if &req.uri != invite_uri {
                vec![format!(
                    "CANCEL Request-URI \"{}\" differs from the INVITE Request-URI \"{invite_uri}\" \
                     — RFC 3261 §9.1 (the CANCEL cannot match the INVITE server transaction)",
                    req.uri,
                )]
            } else {
                Vec::new()
            }
        })
    }
}

/// **RFC 3261 §9.1 — CANCEL top Via branch MUST equal the INVITE's.** The CANCEL
/// carries a single Via whose branch matches the top Via branch of the INVITE it
/// cancels, so the cancel is routed to the same server transaction. A differing
/// branch orphans the CANCEL. Judged on **received** CANCELs against the
/// received INVITE.
pub struct CancelViaBranchRule;

impl PeerAuditRule for CancelViaBranchRule {
    fn name(&self) -> &'static str {
        "rfc3261.cancelViaBranch"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        replay(events, |step, ds| {
            let SipMessage::Request(req) = &step.msg else {
                return Vec::new();
            };
            if req.method.as_str() != "CANCEL" {
                return Vec::new();
            }
            let Some(invite_branch) = &ds.received_invite_branch else {
                return Vec::new();
            };
            let Some(cancel_branch) = top_via_branch(&step.msg) else {
                return Vec::new();
            };
            if &cancel_branch != invite_branch {
                vec![format!(
                    "CANCEL top Via branch \"{cancel_branch}\" differs from the INVITE Via branch \
                     \"{invite_branch}\" — RFC 3261 §9.1 (the CANCEL is routed to a different \
                     server transaction)"
                )]
            } else {
                Vec::new()
            }
        })
    }
}

/// **RFC 3262 §7.2 — a PRACK's RAck CSeq MUST reference an outstanding reliable
/// 1xx's request.** The RAck `CSeq method` component identifies the INVITE/
/// re-INVITE whose reliable provisional is being acknowledged; if this bind
/// never received a request of that method+CSeq, the PRACK matches no reliable
/// transaction. Judged on **received** PRACKs against the INVITEs this bind
/// received.
pub struct RackCorrelationRule;

impl PeerAuditRule for RackCorrelationRule {
    fn name(&self) -> &'static str {
        "rfc3262.rackCorrelation"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        replay(events, |step, ds| {
            let SipMessage::Request(req) = &step.msg else {
                return Vec::new();
            };
            if req.method.as_str() != "PRACK" {
                return Vec::new();
            }
            let rack = match &req.optional.rack {
                Ok(Some(r)) => r,
                Ok(None) => return Vec::new(),
                Err(e) => return vec![e.to_string()],
            };
            // The TS only correlates against received INVITEs (the request WE
            // received in our UAS role that produced the reliable 1xx).
            if rack.method.as_str() != "INVITE" {
                return Vec::new();
            }
            if ds.received_invite_cseqs.contains(&(rack.seq as u32)) {
                return Vec::new();
            }
            let seen = if ds.received_invite_cseqs.is_empty() {
                "none".to_string()
            } else {
                ds.received_invite_cseqs
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            vec![format!(
                "RAck CSeq {} {} does not match any received INVITE CSeq [{seen}] — RFC 3262 §7.2 \
                 (the PRACK acknowledges no outstanding reliable 1xx)",
                rack.seq,
                rack.method.as_str(),
            )]
        })
    }
}

/// **RFC 3261 §17.2.1 / §12.1.1 — a UAS keeps its To-tag stable on a
/// transaction.** Once a UAS commits a To-tag on a provisional response (101–199)
/// it MUST carry that same tag on the final response (≥200) of the same client
/// transaction (top-Via branch). The To-tag identifies the dialog the early
/// (provisional) responses began; a final that mints a *fresh* tag orphans the
/// early dialog the UAC already created and a real UAC cannot reconcile the two.
/// Port of the TS `rfc.tagConsistency` (`validateTagConsistency`). Judged on
/// **sent** responses (the UAS mints the tag).
pub struct TagConsistencyRule;

impl PeerAuditRule for TagConsistencyRule {
    fn name(&self) -> &'static str {
        "rfc3261.tagConsistency"
    }

    /// **Advisory — B2BUA forking divergence.** A forking B2BUA legitimately
    /// relays one early dialog's provisional (tag A) upstream and then, when a
    /// *non-first* early dialog wins, sends that dialog's 2xx (tag B) on the same
    /// upstream INVITE transaction — with no prior provisional for tag B (RFC 3261
    /// §12.1.2 / §13.2.2.4 permits a 2xx that establishes a fresh dialog). Per the
    /// branch this looks like a UAS tag flip, but it is correct forking. The TS
    /// reference marks exactly these scenarios `skipValidation: ["tagConsistency"]`;
    /// we record the finding but do not gate on it. A genuine single-dialog UAS
    /// tag flip (non-forking) is still surfaced as advisory for review.
    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        // top-Via branch -> To-tags this bind committed on prior provisionals.
        let mut provisional_tags: HashMap<String, Vec<String>> = HashMap::new();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            let SipMessage::Response(resp) = &msg else {
                continue;
            };
            let (Some(branch), Some(tag)) =
                (top_via_branch(&msg), to_tag(&msg).map(str::to_string))
            else {
                continue;
            };
            if resp.status > 100 && resp.status < 200 {
                provisional_tags.entry(branch).or_default().push(tag);
            } else if resp.status >= 200 {
                if let Some(priors) = provisional_tags.get(&branch) {
                    if !priors.is_empty() && !priors.contains(&tag) {
                        let mut seen: Vec<&str> = Vec::new();
                        for p in priors {
                            if !seen.contains(&p.as_str()) {
                                seen.push(p);
                            }
                        }
                        out.push(format!(
                            "UAS To-tag mismatch on {} (branch {branch}): prior provisional(s) \
                             established tag(s) [{}] but the final carries \"{tag}\" — RFC 3261 \
                             §17.2.1 / §12.1.1",
                            resp.status,
                            seen.join(", "),
                        ));
                    }
                }
            }
        }
        out
    }
}

/// The peer rules defined in this module. Aggregated by [`super::rfc_peer_rules`].
pub(crate) fn peer_rules() -> Vec<Arc<dyn PeerAuditRule>> {
    vec![
        Arc::new(BranchPrefixRule),
        Arc::new(TagConsistencyRule),
        Arc::new(MaxForwardsRule),
        Arc::new(ContentLengthRule),
        Arc::new(ContentTypeRule),
        Arc::new(ContactPresenceRule),
        Arc::new(NoContactOnByeRule),
        Arc::new(ToTagPresenceRule),
        Arc::new(RecordRouteRule),
        Arc::new(ViaRule),
        Arc::new(ResponseCorrelationRule),
        Arc::new(TagsRule),
        Arc::new(DialogUriRule),
        Arc::new(CallIdRule),
        Arc::new(CancelRequestUriRule),
        Arc::new(CancelViaBranchRule),
        Arc::new(RackCorrelationRule),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    fn invite(branch: &str) -> Vec<u8> {
        format!(
            "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn sent_at(bind: &str, raw: Vec<u8>, to: &str, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::SendCalled {
                bind_key: bind.to_string(),
                to: to.parse().unwrap(),
                msg: raw,
            },
            seq,
            at_ms: seq,
        }
    }

    // A received message must NOT be re-flagged by the sender-direction rule
    // (avoids double-attribution); only what the bind itself sent is judged.
    fn recv_at(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                packet: UdpPacket { raw, src: "127.0.0.1:9999".parse().unwrap(), arrival_ms: seq },
            },
            seq,
            at_ms: seq,
        }
    }

    #[test]
    fn compliant_branch_is_clean() {
        let evs = vec![sent_at("alice", invite("z9hG4bK-abc123"), "127.0.0.1:5080", 0)];
        assert!(BranchPrefixRule.check(&evs, "alice").is_empty());
    }

    fn invite_resp(status: u16, branch: &str, to_tag: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5070;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag={to_tag}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:bob@127.0.0.1:5070>\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn stable_to_tag_across_provisional_and_final_is_clean() {
        // 180 then 200 on the same branch carry the SAME To-tag — clean.
        let evs = vec![
            sent_at("bob", invite_resp(180, "z9hG4bK-i", "bt"), "127.0.0.1:5060", 0),
            sent_at("bob", invite_resp(200, "z9hG4bK-i", "bt"), "127.0.0.1:5060", 1),
        ];
        assert!(TagConsistencyRule.check(&evs, "bob").is_empty());
    }

    #[test]
    fn final_minting_a_fresh_to_tag_is_flagged() {
        // 180 establishes tag "bt"; the 200 mints "bt2" on the same branch — a
        // UAS tag flip that orphans the early dialog.
        let evs = vec![
            sent_at("bob", invite_resp(180, "z9hG4bK-i", "bt"), "127.0.0.1:5060", 0),
            sent_at("bob", invite_resp(200, "z9hG4bK-i", "bt2"), "127.0.0.1:5060", 1),
        ];
        let f = TagConsistencyRule.check(&evs, "bob");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("To-tag mismatch"), "{}", f[0]);
    }

    #[test]
    fn missing_cookie_is_flagged() {
        let evs = vec![sent_at("alice", invite("legacy-2543-branch"), "127.0.0.1:5080", 0)];
        let f = BranchPrefixRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("magic cookie"), "{}", f[0]);
    }

    #[test]
    fn received_message_is_not_judged_by_sender_rule() {
        // The bad branch arrives (received), but this bind did not send it.
        let evs = vec![recv_at("alice", invite("legacy"), 0)];
        assert!(BranchPrefixRule.check(&evs, "alice").is_empty());
    }

    // ── Byte-builders shared by the remaining rule tests ────────────────────

    /// A request with caller-controlled method, CSeq, top-Via branch, To-tag,
    /// Max-Forwards, Contact, Request-URI, Content-Type, body and Call-ID. A
    /// `None` field is omitted from the wire bytes.
    #[allow(clippy::too_many_arguments)]
    fn build_req(
        method: &str,
        uri: &str,
        branch: &str,
        cseq: u32,
        from_tag: &str,
        to_tag: Option<&str>,
        max_forwards: Option<&str>,
        contact: Option<&str>,
        body: &str,
        content_type: Option<&str>,
        extra: &str,
        call_id: &str,
    ) -> Vec<u8> {
        let to = match to_tag {
            Some(t) => format!("<sip:bob@127.0.0.1>;tag={t}"),
            None => "<sip:bob@127.0.0.1>".to_string(),
        };
        let mut s = format!(
            "{method} {uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag={from_tag}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n"
        );
        if let Some(mf) = max_forwards {
            s.push_str(&format!("Max-Forwards: {mf}\r\n"));
        }
        if let Some(c) = contact {
            s.push_str(&format!("Contact: {c}\r\n"));
        }
        if let Some(ct) = content_type {
            s.push_str(&format!("Content-Type: {ct}\r\n"));
        }
        s.push_str(extra);
        s.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        s.into_bytes()
    }

    fn resp(
        status: u16,
        branch: &str,
        cseq: u32,
        method: &str,
        from_tag: &str,
        to_tag: Option<&str>,
        call_id: &str,
    ) -> Vec<u8> {
        let to = match to_tag {
            Some(t) => format!("<sip:bob@127.0.0.1>;tag={t}"),
            None => "<sip:bob@127.0.0.1>".to_string(),
        };
        format!(
            "SIP/2.0 {status} OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag={from_tag}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn sent(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        sent_at(bind, raw, "127.0.0.1:5080", seq)
    }

    // ── MaxForwardsRule ─────────────────────────────────────────────────────

    #[test]
    fn max_forwards_present_is_clean() {
        let r = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-mf",
        );
        assert!(MaxForwardsRule.check(&[sent("a", r, 0)], "a").is_empty());
    }

    #[test]
    fn missing_max_forwards_is_flagged() {
        let r = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 1, "at", None, None,
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-mf",
        );
        let f = MaxForwardsRule.check(&[sent("a", r, 0)], "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("missing Max-Forwards"), "{}", f[0]);
    }

    #[test]
    fn over_70_max_forwards_is_flagged() {
        let r = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 1, "at", None, Some("200"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-mf",
        );
        let f = MaxForwardsRule.check(&[sent("a", r, 0)], "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("exceeds 70"), "{}", f[0]);
    }

    // ── ContentLengthRule ───────────────────────────────────────────────────

    #[test]
    fn matching_content_length_is_clean() {
        let r = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "v=0\r\n", Some("application/sdp"), "", "cid-cl",
        );
        assert!(ContentLengthRule.check(&[sent("a", r, 0)], "a").is_empty());
    }

    #[test]
    fn wrong_content_length_is_flagged() {
        // Hand-build bytes with a deliberately wrong Content-Length.
        let raw = "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: cid-cl\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: 99\r\n\r\nv=0\r\n"
            .to_string()
            .into_bytes();
        let f = ContentLengthRule.check(&[sent("a", raw, 0)], "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("Content-Length mismatch"), "{}", f[0]);
    }

    // ── ContentTypeRule ─────────────────────────────────────────────────────

    #[test]
    fn body_with_content_type_is_clean() {
        let r = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "v=0\r\n", Some("application/sdp"), "", "cid-ct",
        );
        assert!(ContentTypeRule.check(&[sent("a", r, 0)], "a").is_empty());
    }

    #[test]
    fn body_without_content_type_is_flagged() {
        let r = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "v=0\r\n", None, "", "cid-ct",
        );
        let f = ContentTypeRule.check(&[sent("a", r, 0)], "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("no Content-Type"), "{}", f[0]);
    }

    // ── ContactPresenceRule ─────────────────────────────────────────────────

    #[test]
    fn invite_with_contact_is_clean() {
        let r = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-cp",
        );
        assert!(ContactPresenceRule.check(&[sent("a", r, 0)], "a").is_empty());
    }

    #[test]
    fn invite_without_contact_is_flagged() {
        let r = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 1, "at", None, Some("70"),
            None, "", None, "", "cid-cp",
        );
        let f = ContactPresenceRule.check(&[sent("a", r, 0)], "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("missing Contact"), "{}", f[0]);
    }

    // ── NoContactOnByeRule ──────────────────────────────────────────────────

    #[test]
    fn bye_without_contact_is_clean() {
        let r = build_req(
            "BYE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 2, "at", Some("bt"), Some("70"),
            None, "", None, "", "cid-bye",
        );
        assert!(NoContactOnByeRule.check(&[sent("a", r, 0)], "a").is_empty());
    }

    #[test]
    fn bye_with_contact_is_flagged() {
        let r = build_req(
            "BYE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 2, "at", Some("bt"), Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-bye",
        );
        let f = NoContactOnByeRule.check(&[sent("a", r, 0)], "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("BYE carries Contact"), "{}", f[0]);
    }

    // ── ToTagPresenceRule ───────────────────────────────────────────────────

    #[test]
    fn response_with_to_tag_is_clean() {
        let r = resp(200, "z9hG4bK-1", 1, "INVITE", "at", Some("bt"), "cid-tt");
        assert!(ToTagPresenceRule.check(&[sent("a", r, 0)], "a").is_empty());
    }

    #[test]
    fn provisional_100_without_to_tag_is_clean() {
        let r = resp(100, "z9hG4bK-1", 1, "INVITE", "at", None, "cid-tt");
        assert!(ToTagPresenceRule.check(&[sent("a", r, 0)], "a").is_empty());
    }

    #[test]
    fn final_response_without_to_tag_is_flagged() {
        let r = resp(200, "z9hG4bK-1", 1, "INVITE", "at", None, "cid-tt");
        let f = ToTagPresenceRule.check(&[sent("a", r, 0)], "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("missing a To-tag"), "{}", f[0]);
    }

    // ── RecordRouteRule ─────────────────────────────────────────────────────

    #[test]
    fn request_without_b2bua_record_route_is_clean() {
        let r = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None,
            "Record-Route: <sip:proxy@127.0.0.1;lr>\r\n", "cid-rr",
        );
        assert!(RecordRouteRule.check(&[sent("a", r, 0)], "a").is_empty());
    }

    #[test]
    fn b2bua_record_route_is_flagged() {
        let r = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-1", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None,
            "Record-Route: <sip:b2bua@127.0.0.1;callRef=abc;leg=A;lr>\r\n", "cid-rr",
        );
        let f = RecordRouteRule.check(&[sent("a", r, 0)], "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("MUST NOT use Record-Route"), "{}", f[0]);
    }

    #[test]
    fn record_route_subject_is_proxy_only() {
        assert_eq!(RecordRouteRule.subject(), HashSet::from([UaRole::Proxy]));
    }

    // ── ViaRule ─────────────────────────────────────────────────────────────

    #[test]
    fn response_echoing_via_branch_is_clean() {
        let req = build_req(
            "OPTIONS", "sip:bob@127.0.0.1:5070", "z9hG4bK-via", 5, "at", None, Some("70"),
            None, "", None, "", "cid-via",
        );
        let rp = resp(200, "z9hG4bK-via", 5, "OPTIONS", "at", Some("bt"), "cid-via");
        let evs = vec![sent("a", req, 0), recv_at("a", rp, 1)];
        assert!(ViaRule.check(&evs, "a").is_empty());
    }

    #[test]
    fn response_with_wrong_via_branch_is_flagged() {
        let req = build_req(
            "OPTIONS", "sip:bob@127.0.0.1:5070", "z9hG4bK-sent", 5, "at", None, Some("70"),
            None, "", None, "", "cid-via",
        );
        let rp = resp(200, "z9hG4bK-other", 5, "OPTIONS", "at", Some("bt"), "cid-via");
        let evs = vec![sent("a", req, 0), recv_at("a", rp, 1)];
        let f = ViaRule.check(&evs, "a");
        assert!(!f.is_empty(), "{f:?}");
        assert!(f.iter().any(|m| m.contains("differs from the branch")), "{f:?}");
    }

    // ── ResponseCorrelationRule ─────────────────────────────────────────────

    #[test]
    fn correlated_response_is_clean() {
        let req = build_req(
            "OPTIONS", "sip:bob@127.0.0.1:5070", "z9hG4bK-rc", 5, "at", None, Some("70"),
            None, "", None, "", "cid-rc",
        );
        let rp = resp(200, "z9hG4bK-rc", 5, "OPTIONS", "at", Some("bt"), "cid-rc");
        let evs = vec![sent("a", req, 0), recv_at("a", rp, 1)];
        assert!(ResponseCorrelationRule.check(&evs, "a").is_empty());
    }

    #[test]
    fn phantom_response_cseq_is_flagged() {
        // We sent OPTIONS CSeq 5, but a 200 OPTIONS CSeq 9 arrives — phantom.
        let req = build_req(
            "OPTIONS", "sip:bob@127.0.0.1:5070", "z9hG4bK-rc", 5, "at", None, Some("70"),
            None, "", None, "", "cid-rc",
        );
        let rp = resp(200, "z9hG4bK-rc", 9, "OPTIONS", "at", Some("bt"), "cid-rc");
        let evs = vec![sent("a", req, 0), recv_at("a", rp, 1)];
        let f = ResponseCorrelationRule.check(&evs, "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("does not echo any sent"), "{}", f[0]);
    }

    // ── TagsRule ────────────────────────────────────────────────────────────

    #[test]
    fn in_dialog_request_with_local_to_tag_is_clean() {
        // The bind sent a 200 minting local To-tag "lt"; a received in-dialog BYE
        // carries that To-tag → clean. The received INVITE confirms remote tag.
        let recv_invite = build_req(
            "INVITE", "sip:alice@127.0.0.1:5070", "z9hG4bK-i", 1, "rt", None, Some("70"),
            Some("<sip:bob@127.0.0.1>"), "", None, "", "cid-tags",
        );
        let sent_ok = resp(200, "z9hG4bK-i", 1, "INVITE", "rt", Some("lt"), "cid-tags");
        let recv_bye = build_req(
            "BYE", "sip:me@127.0.0.1:5070", "z9hG4bK-b", 2, "rt", Some("lt"), Some("70"),
            None, "", None, "", "cid-tags",
        );
        let evs =
            vec![recv_at("a", recv_invite, 0), sent("a", sent_ok, 1), recv_at("a", recv_bye, 2)];
        assert!(TagsRule.check(&evs, "a").is_empty());
    }

    #[test]
    fn in_dialog_request_with_foreign_to_tag_is_flagged() {
        let recv_invite = build_req(
            "INVITE", "sip:alice@127.0.0.1:5070", "z9hG4bK-i", 1, "rt", None, Some("70"),
            Some("<sip:bob@127.0.0.1>"), "", None, "", "cid-tags",
        );
        let sent_ok = resp(200, "z9hG4bK-i", 1, "INVITE", "rt", Some("lt"), "cid-tags");
        // BYE carries To-tag "WRONG" — not one this bind minted.
        let recv_bye = build_req(
            "BYE", "sip:me@127.0.0.1:5070", "z9hG4bK-b", 2, "rt", Some("WRONG"), Some("70"),
            None, "", None, "", "cid-tags",
        );
        let evs =
            vec![recv_at("a", recv_invite, 0), sent("a", sent_ok, 1), recv_at("a", recv_bye, 2)];
        let f = TagsRule.check(&evs, "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("To-tag mismatch"), "{}", f[0]);
    }

    // ── DialogUriRule ───────────────────────────────────────────────────────

    #[test]
    fn stable_in_dialog_from_uri_is_clean() {
        let recv_invite = build_req(
            "INVITE", "sip:me@127.0.0.1:5070", "z9hG4bK-i", 1, "rt", None, Some("70"),
            Some("<sip:peer@127.0.0.1>"), "", None, "", "cid-du",
        );
        // Re-INVITE from the same From URI (alice@127.0.0.1) — stable.
        let recv_reinvite = build_req(
            "INVITE", "sip:me@127.0.0.1:5070", "z9hG4bK-ri", 2, "rt", Some("lt"), Some("70"),
            Some("<sip:peer@127.0.0.1>"), "", None, "", "cid-du",
        );
        let evs = vec![recv_at("a", recv_invite, 0), recv_at("a", recv_reinvite, 1)];
        assert!(DialogUriRule.check(&evs, "a").is_empty());
    }

    #[test]
    fn rewritten_in_dialog_from_uri_is_flagged() {
        let recv_invite = build_req(
            "INVITE", "sip:me@127.0.0.1:5070", "z9hG4bK-i", 1, "rt", None, Some("70"),
            Some("<sip:peer@127.0.0.1>"), "", None, "", "cid-du",
        );
        // Re-INVITE whose From URI was rewritten (alice → mallory).
        let recv_reinvite = "INVITE sip:me@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-ri\r\n\
             From: <sip:mallory@127.0.0.1>;tag=rt\r\n\
             To: <sip:bob@127.0.0.1>;tag=lt\r\n\
             Call-ID: cid-du\r\n\
             CSeq: 2 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
            .to_string()
            .into_bytes();
        let evs = vec![recv_at("a", recv_invite, 0), recv_at("a", recv_reinvite, 1)];
        let f = DialogUriRule.check(&evs, "a");
        assert!(!f.is_empty(), "{f:?}");
        assert!(f.iter().any(|m| m.contains("differs from the dialog-established remote URI")), "{f:?}");
    }

    // ── CallIdRule ──────────────────────────────────────────────────────────

    #[test]
    fn consistent_call_id_within_dialog_is_clean() {
        // First received INVITE confirms Call-ID "cid-1"; a later in-dialog OPTIONS
        // on the same From-tag carries the same Call-ID → clean.
        let inv = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-i", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-1",
        );
        let opt = build_req(
            "OPTIONS", "sip:bob@127.0.0.1:5070", "z9hG4bK-o", 2, "at", Some("bt"), Some("70"),
            None, "", None, "", "cid-1",
        );
        let evs = vec![recv_at("a", inv, 0), recv_at("a", opt, 1)];
        assert!(CallIdRule.check(&evs, "a").is_empty());
    }

    #[test]
    fn changed_call_id_within_dialog_is_flagged() {
        // INVITE confirms "cid-1"; a later message on the same From-tag carries
        // "cid-2" — a mid-dialog Call-ID change.
        let inv = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-i", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-1",
        );
        let opt = build_req(
            "OPTIONS", "sip:bob@127.0.0.1:5070", "z9hG4bK-o", 2, "at", Some("bt"), Some("70"),
            None, "", None, "", "cid-2",
        );
        let evs = vec![recv_at("a", inv, 0), recv_at("a", opt, 1)];
        let f = CallIdRule.check(&evs, "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("differs from the dialog's confirmed Call-ID"), "{}", f[0]);
    }

    // ── CancelRequestUriRule ────────────────────────────────────────────────

    #[test]
    fn cancel_with_matching_request_uri_is_clean() {
        let inv = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-i", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-cru",
        );
        let cancel = build_req(
            "CANCEL", "sip:bob@127.0.0.1:5070", "z9hG4bK-i", 1, "at", None, Some("70"),
            None, "", None, "", "cid-cru",
        );
        let evs = vec![recv_at("a", inv, 0), recv_at("a", cancel, 1)];
        assert!(CancelRequestUriRule.check(&evs, "a").is_empty());
    }

    #[test]
    fn cancel_with_wrong_request_uri_is_flagged() {
        let inv = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-i", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-cru",
        );
        let cancel = build_req(
            "CANCEL", "sip:carol@127.0.0.1:5070", "z9hG4bK-i", 1, "at", None, Some("70"),
            None, "", None, "", "cid-cru",
        );
        let evs = vec![recv_at("a", inv, 0), recv_at("a", cancel, 1)];
        let f = CancelRequestUriRule.check(&evs, "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("CANCEL Request-URI"), "{}", f[0]);
    }

    // ── CancelViaBranchRule ─────────────────────────────────────────────────

    #[test]
    fn cancel_with_matching_via_branch_is_clean() {
        let inv = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-shared", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-cvb",
        );
        let cancel = build_req(
            "CANCEL", "sip:bob@127.0.0.1:5070", "z9hG4bK-shared", 1, "at", None, Some("70"),
            None, "", None, "", "cid-cvb",
        );
        let evs = vec![recv_at("a", inv, 0), recv_at("a", cancel, 1)];
        assert!(CancelViaBranchRule.check(&evs, "a").is_empty());
    }

    #[test]
    fn cancel_with_wrong_via_branch_is_flagged() {
        let inv = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-inv", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-cvb",
        );
        let cancel = build_req(
            "CANCEL", "sip:bob@127.0.0.1:5070", "z9hG4bK-different", 1, "at", None, Some("70"),
            None, "", None, "", "cid-cvb",
        );
        let evs = vec![recv_at("a", inv, 0), recv_at("a", cancel, 1)];
        let f = CancelViaBranchRule.check(&evs, "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("CANCEL top Via branch"), "{}", f[0]);
    }

    // ── RackCorrelationRule ─────────────────────────────────────────────────

    fn prack_with_rack(rack: &str, call_id: &str) -> Vec<u8> {
        format!(
            "PRACK sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-p\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn rack_matching_received_invite_is_clean() {
        let inv = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-i", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-rack",
        );
        // RAck "1 1 INVITE" → references INVITE CSeq 1, which we received.
        let prack = prack_with_rack("1 1 INVITE", "cid-rack");
        let evs = vec![recv_at("a", inv, 0), recv_at("a", prack, 1)];
        assert!(RackCorrelationRule.check(&evs, "a").is_empty());
    }

    #[test]
    fn rack_referencing_unknown_invite_is_flagged() {
        let inv = build_req(
            "INVITE", "sip:bob@127.0.0.1:5070", "z9hG4bK-i", 1, "at", None, Some("70"),
            Some("<sip:alice@127.0.0.1>"), "", None, "", "cid-rack",
        );
        // RAck references INVITE CSeq 7 — never received.
        let prack = prack_with_rack("1 7 INVITE", "cid-rack");
        let evs = vec![recv_at("a", inv, 0), recv_at("a", prack, 1)];
        let f = RackCorrelationRule.check(&evs, "a");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("does not match any received INVITE"), "{}", f[0]);
    }
}
