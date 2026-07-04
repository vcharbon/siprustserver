//! generators — pure, correct-by-default SIP message builders. Port of
//! `src/sip/generators.ts`.
//!
//! Every returned [`SipRequest`] / [`SipResponse`] is immediately sendable: no
//! sentinels, no post-processing. Via and Contact are materialised from the
//! [`ViaSpec`] / [`ContactSpec`] passed in. All generators are pure functions —
//! Call-ID, branch, tag, local address and CSeq are arguments, never side
//! effects.
//!
//! [`StackDialog`] and [`InviteClientTransactionHandle`] are modelled here as
//! the **minimal input shapes** the generators read. The full `Dialog` /
//! `TransactionLayer` modules they originate from are slice 2 (network /
//! transaction); when ported, these can become re-exports.

use crate::message_helpers::get_header;
use crate::parser::custom::structured_headers::parse_name_addr;
use crate::parser::custom::{hydrate_request, hydrate_response};
use crate::types::{SipHeader, SipMessage, SipRequest, SipResponse};

// ---------------------------------------------------------------------------
// Public input shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SipTransport {
    Udp,
    Tcp,
    Tls,
    Ws,
    Wss,
}

impl SipTransport {
    pub fn as_str(&self) -> &'static str {
        match self {
            SipTransport::Udp => "UDP",
            SipTransport::Tcp => "TCP",
            SipTransport::Tls => "TLS",
            SipTransport::Ws => "WS",
            SipTransport::Wss => "WSS",
        }
    }
}

/// Structured Via input. `custom_params` are B2BUA-opaque (e.g. `cr`, `lg`,
/// `em`) and are appended in order; an empty value serialises as a flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViaSpec {
    pub local_ip: String,
    pub local_port: u16,
    pub transport: SipTransport,
    pub branch: String,
    pub custom_params: Vec<(String, String)>,
}

/// Structured Contact input. `uri_params` are B2BUA-opaque (e.g. `callRef`,
/// `leg`, `emerg`) and appended verbatim in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSpec {
    pub user: String,
    pub host: String,
    pub port: u16,
    pub uri_params: Vec<(String, String)>,
}

/// Methods the in-dialog generator accepts (ACK excluded — it has its own
/// primitive [`generate_ack_for_2xx`], a compile-time guarantee).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InDialogMethod {
    Bye,
    Invite,
    Prack,
    Notify,
    Options,
    Info,
    Update,
    Message,
    /// REFER (RFC 3515). The B2BUA never *originates* a REFER; this exists for
    /// the harness to send one from an agent (Refer-To / Replaces ride via
    /// `extra_headers`).
    Refer,
}

impl InDialogMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            InDialogMethod::Bye => "BYE",
            InDialogMethod::Invite => "INVITE",
            InDialogMethod::Prack => "PRACK",
            InDialogMethod::Notify => "NOTIFY",
            InDialogMethod::Options => "OPTIONS",
            InDialogMethod::Info => "INFO",
            InDialogMethod::Update => "UPDATE",
            InDialogMethod::Message => "MESSAGE",
            InDialogMethod::Refer => "REFER",
        }
    }
}

impl From<InDialogMethod> for crate::method::Method {
    fn from(m: InDialogMethod) -> Self {
        use crate::method::Method;
        match m {
            InDialogMethod::Bye => Method::Bye,
            InDialogMethod::Invite => Method::Invite,
            InDialogMethod::Prack => Method::Prack,
            InDialogMethod::Notify => Method::Notify,
            InDialogMethod::Options => Method::Options,
            InDialogMethod::Info => Method::Info,
            InDialogMethod::Update => Method::Update,
            InDialogMethod::Message => Method::Message,
            InDialogMethod::Refer => Method::Refer,
        }
    }
}

/// `InDialogMethod` is a *view* over [`Method`](crate::method::Method): exactly
/// the methods the B2BUA may send as an ordinary in-dialog request. `ACK`/`CANCEL`
/// (own primitives) and out-of-dialog-only methods (`REGISTER`/`SUBSCRIBE`/…) are
/// rejected — this is the admissibility invariant the generators rely on.
impl TryFrom<&crate::method::Method> for InDialogMethod {
    type Error = ();
    fn try_from(m: &crate::method::Method) -> Result<Self, Self::Error> {
        use crate::method::Method;
        Ok(match m {
            Method::Bye => InDialogMethod::Bye,
            Method::Invite => InDialogMethod::Invite,
            Method::Prack => InDialogMethod::Prack,
            Method::Notify => InDialogMethod::Notify,
            Method::Options => InDialogMethod::Options,
            Method::Info => InDialogMethod::Info,
            Method::Update => InDialogMethod::Update,
            Method::Message => InDialogMethod::Message,
            Method::Refer => InDialogMethod::Refer,
            _ => return Err(()),
        })
    }
}

