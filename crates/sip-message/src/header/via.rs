//! [`Via`] — the hop header, with the whole parameter surface a router needs.
//!
//! Branch, sent-by, `received` and `rport` are accessors on the value, so no
//! consumer has to split a sent-protocol on `/` or scan a parameter list again.

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::cardinality::{held_to_the_table, lines};
use super::name::HeaderId;
use super::params::{read_params, ParamReading, ParamValue, Params};
use super::reading::Reading;
use super::scan::{byte_at, scan_until, skip_ws, sub, sub_trimmed, token_end};
use super::single_valued::Listed;
use super::uri::{read_sent_by, HostPort};
use super::value::HeaderValue;
use super::wire::Wire;
use crate::ip_literal::{ip_host, same_host};
use crate::parser::custom::structured_headers::top_level_comma_entries;

/// The RFC 3261 §8.1.1.7 magic cookie every branch this stack emits starts
/// with.
pub const BRANCH_MAGIC_COOKIE: &str = "z9hG4bK";

/// Whether `branch` starts with the magic cookie, compared ignoring ASCII case
/// as every branch is (RFC 3261 §7.3.1: `via-branch` states no exception).
pub fn has_magic_cookie(branch: &str) -> bool {
    branch
        .get(..BRANCH_MAGIC_COOKIE.len())
        .is_some_and(|lead| lead.eq_ignore_ascii_case(BRANCH_MAGIC_COOKIE))
}

/// Whether two branches name one transaction: compared ignoring ASCII case
/// (RFC 3261 §7.3.1, §17.1.3, §17.2.3).
pub fn same_branch(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Feeds `branch` to `state` as [`same_branch`] compares it, so two branches
/// that are one hash alike.
pub fn hash_branch<H: std::hash::Hasher>(branch: &str, state: &mut H) {
    fold_branch(branch, |chunk| state.write(chunk));
    state.write_u8(0xff);
}

/// Hands `branch` to `feed` folded to lower case, chunk by chunk, folded on
/// the stack: what a digest of a branch reads, as [`same_branch`] compares.
pub fn fold_branch(branch: &str, mut feed: impl FnMut(&[u8])) {
    let mut room = [0u8; 64];
    for chunk in branch.as_bytes().chunks(room.len()) {
        let Some(folded) = room.get_mut(..chunk.len()) else { return };
        for (slot, byte) in folded.iter_mut().zip(chunk) {
            *slot = byte.to_ascii_lowercase();
        }
        feed(folded);
    }
}

/// `branch` folded to lower case as one byte string: in `room` when it fits,
/// allocated only past it.
pub fn folded_branch<'r, const N: usize>(
    branch: &str,
    room: &'r mut [u8; N],
) -> std::borrow::Cow<'r, [u8]> {
    match room.get_mut(..branch.len()) {
        Some(folded) => {
            for (slot, byte) in folded.iter_mut().zip(branch.bytes()) {
                *slot = byte.to_ascii_lowercase();
            }
            std::borrow::Cow::Borrowed(folded)
        }
        None => std::borrow::Cow::Owned(branch.to_ascii_lowercase().into_bytes()),
    }
}

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

    /// Whether the branch carries the RFC 3261 magic cookie, in any case.
    pub fn is_rfc3261_branch(&self) -> bool {
        self.branch().is_some_and(has_magic_cookie)
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
    /// A `received` that is no IP address (§25.1) is no target. `maddr` chooses
    /// no destination (ADR-0001).
    pub fn response_target(&self) -> (&str, u16) {
        let host = self.received().filter(|r| ip_host(r).is_some()).unwrap_or_else(|| self.host());
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

    /// Stamp what the receiving side observed (RFC 3261 §18.2.1, RFC 3581 §4):
    /// `received` is the source address whenever it differs from the sent-by
    /// host, the hop carries `rport` (even when they are equal), or the hop
    /// already carries a `received`; `rport` is the source port whenever the
    /// hop carries one. A value the sender wrote in either is never kept.
    /// Hosts differ as [`same_host`] tells them.
    pub fn stamped_from(mut self, source_host: &str, source_port: u16) -> Self {
        let rport_asked = self.params.get(RPORT.as_str()).is_some();
        if rport_asked || self.received().is_some() || !same_host(self.host(), source_host) {
            self = self.with_received(SipStr::owned(source_host));
        }
        if rport_asked {
            self = self.with_rport(source_port);
        }
        self
    }
}

impl Via {
    /// Read one Via entry (RFC 3261 §20.42, §25.1): `sent-protocol LWS
    /// sent-by *( SEMI via-params )`, `sent-protocol = protocol-name SLASH
    /// protocol-version SLASH transport`, each a token, the via-params held to
    /// `generic-param` with `branch` a token and `received` also an
    /// unbracketed `IPv6address`. [`Reading::Lenient`] keeps what it can: an
    /// empty part, a sent-by no host reader takes held whole, the parameters
    /// read on past a byte off their grammar.
    pub(crate) fn read(raw: &SipStr, reading: Reading) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        let strict = reading == Reading::Strict;
        let refused =
            |reason: &str| SipParseError::new(format!("Via {reason}: {:?}", value.as_str()));
        let bytes = value.as_bytes();
        let (protocol, i) = protocol_part(&value, 0, reading, false);
        if strict && protocol.is_empty() {
            return Err(refused("has an empty protocol-name"));
        }
        if strict && byte_at(bytes, i) != Some(b'/') {
            return Err(refused("has no `/` after its protocol-name"));
        }
        let (version, i) = protocol_part(&value, i + 1, reading, false);
        if strict && version.is_empty() {
            return Err(refused("has an empty protocol-version"));
        }
        if strict && byte_at(bytes, i) != Some(b'/') {
            return Err(refused("has no `/` after its protocol-version"));
        }
        let (transport, after_transport) = protocol_part(&value, i + 1, reading, true);
        if strict && transport.is_empty() {
            return Err(refused("has an empty transport"));
        }
        let host_at = skip_ws(bytes, after_transport);
        if strict && host_at == after_transport {
            return Err(refused("transport is not a token followed by LWS"));
        }
        let read = read_sent_by(&value, host_at).and_then(|(host, port, after)| {
            let narrow = port.map(u16::try_from).transpose();
            let port_text = port.map(|p| p.to_string()).unwrap_or_default();
            narrow
                .map(|port| (HostPort::new(host, port), after))
                .map_err(|_| format!("port out of range: {port_text}"))
        });
        let (sent_by, params_at) = match read {
            Ok(read) => read,
            Err(reason) if strict => return Err(refused(&format!("sent-by: {reason}"))),
            Err(_) => {
                let end = scan_until(bytes, host_at, b";");
                (HostPort::whole(sub_trimmed(&value, host_at, end)), end)
            }
        };
        let param_reading = if strict { ParamReading::Strict } else { ParamReading::Lenient };
        let (params, _) = read_params(&value, params_at, param_reading, Some("received"))
            .map_err(|e| refused(&e.reason))?;
        if strict && params.get("branch").is_some_and(|b| !matches!(b, ParamValue::Token(_))) {
            return Err(refused("branch is not a token"));
        }
        Ok(Self { protocol, version, transport, sent_by, params })
    }
}

