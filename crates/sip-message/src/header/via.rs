//! [`Via`] — the hop header, with the whole parameter surface a router needs.
//!
//! Branch, sent-by, `received` and `rport` are accessors on the value, so no
//! consumer has to split a sent-protocol on `/` or scan a parameter list again.

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::name::HeaderName;
use super::params::{parse_semicolon_params, ParamValue, Params, HEADER_PARAMS};
use super::scan::{scan_until, skip_ws, sub, sub_trimmed};
use super::uri::HostPort;
use super::value::{Folding, HeaderValue};
use super::wire::Wire;

/// The RFC 3261 §8.1.1.7 magic cookie every branch this stack emits starts
/// with.
pub const BRANCH_MAGIC_COOKIE: &str = "z9hG4bK";

/// The parameter names a router writes, spelled once so stamping a hop copies
/// no name text.
const BRANCH: SipStr = SipStr::from_static("branch");
const RECEIVED: SipStr = SipStr::from_static("received");
const RPORT: SipStr = SipStr::from_static("rport");

/// What a Via's `rport` parameter says (RFC 3581).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rport {
    /// `;rport` bare — the sender asks the next hop to fill it in.
    Requested,
    /// `;rport=<port>` — the source port the next hop observed.
    Observed(u16),
}

/// One Via hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Via {
    protocol: SipStr,
    version: SipStr,
    transport: SipStr,
    sent_by: HostPort,
    params: Params,
}

impl Via {
    /// A `SIP/2.0/<transport>` hop.
    pub fn new(transport: impl Into<SipStr>, sent_by: HostPort) -> Self {
        Self {
            protocol: SipStr::from_static("SIP"),
            version: SipStr::from_static("2.0"),
            transport: transport.into(),
            sent_by,
            params: Params::new(),
        }
    }

    pub fn udp(host: impl Into<SipStr>, port: u16) -> Self {
        Self::new(SipStr::from_static("UDP"), HostPort::new(host, Some(port)))
    }

    pub fn tcp(host: impl Into<SipStr>, port: u16) -> Self {
        Self::new(SipStr::from_static("TCP"), HostPort::new(host, Some(port)))
    }

    pub fn tls(host: impl Into<SipStr>, port: u16) -> Self {
        Self::new(SipStr::from_static("TLS"), HostPort::new(host, Some(port)))
    }

    pub fn protocol(&self) -> &str {
        self.protocol.as_str()
    }

    pub fn version(&self) -> &str {
        self.version.as_str()
    }

    pub fn transport(&self) -> &str {
        self.transport.as_str()
    }

    /// Where a response to this hop goes, before `received`/`rport` correction.
    pub fn sent_by(&self) -> &HostPort {
        &self.sent_by
    }

    pub fn host(&self) -> &str {
        self.sent_by.host()
    }

    pub fn port(&self) -> Option<u16> {
        self.sent_by.port()
    }

    pub fn host_port(&self) -> (&str, u16) {
        self.sent_by.pair()
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn param(&self, name: &str) -> Option<&ParamValue> {
        self.params.get(name)
    }

    /// The transaction branch.
    pub fn branch(&self) -> Option<&str> {
        self.params.value("branch")
    }

    /// Whether the branch carries the RFC 3261 magic cookie.
    pub fn is_rfc3261_branch(&self) -> bool {
        self.branch().is_some_and(|b| b.starts_with(BRANCH_MAGIC_COOKIE))
    }

    /// The source address the previous hop was seen from (RFC 3261 §18.2.1).
    pub fn received(&self) -> Option<&str> {
        self.params.value("received")
    }

    pub fn rport(&self) -> Option<Rport> {
        match self.params.get("rport")? {
            ParamValue::Flag => Some(Rport::Requested),
            v => v.as_str()?.parse::<u16>().ok().map(Rport::Observed),
        }
    }

    pub fn maddr(&self) -> Option<&str> {
        self.params.value("maddr")
    }

    /// The address a response to this hop is actually sent to: `received` (and
    /// an observed `rport`) override the sent-by (RFC 3261 §18.2.2, RFC 3581).
    pub fn response_target(&self) -> (&str, u16) {
        let host = self.received().unwrap_or_else(|| self.host());
        let port = match self.rport() {
            Some(Rport::Observed(p)) => p,
            _ => self.sent_by.port_or_default(),
        };
        (host, port)
    }

    pub fn with_branch(self, branch: impl Into<SipStr>) -> Self {
        self.with_param(BRANCH, ParamValue::Token(branch.into()))
    }

    pub fn with_received(self, host: impl Into<SipStr>) -> Self {
        self.with_param(RECEIVED, ParamValue::Token(host.into()))
    }

    /// Ask the next hop to report the source port back (`;rport`).
    pub fn requesting_rport(self) -> Self {
        self.with_param(RPORT, ParamValue::Flag)
    }

    pub fn with_rport(self, port: u16) -> Self {
        self.with_param(RPORT, ParamValue::Token(SipStr::owned(&port.to_string())))
    }

    pub fn with_param(mut self, name: impl Into<SipStr>, value: ParamValue) -> Self {
        self.params.set(name, value);
        self
    }

    pub fn without_param(mut self, name: &str) -> Self {
        self.params.remove(name);
        self
    }

    /// Stamp what the receiving side observed: `received` whenever the source
    /// address differs from the sent-by host, and `rport` only when the sender
    /// asked for it (RFC 3261 §18.2.1, RFC 3581 §4). A `received` another hop
    /// already recorded stands — stamping is idempotent, never corrective.
    pub fn stamped_from(mut self, source_host: &str, source_port: u16) -> Self {
        if self.received().is_none() && self.host() != source_host {
            self = self.with_received(SipStr::owned(source_host));
        }
        if matches!(self.rport(), Some(Rport::Requested)) {
            self = self.with_rport(source_port);
        }
        self
    }
}

impl HeaderValue for Via {
    fn header_name() -> HeaderName {
        HeaderName::Via
    }