/// Methods the out-of-dialog generator accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutOfDialogMethod {
    Invite,
    Options,
    Message,
    Register,
    Subscribe,
    Publish,
}

impl OutOfDialogMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutOfDialogMethod::Invite => "INVITE",
            OutOfDialogMethod::Options => "OPTIONS",
            OutOfDialogMethod::Message => "MESSAGE",
            OutOfDialogMethod::Register => "REGISTER",
            OutOfDialogMethod::Subscribe => "SUBSCRIBE",
            OutOfDialogMethod::Publish => "PUBLISH",
        }
    }
}

impl From<OutOfDialogMethod> for crate::method::Method {
    fn from(m: OutOfDialogMethod) -> Self {
        use crate::method::Method;
        match m {
            OutOfDialogMethod::Invite => Method::Invite,
            OutOfDialogMethod::Options => Method::Options,
            OutOfDialogMethod::Message => Method::Message,
            OutOfDialogMethod::Register => Method::Register,
            OutOfDialogMethod::Subscribe => Method::Subscribe,
            OutOfDialogMethod::Publish => Method::Publish,
        }
    }
}

/// `OutOfDialogMethod` is a *view* over [`Method`](crate::method::Method): the
/// methods the stack may originate outside a dialog.
impl TryFrom<&crate::method::Method> for OutOfDialogMethod {
    type Error = ();
    fn try_from(m: &crate::method::Method) -> Result<Self, Self::Error> {
        use crate::method::Method;
        Ok(match m {
            Method::Invite => OutOfDialogMethod::Invite,
            Method::Options => OutOfDialogMethod::Options,
            Method::Message => OutOfDialogMethod::Message,
            Method::Register => OutOfDialogMethod::Register,
            Method::Subscribe => OutOfDialogMethod::Subscribe,
            Method::Publish => OutOfDialogMethod::Publish,
            _ => return Err(()),
        })
    }
}

/// Minimal dialog shape the in-dialog generators read (full `Dialog` is slice 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackDialog {
    pub call_id: String,
    pub local_tag: String,
    pub remote_tag: String,
    pub local_uri: String,
    pub remote_uri: String,
    pub remote_target: String,
    pub local_cseq: u32,
    pub route_set: Vec<String>,
}

/// Minimal INVITE client-transaction handle the CANCEL / ACK-for-2xx generators
/// read (full `TransactionLayer` is slice 2). Only `original_invite` is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteClientTransactionHandle {
    pub original_invite: SipRequest,
}

// ---------------------------------------------------------------------------
// Structural header set (RFC 3261 §16.6 — stack-owned; never copied transparently)
// ---------------------------------------------------------------------------

const STRUCTURAL_HEADERS: &[&str] = &[
    "via",
    "contact",
    "from",
    "to",
    "call-id",
    "cseq",
    "max-forwards",
    "content-length",
    "content-type",
    "record-route",
    "route",
];

// RFC 3261 §13.2.1 / §20.37 — accepted methods + supported extensions, advertised
// on every B2BUA-originated INVITE so the peer can negotiate.
pub const B2BUA_ALLOW: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, REFER, NOTIFY, PRACK";
pub const B2BUA_SUPPORTED: &str = "100rel, timer, replaces";

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn h(name: &str, value: impl Into<String>) -> SipHeader {
    SipHeader { name: name.to_string(), value: value.into() }
}

/// `hydrate_request` cannot fail for stack-built (well-formed) input; surface a
/// failure as a panic at construction time rather than threading a Result —
/// matching the TS generators, which return `SipRequest` directly.
fn make_request(method: &str, uri: &str, headers: Vec<SipHeader>, body: Vec<u8>) -> SipRequest {
    hydrate_request(method, uri, headers, body)
        .unwrap_or_else(|e| panic!("generators built a malformed request: {}", e.reason))
}

fn make_response(status: u16, reason: &str, headers: Vec<SipHeader>, body: Vec<u8>) -> SipResponse {
    hydrate_response(status, reason, headers, body)
        .unwrap_or_else(|e| panic!("generators built a malformed response: {}", e.reason))
}

