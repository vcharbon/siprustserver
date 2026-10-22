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

/// The dialog a received request rides: its To-tag is ours, its From-tag
/// the peer's (RFC 3261 §12). `None` for a request with no From tag.
pub fn received_on(req: &SipRequest) -> Option<call::DialogId> {
    Some(call::DialogId {
        our_tag: req.to().tag().unwrap_or_default().to_string(),
        peer_tag: req.from().tag()?.to_string(),
    })
}

/// A dialog sequence number `call` keeps as an `i64`, as a CSeq header
/// carries it (RFC 3261 §8.1.1.5 keeps a CSeq below 2³¹, which a `u32`
/// holds): a negative number reads as `0`, one past `u32::MAX` as `u32::MAX`.
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
/// accepts, or whose scheme names no host, falls back to its own text on the
/// default port, and says so — the fallback resolves a host nothing routes
/// to, so it must not pass unnamed.
pub fn target_dest(target: &str) -> (String, u16) {
    let read = NameAddr::parse(&SipStr::owned(target)).map_err(|e| e.reason).and_then(|addr| {
        match addr.uri().opaque_part() {
            None => Ok(addr),
            Some(_) => Err(format!("a {} URI names no host", addr.uri().scheme())),
        }
    });
    match read {
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

#[cfg(test)]
mod tests {
    use super::target_dest;

    #[test]
    fn a_target_names_its_host_and_port() {
        assert_eq!(target_dest("<sip:bob@192.0.2.4:5070>"), ("192.0.2.4".to_string(), 5070));
    }

    /// A target whose scheme names no host resolves to no part of its text.
    #[test]
    fn a_target_of_an_opaque_scheme_names_no_host() {
        for target in ["<urn:service:sos>", "<http://example.com/a:b>"] {
            assert_eq!(target_dest(target), (target.to_string(), 5060));
        }
    }
}
