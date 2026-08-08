//! Reading the `call` crate's text-typed dialog state back into typed values.
//! `call` keeps dialog identity, targets and route sets as text (it has no
//! sip-message dependency by design, ADR-0008); these are the bridges back.
//! Decision-supplied address *refusal* does NOT live here — see
//! [`super::address`] (dialog text was already admitted by the inbound parser).

use call::StackDialog;
use sip_message::generators;
use sip_message::header::{HostPort, NameAddr};
use sip_message::SipStr;

/// Convert a `call` dialog to the generators' `StackDialog` input shape.
pub fn to_gen_dialog(d: &StackDialog) -> generators::StackDialog {
    generators::StackDialog {
        call_id: d.call_id.clone(),
        local_tag: d.local_tag.clone(),
        remote_tag: d.remote_tag.clone(),
        local_uri: d.local_uri.clone(),
        remote_uri: d.remote_uri.clone(),
        remote_target: d.remote_target.clone(),
        local_cseq: d.local_cseq.max(0) as u32,
        route_set: d.route_set.clone(),
    }
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