/// Deterministic fallback To-tag for a non-100 response whose request carried a
/// tag-less To and whose caller supplied no `to_tag`. Derived from the Call-ID so
/// it is stable per call (a retransmit re-derives the same tag) and unique across
/// calls. This only fires on a degenerate path — it exists so the worker emits a
/// well-formed response instead of panicking. See [`generate_response`].
fn fallback_to_tag(call_id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    call_id.hash(&mut hasher);
    format!("b2bua-fb-{:016x}", hasher.finish())
}

/// Serialize a [`ViaSpec`] into a Via header value. Custom params are appended
/// verbatim; an empty value serialises as a flag (RFC 3581 §3).
fn build_via_value(v: &ViaSpec) -> String {
    let mut out = format!(
        "SIP/2.0/{} {}:{};branch={}",
        v.transport.as_str(),
        v.local_ip,
        v.local_port,
        v.branch
    );
    for (k, val) in &v.custom_params {
        if val.is_empty() {
            out.push_str(&format!(";{k}"));
        } else {
            out.push_str(&format!(";{k}={val}"));
        }
    }
    out
}

/// Serialize a [`ContactSpec`] into an angle-bracketed Contact header value.
fn build_contact_value(c: &ContactSpec) -> String {
    let mut uri = format!("sip:{}@{}:{}", c.user, c.host, c.port);
    for (k, val) in &c.uri_params {
        uri.push_str(&format!(";{k}={val}"));
    }
    format!("<{uri}>")
}

/// Wrap a bare URI in angle brackets, unless it already contains `<` (a full
/// name-addr with display name passes through unchanged).
fn wrap_uri(uri_or_name_addr: &str) -> String {
    if uri_or_name_addr.contains('<') {
        uri_or_name_addr.to_string()
    } else {
        format!("<{uri_or_name_addr}>")
    }
}

// ---------------------------------------------------------------------------
// Loose / strict routing (RFC 3261 §12.2.1.1 / §16.12). Port of the
// `firstRouteIsLoose` / `stripRouteUriToRequestUri` helpers in
// `message-builder.ts`. Zero-regex per ADR-0001.
// ---------------------------------------------------------------------------

/// `true` if a Route / Record-Route header value carries the `;lr` loose-route
/// flag as a URI parameter — i.e. `;lr` followed by `;`, `>`, `,`, whitespace,
/// or end-of-string (not as a substring of some other token). Loose routing is
/// the modern default; strict routing is the legacy fallback.
pub fn first_route_is_loose(route_value: &str) -> bool {
    let lower = route_value.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(";lr") {
        let idx = from + rel;
        let after = idx + ";lr".len();
        if after == bytes.len()
            || matches!(bytes[after], b';' | b'>' | b',' | b' ' | b'\t' | b'\r' | b'\n')
        {
            return true;
        }
        from = after;
    }
    false
}

/// Extract the URI portion of a Route value for use as a strict-route
/// Request-URI: strips the surrounding angle brackets (RFC 3261 §16.12).
pub fn strip_route_uri_to_request_uri(route_value: &str) -> String {
    let trimmed = route_value.trim();
    if let Some(rest) = trimmed.strip_prefix('<') {
        if let Some(end) = rest.find('>') {
            return rest[..end].to_string();
        }
    }
    trimmed.to_string()
}

/// Compute the Request-URI and ordered Route header values for an in-dialog
/// request, given the dialog's remote target and route set (RFC 3261
/// §12.2.1.1 / §16.12):
///   - empty route set → `(remote_target, [])`;
///   - loose (first route has `;lr`) → `(remote_target, route_set)` as-is;
///   - strict → `(first route URI, rest of route_set ++ <remote_target>)`.
fn route_for_in_dialog(remote_target: &str, route_set: &[String]) -> (String, Vec<String>) {
    if route_set.is_empty() {
        return (remote_target.to_string(), Vec::new());
    }
    if first_route_is_loose(&route_set[0]) {
        (remote_target.to_string(), route_set.to_vec())
    } else {
        let request_uri = strip_route_uri_to_request_uri(&route_set[0]);
        let mut routes: Vec<String> = route_set[1..].to_vec();
        routes.push(format!("<{remote_target}>"));
        (request_uri, routes)
    }
}

/// Append Content-Type (when body is non-empty and the caller didn't already
/// include one) + Content-Length (RFC 3261 §7.4.1).
fn append_body_headers(headers: &mut Vec<SipHeader>, body: &[u8], content_type: Option<&str>) {
    let has_ct = headers.iter().any(|hdr| hdr.name.eq_ignore_ascii_case("content-type"));
    if !body.is_empty() && !has_ct {
        headers.push(h("Content-Type", content_type.unwrap_or("application/sdp")));
    }
    headers.push(h("Content-Length", body.len().to_string()));
}

