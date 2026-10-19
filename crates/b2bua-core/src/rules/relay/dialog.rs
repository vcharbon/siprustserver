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
        local_cseq: d.our_cseq.max(0) as u32,
        route_set: d.route_set.clone(),
    }
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
