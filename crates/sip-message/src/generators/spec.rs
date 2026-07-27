//! Input shapes the generators consume: transport tag, Via / Contact
//! specs, and the minimal dialog / INVITE-transaction views. Reading these
//! headers back off a parsed message is the message's own typed surface, not
//! this module's concern.

use crate::header::{Contact, HostPort, ParamValue, Uri, Via};
use crate::sip_str::SipStr;

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

impl ViaSpec {
    /// The Via value this spec describes. Custom params keep their order; an
    /// empty value is a flag (RFC 3581 §3).
    pub fn value(&self) -> Via {
        let mut via = Via::new(
            SipStr::from_static(self.transport.as_str()),
            HostPort::new(SipStr::owned(&self.local_ip), Some(self.local_port)),
        )
        .with_branch(SipStr::owned(&self.branch));
        for (name, value) in &self.custom_params {
            let param = if value.is_empty() {
                ParamValue::Flag
            } else {
                ParamValue::Token(SipStr::owned(value))
            };
            via = via.with_param(SipStr::owned(name), param);
        }
        via
    }
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

impl ContactSpec {
    /// The Contact value this spec describes — a `sip:` URI whose parameters
    /// sit inside the angle brackets, where they bind to the URI and not to the
    /// header.
    pub fn value(&self) -> Contact {
        let mut uri = Uri::sip_user(SipStr::owned(&self.user), SipStr::owned(&self.host))
            .with_port(self.port);
        for (name, value) in &self.uri_params {
            uri = uri.with_param(SipStr::owned(name), ParamValue::Token(SipStr::owned(value)));
        }
        Contact::from_uri(uri)
    }
}

/// Minimal dialog shape the in-dialog generators read — deliberately
/// decoupled from any richer dialog type; callers project into it.
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

/// Minimal INVITE client-transaction view the CANCEL / ACK-for-2xx
/// generators read: only the original INVITE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteClientTransactionHandle {
    pub original_invite: crate::types::SipRequest,
}