/// RFC 3261 §18.2.1 + RFC 3581 §4: stamp `received=` (if sent-by host differs
/// from the source) and replace any `rport` flag with `rport=<port>` on the
/// topmost Via. Idempotent: already-populated parameters are left alone.
pub fn stamp_received_rport_on_via(value: &str, src_ip: &str, src_port: u16) -> String {
    let (head, mut params) = match value.find(';') {
        Some(semi) => (&value[..semi], value[semi..].to_string()),
        None => (value, String::new()),
    };
    let hp = head.split(' ').next_back().unwrap_or("");
    let sent_by_host = match hp.rfind(':') {
        Some(colon) => &hp[..colon],
        None => hp,
    };
    let need_received = sent_by_host != src_ip;
    let lower = params.to_ascii_lowercase();
    let has_received = lower.contains(";received=");
    let rport_flag = rport_flag_present(&params);

    if need_received && !has_received {
        params.push_str(&format!(";received={src_ip}"));
    }
    if rport_flag {
        params = replace_rport_flag(&params, src_port);
    }
    format!("{head}{params}")
}

/// Detect a bare `;rport` flag (followed by `;` or end), case-insensitive.
fn rport_flag_present(params: &str) -> bool {
    find_rport_flag(params).is_some()
}

/// Find the byte offset of a bare `;rport` flag in `params`, if present.
fn find_rport_flag(params: &str) -> Option<usize> {
    let lower = params.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find(";rport") {
        let idx = search_from + rel;
        let after = idx + ";rport".len();
        if after == bytes.len() || bytes[after] == b';' {
            return Some(idx);
        }
        search_from = after;
    }
    None
}

fn replace_rport_flag(params: &str, src_port: u16) -> String {
    match find_rport_flag(params) {
        Some(idx) => {
            let after = idx + ";rport".len();
            format!("{};rport={}{}", &params[..idx], src_port, &params[after..])
        }
        None => params.to_string(),
    }
}

// ---------------------------------------------------------------------------
// extract_non_structural_headers
// ---------------------------------------------------------------------------

