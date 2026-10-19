//! Reading the `call` crate's text-typed dialog state back into typed values.
//! `call` keeps dialog identity, targets and route sets as text (it has no
//! sip-message dependency by design, ADR-0008); these are the bridges back.
//! Decision-supplied address *refusal* does NOT live here — see
//! [`super::address`] (dialog text was already admitted by the inbound parser).

use call::{Dialog, Leg};
use sip_message::generators;
use sip_message::header::{HeaderValue, HostPort, NameAddr};
use sip_message::{SipRequest, SipStr};

/// The generators' `StackDialog` input for dialog `d` of `leg`: the Call-ID
/// and the two addresses are the leg's, the rest the dialog's.
pub fn to_gen_dialog(leg: &Leg, d: &Dialog) -> generators::StackDialog {
    generators::StackDialog {
        call_id: leg.call_id.clone(),
        local_tag: d.our_tag.clone(),
        remote_tag: d.peer_tag.clone(),
        local_uri: leg.local_uri.clone().unwrap_or_default(),
        remote_uri: leg.remote_uri.clone().unwrap_or_default(),
        remote_target: d.remote_target.clone(),
        local_cseq: wire_cseq(d.our_cseq),
        route_set: d.route_set.clone(),
    }
}

/// A dialog sequence number `call` keeps as an `i64`, as a CSeq header
/// carries it: RFC 3261 §8.1.1.5 keeps a CSeq below 2³¹, which a `u32` holds;
/// a number outside it saturates at the nearer end.
pub fn wire_cseq(seq: i64) -> u32 {
    u32::try_from(seq.max(0)).unwrap_or(u32::MAX)
}

/// The local and remote address of the UAS dialog `invite` creates: its To and
/// its From (RFC 3261 §12.1.1), each written whole but for the tag, so the
/// requests the dialog later carries keep the display names the INVITE stated.
pub fn uas_addresses(invite: &SipRequest) -> (String, String) {
    (invite.to().clone().without_tag().to_wire(), invite.from().clone().without_tag().to_wire())
}

/// The transport destination the address `target` names. The `call` crate keeps
/// dialog targets and route sets as text (it has no sip-message dependency by
/// design, ADR-0008), so reading one back is a parse; an address no reader
/// accepts falls back to its own text on the default port, and says so — the
/// fallback resolves a host nothing routes to, so it must not pass unnamed.
pub fn target_dest(target: &str) -> (String, u16) {
    match NameAddr::parse(&SipStr::owned(target)) {
        Ok(addr) => {
            let (host, port) = addr.uri().host_port();
            (host.to_string(), port)
        }
        Err(err) => {
            if !target.trim().is_empty() {
                tracing::warn!(%target, error = %err, "dialog target does not read; resolving it as a host name");
            }
            (target.trim().to_string(), HostPort::DEFAULT_PORT)
        }
    }
}
