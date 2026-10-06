//! A Via sent-by as RFC 3261 §17.2.3 compares it when matching a request to a
//! server transaction: the host case-insensitively (§19.1.4), the port as
//! written — an omitted port does not equal an explicit default one, as
//! §19.1.4 holds for a URI's components. The `received`, `rport` and every
//! other Via parameter are no part of it (§18.2.1 adds them on the way in),
//! and neither is the transport.

use std::fmt;
use std::hash::{Hash, Hasher};

use super::via::Via;

/// A borrowed sent-by, read off a [`Via`] with [`Via::sent_by_ref`].
#[derive(Debug, Clone, Copy)]
pub struct SentByRef<'a> {
    host: &'a str,
    port: Option<u16>,
}

impl<'a> SentByRef<'a> {
    pub fn host(&self) -> &'a str {
        self.host
    }

    /// The port as written, `None` when the Via omits it.
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Whether `other` names the same host (case-insensitively, §19.1.4),
    /// whatever its port: a UA on TCP may name a new ephemeral port per
    /// connection (§18.1.1), so the host is what names the element.
    pub fn same_host(&self, other: &SentByRef<'_>) -> bool {
        self.host.eq_ignore_ascii_case(other.host)
    }

    /// An owned copy, for state that outlives the message.
    pub fn to_sent_by(self) -> SentBy {
        SentBy { host: self.host.to_string(), port: self.port }
    }

    /// Write the form `eq` compares: `host[:port]`, the host lowercased, an
    /// IPv6 reference in brackets, the port only when written. Two sent-bys
    /// write the same text exactly when they are equal, so the text can key
    /// state that matches them.
    pub fn write_canonical<W: fmt::Write>(&self, out: &mut W) -> fmt::Result {
        let v6 = self.host.contains(':');
        if v6 {
            out.write_char('[')?;
        }
        for c in self.host.chars() {
            out.write_char(c.to_ascii_lowercase())?;
        }
        if v6 {
            out.write_char(']')?;
        }
        match self.port {
            Some(port) => write!(out, ":{port}"),
            None => Ok(()),
        }
    }
}

impl PartialEq for SentByRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.port == other.port && self.host.eq_ignore_ascii_case(other.host)
    }
}

impl Eq for SentByRef<'_> {}

/// Consistent with the case-insensitive host of `eq`.
impl Hash for SentByRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for b in self.host.bytes() {
            state.write_u8(b.to_ascii_lowercase());
        }
        state.write_u8(0xff);
        self.port.hash(state);
    }
}

/// An owned sent-by. It pins no message image.
#[derive(Debug, Clone)]
pub struct SentBy {
    host: String,
    port: Option<u16>,
}

impl SentBy {
    /// The sent-by of `via`.
    pub fn of(via: &Via) -> Self {
        via.sent_by_ref().to_sent_by()
    }

    pub fn as_borrowed(&self) -> SentByRef<'_> {
        SentByRef { host: &self.host, port: self.port }
    }
}

impl PartialEq for SentBy {
    fn eq(&self, other: &Self) -> bool {
        self.as_borrowed() == other.as_borrowed()
    }
}

impl Eq for SentBy {}

impl Hash for SentBy {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_borrowed().hash(state);
    }
}

impl Via {
    /// This hop's sent-by as §17.2.3 compares it ([`SentByRef`]).
    pub fn sent_by_ref(&self) -> SentByRef<'_> {
        SentByRef { host: self.host(), port: self.port() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::HeaderValue;
    use crate::sip_str::SipStr;

    fn via(line: &str) -> Via {
        Via::parse(&SipStr::owned(line)).expect("a Via value")
    }

    #[test]
    fn host_case_received_and_rport_do_not_change_it() {
        let sent = via("SIP/2.0/UDP Host.Example.com:5070;branch=z9hG4bK1");
        for other in [
            "SIP/2.0/UDP host.example.COM:5070;branch=z9hG4bK1",
            "SIP/2.0/UDP host.example.com:5070;branch=z9hG4bK1;rport=4000;received=192.0.2.9",
            "SIP/2.0/TCP host.example.com:5070;branch=z9hG4bK1",
        ] {
            assert_eq!(sent.sent_by_ref(), via(other).sent_by_ref(), "{other}");
        }
    }

    #[test]
    fn same_host_ignores_the_port_and_the_host_case() {
        let sent = via("SIP/2.0/TCP UA.example:49152;branch=z9hG4bK1");
        let again = via("SIP/2.0/TCP ua.EXAMPLE:49170;branch=z9hG4bK2");
        assert!(sent.sent_by_ref().same_host(&again.sent_by_ref()));
        assert!(!sent.sent_by_ref().same_host(&via("SIP/2.0/TCP ub.example:49152").sent_by_ref()));
    }

    #[test]
    fn another_host_or_port_is_another_sent_by() {
        let sent = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1");
        for other in [
            "SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bK1",
            "SIP/2.0/UDP 10.0.0.1:5061;branch=z9hG4bK1",
            "SIP/2.0/UDP 10.0.0.1;branch=z9hG4bK1",
        ] {
            assert_ne!(sent.sent_by_ref(), via(other).sent_by_ref(), "{other}");
        }
    }

    fn canonical(line: &str) -> String {
        let mut out = String::new();
        via(line).sent_by_ref().write_canonical(&mut out).expect("a String takes the text");
        out
    }

    #[test]
    fn the_canonical_text_is_equal_exactly_when_the_sent_bys_are() {
        let sent = "SIP/2.0/UDP Host.Example.com:5070;branch=z9hG4bK1";
        assert_eq!(canonical(sent), "host.example.com:5070");
        assert_eq!(canonical(sent), canonical("SIP/2.0/UDP host.example.COM:5070;rport"));
        for other in [
            "SIP/2.0/UDP host.example.com:5071;branch=z9hG4bK1",
            "SIP/2.0/UDP host.example.com;branch=z9hG4bK1",
            "SIP/2.0/UDP host.example.org:5070;branch=z9hG4bK1",
        ] {
            assert_ne!(canonical(sent), canonical(other), "{other}");
        }
        assert_eq!(
            canonical("SIP/2.0/UDP [2001:DB8::1]:5070;branch=z9hG4bK1"),
            "[2001:db8::1]:5070"
        );
        assert_ne!(
            canonical("SIP/2.0/UDP [2001:db8::1]:5070;branch=z9hG4bK1"),
            canonical("SIP/2.0/UDP [2001:db8::1:5070];branch=z9hG4bK1"),
        );
    }

    #[test]
    fn the_owned_copy_compares_as_the_borrowed_one() {
        let sent = via("SIP/2.0/UDP [2001:db8::1]:5070;branch=z9hG4bK1");
        let owned = SentBy::of(&sent);
        assert_eq!(owned.as_borrowed(), sent.sent_by_ref());
        assert_eq!(owned.as_borrowed().host(), "2001:db8::1");
    }

    #[test]
    fn equal_sent_bys_hash_alike() {
        use std::hash::BuildHasher;
        let hasher = std::collections::hash_map::RandomState::new();
        let a = via("SIP/2.0/UDP Host.Example.com:5070;branch=z9hG4bK1");
        let b = via("SIP/2.0/UDP host.example.COM:5070;branch=z9hG4bK2;rport");
        assert_eq!(hasher.hash_one(a.sent_by_ref()), hasher.hash_one(b.sent_by_ref()));
        assert_eq!(hasher.hash_one(SentBy::of(&a)), hasher.hash_one(b.sent_by_ref()));
    }
}