/// Every header from `msg` whose name is NOT in the stack-owned structural set
/// — callers pass the result through `extra_headers` when relaying so
/// transparent fields (Allow, Supported, P-Asserted-Identity, …) flow through
/// unchanged while the generator owns the dialog headers.
pub fn extract_non_structural_headers(msg: &SipMessage) -> Vec<SipHeader> {
    msg.headers()
        .iter()
        .filter(|hdr| {
            let lower = hdr.name.to_ascii_lowercase();
            !STRUCTURAL_HEADERS.contains(&lower.as_str())
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Out-of-dialog request (initial INVITE, OPTIONS, …)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GenerateOutOfDialogRequestOpts {
    pub request_uri: String,
    pub call_id: String,
    pub from_uri: String,
    pub from_tag: String,
    pub to_uri: String,
    pub to_tag: Option<String>,
    pub cseq: u32,
    pub via: Option<ViaSpec>,
    pub contact: Option<ContactSpec>,
    pub max_forwards: Option<u32>,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub extra_headers: Vec<SipHeader>,
}

/// Build an out-of-dialog request — initial INVITE, one-shot OPTIONS, MESSAGE,
/// REGISTER, SUBSCRIBE, PUBLISH (RFC 3261 §8.1.1).
pub fn generate_out_of_dialog_request(
    method: OutOfDialogMethod,
    opts: &GenerateOutOfDialogRequestOpts,
) -> SipRequest {
    let body = opts.body.clone();
    let max_forwards = opts.max_forwards.unwrap_or(70);
    let via = opts.via.as_ref().expect("ViaSpec required");
    let contact = opts.contact.as_ref().expect("ContactSpec required");

    let to_value = match &opts.to_tag {
        Some(tag) => format!("{};tag={}", wrap_uri(&opts.to_uri), tag),
        None => wrap_uri(&opts.to_uri),
    };

    let mut headers: Vec<SipHeader> = vec![
        h("Via", build_via_value(via)),
        h("Max-Forwards", max_forwards.to_string()),
        h("From", format!("{};tag={}", wrap_uri(&opts.from_uri), opts.from_tag)),
        h("To", to_value),
        h("Call-ID", opts.call_id.clone()),
        h("CSeq", format!("{} {}", opts.cseq, method.as_str())),
        h("Contact", build_contact_value(contact)),
    ];
    headers.extend(opts.extra_headers.iter().cloned());
    append_body_headers(&mut headers, &body, opts.content_type.as_deref());

    make_request(method.as_str(), &opts.request_uri, headers, body)
}

// ---------------------------------------------------------------------------
// In-dialog request (BYE, re-INVITE, PRACK, NOTIFY, INFO, UPDATE, MESSAGE)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GenerateInDialogRequestOpts {
    pub via: Option<ViaSpec>,
    pub contact: Option<ContactSpec>,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub extra_headers: Vec<SipHeader>,
    /// Required when method == PRACK (RFC 3262).
    pub rack: Option<String>,
    /// Required when method == NOTIFY (RFC 6665 §7.2).
    pub event: Option<String>,
    /// Required when method == NOTIFY (RFC 6665 §4.1.3).
    pub subscription_state: Option<String>,
    /// Explicit CSeq override; defaults to `dialog.local_cseq + 1`.
    pub cseq: Option<u32>,
    /// Request-URI override; defaults to `dialog.remote_target`.
    pub request_uri: Option<String>,
}

/// Result of [`generate_in_dialog_request`]: the request plus the dialog with
/// `local_cseq` bumped to the used CSeq (callers persist the new dialog).
pub struct InDialogResult {
    pub request: SipRequest,
    pub dialog: StackDialog,
}

/// Build an in-dialog request (RFC 3261 §12.2.1.1).
pub fn generate_in_dialog_request(
    method: InDialogMethod,
    dialog: &StackDialog,
    opts: &GenerateInDialogRequestOpts,
) -> InDialogResult {
    let body = opts.body.clone();
    let next_cseq = opts.cseq.unwrap_or(dialog.local_cseq + 1);
    let remote_target = opts.request_uri.clone().unwrap_or_else(|| dialog.remote_target.clone());
    // RFC 3261 §12.2.1.1 / §16.12: Request-URI + Route headers from the route
    // set (loose → R-URI = remote target, routes as-is; strict → R-URI = first
    // route, remote target appended as the final Route).
    let (request_uri, route_values) = route_for_in_dialog(&remote_target, &dialog.route_set);
    let via = opts.via.as_ref().expect("ViaSpec required");

    // RFC 3261 §12.2.1.1: the To header carries the remote tag — but a dialog
    // taken over mid-confirm (a failover hydrate of a replica copy whose 200-OK
    // had not yet established the remote tag) can have an EMPTY `remote_tag`.
    // Emitting `;tag=` (empty value) builds a malformed header that
    // `hydrate_request` rejects → panic ("Empty To tag parameter"). Skip the tag
    // when absent so the request is at least well-formed (the degenerate dialog is
    // handled elsewhere); a present tag is emitted as before.
    let to_value = if dialog.remote_tag.is_empty() {
        wrap_uri(&dialog.remote_uri)
    } else {
        format!("{};tag={}", wrap_uri(&dialog.remote_uri), dialog.remote_tag)
    };
    let mut headers: Vec<SipHeader> = vec![
        h("Via", build_via_value(via)),
        h("Max-Forwards", "70"),
        h("From", format!("{};tag={}", wrap_uri(&dialog.local_uri), dialog.local_tag)),
        h("To", to_value),
        h("Call-ID", dialog.call_id.clone()),
        h("CSeq", format!("{} {}", next_cseq, method.as_str())),
    ];

    // Contact for every in-dialog method EXCEPT BYE (RFC 3261 §15.1).
    if method != InDialogMethod::Bye {
        let contact = opts.contact.as_ref().expect("ContactSpec required");
        headers.push(h("Contact", build_contact_value(contact)));
    }

    for route in &route_values {
        headers.push(h("Route", route.clone()));
    }

    if method == InDialogMethod::Prack {
        if let Some(rack) = &opts.rack {
            headers.push(h("RAck", rack.clone()));
        }
    }
    if method == InDialogMethod::Notify {
        if let Some(event) = &opts.event {
            headers.push(h("Event", event.clone()));
        }
        if let Some(ss) = &opts.subscription_state {
            headers.push(h("Subscription-State", ss.clone()));
        }
    }
    if method == InDialogMethod::Invite {
        // Advertise capabilities — but do NOT duplicate a header the caller already
        // carries through `extra_headers` (e.g. a transparently-relayed re-INVITE
        // whose inbound Allow/Supported are passed through verbatim). Emitting it
        // twice is a malformed-adjacent header (values merge per RFC 3261 §7.3.1).
        let carried =
            |name: &str| opts.extra_headers.iter().any(|hdr| hdr.name.eq_ignore_ascii_case(name));
        if !carried("Allow") {
            headers.push(h("Allow", B2BUA_ALLOW));
        }
        if !carried("Supported") {
            headers.push(h("Supported", B2BUA_SUPPORTED));
        }
    }

    headers.extend(opts.extra_headers.iter().cloned());
    append_body_headers(&mut headers, &body, opts.content_type.as_deref());

    let request = make_request(method.as_str(), &request_uri, headers, body);
    let next_dialog = StackDialog { local_cseq: next_cseq, ..dialog.clone() };
    InDialogResult { request, dialog: next_dialog }
}

// ---------------------------------------------------------------------------
// ACK for 2xx
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GenerateAckFor2xxOpts {
    pub via: Option<ViaSpec>,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub extra_headers: Vec<SipHeader>,
    /// Explicit CSeq override; required when `invite_txn` is `None`.
    pub cseq: Option<u32>,
    /// Request-URI override; defaults to `dialog.remote_target`.
    pub request_uri: Option<String>,
}

/// Build an ACK for a 2xx response. The CSeq number comes from the INVITE
/// (RFC 3261 §13.2.2.4), not from `dialog.local_cseq`. Panics when neither
/// `invite_txn` nor `opts.cseq` is provided.
pub fn generate_ack_for_2xx(
    invite_txn: Option<&InviteClientTransactionHandle>,
    dialog: &StackDialog,
    opts: &GenerateAckFor2xxOpts,
) -> SipRequest {
    let body = opts.body.clone();
    let invite_cseq = opts
        .cseq
        .or_else(|| invite_txn.map(|t| t.original_invite.cseq.seq))
        .expect("generate_ack_for_2xx: either invite_txn or opts.cseq must be provided");
    let remote_target = opts.request_uri.clone().unwrap_or_else(|| dialog.remote_target.clone());
    // ACK honours the dialog route set the same way (RFC 3261 §13.2.2.4 routes
    // the ACK like any in-dialog request).
    let (request_uri, route_values) = route_for_in_dialog(&remote_target, &dialog.route_set);
    let via = opts.via.as_ref().expect("ViaSpec required");

    // RFC 3261 §12.2.1.1: the To header carries the remote tag — but a dialog
    // taken over mid-confirm (a reactive failover hydrate of a replica copy whose
    // relayed 2xx had not yet established the remote tag) can have an EMPTY
    // `remote_tag`. Emitting `;tag=` (empty value) builds a malformed header that
    // `hydrate_request` rejects → panic ("Empty To tag parameter"). Skip the tag
    // when absent so the ACK is at least well-formed (mirrors the request
    // generator at the top of this file; reachable via reactive mid-confirm
    // takeover → relayed-2xx ACK in `rules::relay`).
    let to_value = if dialog.remote_tag.is_empty() {
        wrap_uri(&dialog.remote_uri)
    } else {
        format!("{};tag={}", wrap_uri(&dialog.remote_uri), dialog.remote_tag)
    };

    let mut headers: Vec<SipHeader> = vec![
        h("Via", build_via_value(via)),
        h("Max-Forwards", "70"),
        h("From", format!("{};tag={}", wrap_uri(&dialog.local_uri), dialog.local_tag)),
        h("To", to_value),
        h("Call-ID", dialog.call_id.clone()),
        h("CSeq", format!("{invite_cseq} ACK")),
    ];
    for route in &route_values {
        headers.push(h("Route", route.clone()));
    }
    headers.extend(opts.extra_headers.iter().cloned());
    append_body_headers(&mut headers, &body, opts.content_type.as_deref());

    make_request("ACK", &request_uri, headers, body)
}

// ---------------------------------------------------------------------------
// CANCEL
// ---------------------------------------------------------------------------

/// Build a CANCEL for the outstanding INVITE (RFC 3261 §9.1): the CANCEL "MUST
/// have a single Via header field value, and that value MUST equal the top Via
/// header field value of the request being cancelled" — copied verbatim (same
/// branch, the transaction-correlation key); Request-URI / Call-ID / From / To
/// echo the INVITE; CSeq number reused with method CANCEL. §9.1 also requires
/// the CANCEL to be routed the SAME way as the INVITE — "the Route header fields
/// of the CANCEL … MUST equal" the INVITE's — so its Route set is reproduced
/// verbatim (load-bearing when the INVITE carried a preloaded outbound-proxy
/// Route: without it the CANCEL bypasses the proxy the INVITE traversed, and the
/// cross-message audit flags `cancelRouteEchoesInvite`). Panics when the INVITE
/// is missing a required header.
pub fn generate_cancel(invite_txn: &InviteClientTransactionHandle) -> SipRequest {
    let invite = &invite_txn.original_invite;
    let via = get_header(&invite.headers, "via").expect("generate_cancel: INVITE missing Via");
    let from = get_header(&invite.headers, "from").expect("generate_cancel: INVITE missing From");
    let to = get_header(&invite.headers, "to").expect("generate_cancel: INVITE missing To");
    let call_id =
        get_header(&invite.headers, "call-id").expect("generate_cancel: INVITE missing Call-ID");
    let invite_cseq = invite.cseq.seq;

    let mut headers: Vec<SipHeader> = vec![
        h("Via", via),
        h("Max-Forwards", "70"),
        h("From", from),
        h("To", to),
        h("Call-ID", call_id),
        h("CSeq", format!("{invite_cseq} CANCEL")),
    ];
    for route in crate::message_helpers::get_headers(&invite.headers, "route") {
        headers.push(h("Route", route));
    }
    headers.push(h("Content-Length", "0"));

    make_request("CANCEL", &invite.uri, headers, Vec::new())
}

// ---------------------------------------------------------------------------
// UAS response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GenerateResponseOpts {
    /// Tag added to To when status > 100 and the request's To lacks one.
    pub to_tag: Option<String>,
    pub contact: Option<ContactSpec>,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub extra_headers: Vec<SipHeader>,
    /// Source the request arrived from — stamps `received=` / `rport=` on the
    /// topmost echoed Via (RFC 3261 §18.2.1 + RFC 3581 §4).
    pub incoming_source: Option<(String, u16)>,
}

/// Build a UAS response to `incoming_request`, echoing Via / From / To /
/// Call-ID / CSeq (RFC 3261 §8.2.6.2).
pub fn generate_response(
    incoming_request: &SipRequest,
    status: u16,
    reason: &str,
    opts: &GenerateResponseOpts,
) -> SipResponse {
    let body = opts.body.clone();

    let raw_to = get_header(&incoming_request.headers, "to").unwrap_or("");
    let from = get_header(&incoming_request.headers, "from").unwrap_or("");
    let call_id = get_header(&incoming_request.headers, "call-id").unwrap_or("");
    let cseq = get_header(&incoming_request.headers, "cseq").unwrap_or("");

    // A non-100 response MUST carry a To-tag (RFC 3261 §8.2.6.2); hydrate_response
    // rejects one that doesn't. If the request's To already has a tag (in-dialog)
    // echo it; otherwise add `opts.to_tag` when the caller supplied one, else a
    // deterministic fallback derived from the Call-ID. The fallback is a safety
    // net: a worker must NEVER panic building a response (it kills the handler
    // task, leaks the dialog, and eventually OOMs) just because some caller path
    // answered a tag-less request without threading a To-tag through.
    let to = if status > 100 && parse_name_addr(raw_to).tag.is_none() {
        let tag = opts
            .to_tag
            .clone()
            .unwrap_or_else(|| fallback_to_tag(call_id));
        format!("{raw_to};tag={tag}")
    } else {
        raw_to.to_string()
    };

    let mut headers: Vec<SipHeader> = Vec::new();

    // Echo every Via in order; stamp the topmost from `incoming_source`.
    let mut stamped_top_via = false;
    for hdr in &incoming_request.headers {
        if !hdr.name.eq_ignore_ascii_case("via") {
            continue;
        }
        let value = match (&opts.incoming_source, stamped_top_via) {
            (Some((ip, port)), false) => stamp_received_rport_on_via(&hdr.value, ip, *port),
            _ => hdr.value.clone(),
        };
        stamped_top_via = true;
        headers.push(h("Via", value));
    }

    // Echo Record-Route verbatim (RFC 3261 §16.6) — but NOT on a 100 Trying: a
    // 100 establishes no dialog, so its Record-Route is inert (the UAC ignores it)
    // and merely bloats the provisional. Dialog-establishing 18x/2xx still carry it.
    if status != 100 {
        for hdr in &incoming_request.headers {
            if hdr.name.eq_ignore_ascii_case("record-route") {
                headers.push(h("Record-Route", hdr.value.clone()));
            }
        }
    }

    headers.push(h("From", from));
    headers.push(h("To", to));
    headers.push(h("Call-ID", call_id));
    headers.push(h("CSeq", cseq));

    if let Some(contact) = &opts.contact {
        headers.push(h("Contact", build_contact_value(contact)));
    }

    headers.extend(opts.extra_headers.iter().cloned());
    append_body_headers(&mut headers, &body, opts.content_type.as_deref());

    make_response(status, reason, headers, body)
}

// ---------------------------------------------------------------------------
// Relayed response — B2BUA rebuilds a response from snapshotted fields
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GenerateRelayedResponseOpts {
    /// Via headers from the target-facing request (one per entry).
    pub vias: Vec<String>,
    pub from: String,
    pub to: String,
    pub call_id: String,
    /// Full CSeq value (`"<number> <METHOD>"`).
    pub cseq: String,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    /// Non-structural headers carried through from the source response (§16.6).
    pub transparent_headers: Vec<SipHeader>,
    /// Record-Route headers reflected verbatim, in received order.
    pub record_routes: Vec<String>,
    pub contact: Option<ContactSpec>,
}

/// Rebuild a B2BUA-side response for relay to a peer leg (RFC 3261 §16.6 /
/// §12.1.1).
pub fn generate_relayed_response(
    status: u16,
    reason: &str,
    opts: &GenerateRelayedResponseOpts,
) -> SipResponse {
    let body = opts.body.clone();

    let mut headers: Vec<SipHeader> = Vec::new();
    for via in &opts.vias {
        headers.push(h("Via", via.clone()));
    }
    for rr in &opts.record_routes {
        headers.push(h("Record-Route", rr.clone()));
    }
    headers.push(h("From", opts.from.clone()));
    headers.push(h("To", opts.to.clone()));
    headers.push(h("Call-ID", opts.call_id.clone()));
    headers.push(h("CSeq", opts.cseq.clone()));

    headers.extend(opts.transparent_headers.iter().cloned());

    if let Some(contact) = &opts.contact {
        headers.push(h("Contact", build_contact_value(contact)));
    }
    append_body_headers(&mut headers, &body, opts.content_type.as_deref());

    make_response(status, reason, headers, body)
}

// ---------------------------------------------------------------------------
// ACK for non-2xx (stack-internal)
// ---------------------------------------------------------------------------

/// Build an ACK for a non-2xx final response inside the INVITE client
/// transaction (RFC 3261 §17.1.1.3). Reuses the INVITE's topmost Via (same
/// branch); copies From / To / Call-ID from the response; CSeq method ACK with
/// the INVITE's sequence number; and reproduces the INVITE's Route headers
/// verbatim ("the Route header fields of the ACK MUST equal" the INVITE's) —
/// load-bearing when the INVITE carried a preloaded outbound-proxy Route
/// (RFC3261-MUST-145, flagged by the cross-message audit).
pub fn generate_ack_for_non_2xx(
    original_invite: &SipRequest,
    final_response: &SipResponse,
) -> SipRequest {
    let via = get_header(&original_invite.headers, "via")
        .expect("generate_ack_for_non_2xx: INVITE missing Via");
    let from = get_header(&final_response.headers, "from").unwrap_or("");
    let to = get_header(&final_response.headers, "to").unwrap_or("");
    let call_id = get_header(&final_response.headers, "call-id").unwrap_or("");
    let cseq_num = original_invite.cseq.seq;

    let mut headers: Vec<SipHeader> = vec![
        h("Via", via),
        h("Max-Forwards", "70"),
        h("From", from),
        h("To", to),
        h("Call-ID", call_id),
        h("CSeq", format!("{cseq_num} ACK")),
    ];
    for route in crate::message_helpers::get_headers(&original_invite.headers, "route") {
        headers.push(h("Route", route));
    }
    headers.push(h("Content-Length", "0"));

    make_request("ACK", &original_invite.uri, headers, Vec::new())
}

/// Build the hop-by-hop ACK a stateless proxy sends downstream when forwarding
/// a 3xx-6xx INVITE final response upstream (RFC 3261 §17.1.1.3 / §17.2.6).
pub fn generate_proxy_ack_for_non_2xx(
    final_response: &SipResponse,
    target: (&str, u16),
    our_branch: &str,
    our_advertised: (&str, u16),
) -> SipRequest {
    let from = get_header(&final_response.headers, "from").unwrap_or("");
    let to = get_header(&final_response.headers, "to").unwrap_or("");
    let call_id = get_header(&final_response.headers, "call-id").unwrap_or("");
    let cseq_num = final_response.cseq.seq;

    let headers: Vec<SipHeader> = vec![
        h(
            "Via",
            format!("SIP/2.0/UDP {}:{};branch={};rport", our_advertised.0, our_advertised.1, our_branch),
        ),
        h("Max-Forwards", "70"),
        h("From", from),
        h("To", to),
        h("Call-ID", call_id),
        h("CSeq", format!("{cseq_num} ACK")),
        h("Content-Length", "0"),
    ];

    make_request("ACK", &format!("sip:{}:{}", target.0, target.1), headers, Vec::new())
}
