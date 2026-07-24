//! Input shapes the generators consume: transport tag, Via / Contact
//! serialization specs, and the minimal dialog / INVITE-transaction views.
//! Reading these headers back off a parsed message does NOT live here — see
//! [`crate::message_helpers`].

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
    /// Serialize into a Via header value. Custom params are appended
    /// verbatim; an empty value serialises as a flag (RFC 3581 §3).
    pub(crate) fn header_value(&self) -> String {
        let mut out = format!(
            "SIP/2.0/{} {}:{};branch={}",
            self.transport.as_str(),
            self.local_ip,
            self.local_port,
            self.branch
        );
        for (k, val) in &self.custom_params {
            if val.is_empty() {
                out.push_str(&format!(";{k}"));
            } else {
                out.push_str(&format!(";{k}={val}"));
            }
        }
        out
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
    /// Serialize into an angle-bracketed Contact header value.
    pub(crate) fn header_value(&self) -> String {
        let mut uri = format!("sip:{}@{}:{}", self.user, self.host, self.port);
        for (k, val) in &self.uri_params {
            uri.push_str(&format!(";{k}={val}"));
        }
        format!("<{uri}>")
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