/// One part of a sent-protocol from byte `i`; yields it and the position
/// after it. Strict, a token, the position past the LWS that follows unless it
/// is the `last` part, whose LWS the sent-by needs; lenient, every byte up to
/// a `/`, or LWS, `;` or `,` for the `last` part, trimmed.
fn protocol_part(value: &SipStr, i: usize, reading: Reading, last: bool) -> (SipStr, usize) {
    let bytes = value.as_bytes();
    let at = skip_ws(bytes, i);
    match reading {
        Reading::Strict => {
            let end = token_end(bytes, at);
            (sub(value, at, end), if last { end } else { skip_ws(bytes, end) })
        }
        Reading::Lenient => {
            let end = scan_until(bytes, at, if last { b" \t;," } else { b"/" });
            (sub_trimmed(value, at, end), end)
        }
    }
}

impl Via {
    /// The first hop one Via line states, the line held as written: a line may
    /// fold several hops with commas (RFC 3261 §7.3.1), so the first entry is
    /// read, strictly, and the ones after it are not.
    pub fn first_of_line(line: &SipStr) -> Result<Self, SipParseError> {
        let first = top_level_comma_entries(line.as_str()).next().unwrap_or_default();
        Self::parse(&line.reslice(first))
    }
}

impl HeaderValue for Via {
    type Lines = lines::Comma;
    held_to_the_table!();

    const HEADER: Listed = Listed::Known(HeaderId::Via);

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        Self::read(raw, Reading::Strict)
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

