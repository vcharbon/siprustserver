//! Residual header/URI readers the harness routes and addresses with.
//!
//! FIXME(scenario-harness): SIP header/URI extraction belongs in `sip-message`
//! (CLAUDE.md hard rule) — replace these with
//! `sip_message::message_helpers::{via, name_addr, uri}` readers and keep only
//! the SocketAddr resolution / next-hop policy here.

use std::net::SocketAddr;

use sip_message::generators::{strip_route_uri_to_request_uri, StackDialog};
use sip_message::message_helpers::get_header;
use sip_message::{SipHeader, SipRequest, SipResponse};

/// The `branch` parameter of the topmost Via header, if any.
pub(crate) fn top_via_branch(headers: &[SipHeader]) -> Option<String> {
    let via = get_header(headers, "via")?;
    via.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("branch").then(|| v.trim().to_string())
    })
}

/// Unwrap a `<uri>` name-addr / Route value to its bare URI (params after `>`
/// dropped); a bare URI passes through trimmed.
pub(super) fn unwrap_angle(value: &str) -> String {
    let t = value.trim();
    match (t.find('<'), t.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => t[a + 1..b].to_string(),
        _ => t.to_string(),
    }
}

/// The first Contact URI on a response, unwrapped from `<...>`. Used to learn
/// the dialog remote target.
pub(super) fn first_contact_uri(resp: &SipResponse) -> Option<String> {
    get_header(&resp.headers, "contact").map(unwrap_angle)
}

/// The `RAck` value acknowledging a reliable provisional (RFC 3262 §7.2):
/// `<RSeq> <CSeq-num> <CSeq-method>`, all read off the 1xx itself. `None` when
/// the response carries no parseable `RSeq` (it is not a reliable provisional).
pub(super) fn rack_for(reliable_1xx: &SipResponse) -> Option<String> {
    let rseq: u64 = get_header(&reliable_1xx.headers, "rseq")?.trim().parse().ok()?;
    Some(format!("{rseq} {} {}", reliable_1xx.cseq.seq, reliable_1xx.cseq.method))
}

/// Resolve a SIP URI to a socket address (default port 5060, IPv4 fixtures
/// only). Handles `sip:user@host:port`, the userless `sip:host:port;lr` form
/// of a Route/Record-Route URI, and a bare `host:port`.
pub(super) fn uri_to_addr(uri: &str) -> Option<SocketAddr> {
    let no_scheme = uri
        .strip_prefix("sips:")
        .or_else(|| uri.strip_prefix("sip:"))
        .unwrap_or(uri);
    // Host part is whatever follows the last '@' (none → the whole thing).
    let host_part = no_scheme.rsplit('@').next()?;
    let host_port = host_part.split([';', '?']).next()?.trim();
    hostport_to_addr(host_port)
}

/// Parse a bare `host:port` (or `host`, default port 5060) to a socket address.
pub(super) fn hostport_to_addr(host_port: &str) -> Option<SocketAddr> {
    if let Ok(sa) = host_port.parse::<SocketAddr>() {
        return Some(sa);
    }
    format!("{host_port}:5060").parse().ok()
}

/// The wire destination for an in-dialog request: the first hop in the route
/// set (the proxy) when present, else the dialog's remote target. For both
/// loose and strict routing the next hop is the address of `route_set[0]`'s
/// URI; with no route set it is the remote target.
pub(super) fn next_hop(dialog: &StackDialog, fallback: SocketAddr) -> SocketAddr {
    if let Some(top) = dialog.route_set.first() {
        if let Some(addr) = uri_to_addr(&strip_route_uri_to_request_uri(top)) {
            return addr;
        }
    }
    uri_to_addr(&dialog.remote_target).unwrap_or(fallback)
}

/// The address a response to `req` must be sent to: the topmost Via's sent-by
/// (RFC 3261 §18.2.2). (`received=`/`rport=` are not stamped by this harness's
/// `generate_response`, so the sent-by host:port is authoritative here.)
pub(super) fn top_via_addr(req: &SipRequest) -> Option<SocketAddr> {
    let via = get_header(&req.headers, "via")?;
    // "SIP/2.0/UDP host:port;branch=…" → take the token after the transport,
    // before the first ';'.
    let after_transport = via.split_whitespace().nth(1)?;
    let sent_by = after_transport.split(';').next()?.trim();
    hostport_to_addr(sent_by)
}