    fn folding() -> Folding {
        Folding::Comma
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        let bytes = value.as_bytes();
        let mut i = skip_ws(bytes, 0);

        let proto_end = scan_until(bytes, i, b"/");
        if proto_end >= bytes.len() {
            return Err(SipParseError::new(format!(
                "Via has no sent-protocol: {:?}",
                value.as_str()
            )));
        }
        let protocol = sub_trimmed(&value, i, proto_end);
        i = proto_end + 1;

        let ver_end = scan_until(bytes, i, b"/");
        if ver_end >= bytes.len() {
            return Err(SipParseError::new(format!(
                "Via has no transport: {:?}",
                value.as_str()
            )));
        }
        let version = sub_trimmed(&value, i, ver_end);
        i = skip_ws(bytes, ver_end + 1);

        let trans_end = scan_until(bytes, i, b" \t;,");
        let transport = sub(&value, i, trans_end);
        if transport.is_empty() {
            return Err(SipParseError::new(format!(
                "Via has an empty transport: {:?}",
                value.as_str()
            )));
        }

        let host_at = skip_ws(bytes, trans_end);
        let (sent_by, after_host) = HostPort::parse_at(&value, host_at)?;
        if sent_by.host().is_empty() {
            return Err(SipParseError::new(format!(
                "Via has an empty sent-by: {:?}",
                value.as_str()
            )));
        }
        let (params, _) = parse_semicolon_params(&value, after_host, &HEADER_PARAMS);

        Ok(Self { protocol, version, transport, sent_by, params })
    }

    fn render(&self, out: &mut Wire) {
        out.str(self.protocol.as_str());
        out.byte(b'/');
        out.str(self.version.as_str());
        out.byte(b'/');
        out.str(self.transport.as_str());
        out.byte(b' ');
        self.sent_by.render(out);
        self.params.render(out);
    }
}

impl std::fmt::Display for Via {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_wire())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn via(s: &str) -> Via {
        Via::parse(&SipStr::owned(s)).unwrap_or_else(|e| panic!("{s}: {}", e.reason))
    }

    #[test]
    fn every_part_of_a_hop_is_readable() {
        let v = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776;received=192.0.2.7;rport=5061");
        assert_eq!(v.transport(), "UDP");
        assert_eq!(v.host_port(), ("10.0.0.1", 5060));
        assert_eq!(v.branch(), Some("z9hG4bK776"));
        assert!(v.is_rfc3261_branch());
        assert_eq!(v.received(), Some("192.0.2.7"));
        assert_eq!(v.rport(), Some(Rport::Observed(5061)));
        assert_eq!(v.response_target(), ("192.0.2.7", 5061));
    }

    #[test]
    fn a_plain_hop_answers_at_its_sent_by() {
        assert_eq!(via("SIP/2.0/TCP host.example").response_target(), ("host.example", 5060));
    }

    #[test]
    fn stamping_adds_received_only_when_the_source_differs() {
        let asked = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z1;rport");
        assert_eq!(
            asked.clone().stamped_from("192.0.2.7", 33000).to_wire(),
            "SIP/2.0/UDP 10.0.0.1:5060;branch=z1;rport=33000;received=192.0.2.7"
        );
        assert_eq!(
            asked.stamped_from("10.0.0.1", 33000).to_wire(),
            "SIP/2.0/UDP 10.0.0.1:5060;branch=z1;rport=33000"
        );
    }

    #[test]
    fn stamping_leaves_a_received_another_hop_recorded() {
        let stamped = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z1;received=192.0.2.7")
            .stamped_from("198.51.100.4", 33000);
        assert_eq!(stamped.received(), Some("192.0.2.7"));
    }

    #[test]
    fn a_hop_is_its_own_builder() {
        assert_eq!(
            Via::udp("proxy.example", 5080).with_branch("z9hG4bKabc").requesting_rport().to_wire(),
            "SIP/2.0/UDP proxy.example:5080;branch=z9hG4bKabc;rport"
        );
    }

    #[test]
    fn a_via_without_a_sent_by_is_rejected() {
        assert!(Via::parse(&SipStr::owned("SIP/2.0/UDP")).is_err());
        assert!(Via::parse(&SipStr::owned("SIP/2.0")).is_err());
    }
}
