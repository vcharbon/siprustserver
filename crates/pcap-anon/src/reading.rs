//! How this crate reads a captured message: the [`ToolReading`] the capture
//! was extracted under (`sipflow --reading`), so a shape a peer sends and the
//! extraction reads is read here too. The caller states it; nothing here
//! holds a default beyond the RFC's own.

use sip_message::{SipMessage, SipParseError, SipParser, ToolReading};

/// The reading every parse of a captured message runs under.
#[derive(Debug, Clone)]
pub struct Reading {
    /// The SIP reading the capture was extracted under.
    pub sip: ToolReading,
}

impl Reading {
    /// The RFC's reading: no exemption, the parser's default bounds.
    pub fn plain_rfc() -> Reading {
        Reading { sip: ToolReading::plain_rfc() }
    }

    /// The reading a reading file states (`sip_message::ToolReading::stated`),
    /// or every line of it that does not read, `origin` naming the file.
    pub fn stated(text: &str, origin: &str) -> Result<Reading, String> {
        ToolReading::stated(text)
            .map(|sip| Reading { sip })
            .map_err(|problems| format!("{origin}: {}", problems.join("; ")))
    }

    /// `raw` as this reading reads it.
    pub fn parse(&self, raw: &[u8]) -> Result<SipMessage, SipParseError> {
        self.sip.parse(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_MAX_FORWARDS: &str = "BYE sip:bob@192.0.2.10 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-reading\r\n\
From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@example.com>;tag=b\r\n\
Call-ID: reading@192.0.2.5\r\n\
CSeq: 2 BYE\r\n\
Content-Length: 0\r\n\r\n";

    /// A shape a stated reading exempts reads under it, where the RFC's
    /// reading refuses it.
    #[test]
    fn an_exempted_shape_reads_under_the_stated_reading() {
        let stated = Reading::stated("exemptions = max-forwards:absent\n", "test").unwrap();
        assert!(Reading::plain_rfc().parse(NO_MAX_FORWARDS.as_bytes()).is_err());
        assert!(stated.parse(NO_MAX_FORWARDS.as_bytes()).is_ok());
    }

    /// A reading file with a line that does not read is refused, the file
    /// named.
    #[test]
    fn a_reading_with_an_unknown_key_is_refused() {
        let refused = Reading::stated("colour = blue\n", "the.reading").unwrap_err();
        assert!(refused.starts_with("the.reading: line 1: colour"), "{refused}");
    }
}