    /// `received` is an address (RFC 3261 §25.1): a name there is no target
    /// to resolve, and the sent-by stands.
    #[test]
    fn a_received_that_is_no_address_leaves_the_sent_by() {
        let v = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1;received=host.invalid");
        assert_eq!(v.response_target(), ("10.0.0.1", 5060));
        let v6 = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1;received=2001:db8::7");
        assert_eq!(v6.response_target(), ("2001:db8::7", 5060));
    }

    #[test]
    fn stamping_adds_received_when_the_source_differs_or_rport_asks() {
        let asked = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z1;rport");
        assert_eq!(
            asked.clone().stamped_from("192.0.2.7", 33000).to_wire(),
            "SIP/2.0/UDP 10.0.0.1:5060;branch=z1;rport=33000;received=192.0.2.7"
        );
        assert_eq!(
            asked.stamped_from("10.0.0.1", 33000).to_wire(),
            "SIP/2.0/UDP 10.0.0.1:5060;branch=z1;rport=33000;received=10.0.0.1"
        );
        let unasked = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z1");
        assert_eq!(
            unasked.stamped_from("10.0.0.1", 33000).to_wire(),
            "SIP/2.0/UDP 10.0.0.1:5060;branch=z1"
        );
    }

    /// RFC 3581 §4: the `rport` a response carries is the port the request
    /// came from, whatever value the sender wrote there.
    #[test]
    fn stamping_replaces_an_rport_the_sender_wrote() {
        let stamped = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z1;rport=1234")
            .stamped_from("198.51.100.4", 33000);
        assert_eq!(stamped.rport(), Some(Rport::Observed(33000)));
        assert_eq!(stamped.received(), Some("198.51.100.4"));
        let unreadable =
            via("SIP/2.0/UDP 10.0.0.1:5060;branch=z1;rport=x").stamped_from("10.0.0.1", 33000);
        assert_eq!(unreadable.rport(), Some(Rport::Observed(33000)));
        assert_eq!(unreadable.response_target(), ("10.0.0.1", 33000));
    }

    #[test]
    fn stamping_replaces_a_received_the_sender_wrote() {
        let stamped = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z1;received=192.0.2.7")
            .stamped_from("198.51.100.4", 33000);
        assert_eq!(stamped.received(), Some("198.51.100.4"));
        let same_host = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z1;received=evil.example")
            .stamped_from("10.0.0.1", 33000);
        assert_eq!(same_host.response_target(), ("10.0.0.1", 5060));
    }

    #[test]
    fn a_hop_is_its_own_builder() {
        assert_eq!(
            Via::udp("proxy.example", 5080).with_branch("z9hG4bKabc").requesting_rport().to_wire(),
            "SIP/2.0/UDP proxy.example:5080;branch=z9hG4bKabc;rport"
        );
    }

    /// A line folding two hops yields its first; the whole line is no one hop.
    #[test]
    fn the_first_hop_of_a_folded_line_reads() {
        let line = SipStr::owned(
            "SIP/2.0/UDP 192.0.2.10:5070;branch=z9hG4bK-a, SIP/2.0/UDP 192.0.2.20;branch=z9hG4bK-b",
        );
        assert_eq!(Via::first_of_line(&line).unwrap().host_port(), ("192.0.2.10", 5070));
        assert!(Via::parse(&line).is_err());
    }

    /// The one reader holds a hop to §25.1 whole: the transport a token then
    /// LWS, the branch a token, every parameter once, nothing after them.
    #[test]
    fn a_hop_off_the_grammar_is_refused() {
        for raw in [
            "SIP/2.0/UDP/foo 192.0.2.11;branch=z9hG4bK-b",
            "SIP/2.0/X(y) 192.0.2.11;branch=z9hG4bK-b",
            "SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-a garbage;rport",
            "SIP/2.0/UDP 192.0.2.10;branch=\"z9hG4bK-a\"",
            "SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-a;branch=z9hG4bK-a",
            "SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-a;x=\"abc",
            "SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-a;a(b=c",
        ] {
            assert!(Via::parse(&SipStr::owned(raw)).is_err(), "{raw}");
        }
        let lenient = Via::read(
            &SipStr::owned("SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-a garbage;rport"),
            Reading::Lenient,
        )
        .unwrap();
        assert_eq!(lenient.rport(), Some(Rport::Requested));
    }

    #[test]
    fn a_via_without_a_sent_by_is_rejected() {
        assert!(Via::parse(&SipStr::owned("SIP/2.0/UDP")).is_err());
        assert!(Via::parse(&SipStr::owned("SIP/2.0")).is_err());
    }
}
