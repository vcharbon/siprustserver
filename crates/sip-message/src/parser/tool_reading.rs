//! How a tool reads the messages it was handed (a capture's, a run's
//! recording): its own exemptions and bounds, which may admit more than any
//! deployment does (ADR-0053 §6). A reading is stated once, in the text of a
//! file the tool names (`sipflow --reading`), and refused whole when any line
//! of it does not read. A tool that states none reads
//! [`ToolReading::plain_rfc`]: no exemption, the parser's default bounds.
//!
//! The file holds one `key = value` per line; `#` starts a comment, and a
//! key appears at most once:
//!
//! ```text
//! exemptions = max-forwards:absent,bare-user-semicolon
//! max-header-length = 65535
//! max-uri-length = 65535
//! max-header-lines = 1000
//! max-datagram = 65535
//! ```

use super::custom::CustomParser;
use super::{Exemptions, SipParser, SipParserLimits, DATAGRAM_BYTES};
use crate::error::SipParseError;
use crate::types::SipMessage;
use crate::whitespace::trim;

/// A tool's reading of the messages it is handed.
#[derive(Debug, Clone)]
pub struct ToolReading {
    limits: SipParserLimits,
    parser: CustomParser,
    max_datagram: usize,
}

impl ToolReading {
    /// The RFC's reading: no exemption, the parser's default bounds.
    pub fn plain_rfc() -> Self {
        Self::of(SipParserLimits::DEFAULT, DATAGRAM_BYTES)
    }

    fn of(limits: SipParserLimits, max_datagram: usize) -> Self {
        Self { parser: CustomParser::tool(limits.clone()), limits, max_datagram }
    }

    /// The reading `text` states, or every line that does not read, one per
    /// entry of the error.
    pub fn stated(text: &str) -> Result<Self, Vec<String>> {
        let mut problems = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        let mut limits = SipParserLimits::DEFAULT;
        let mut max_datagram = DATAGRAM_BYTES;
        for (n, line) in text.lines().enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let line = trim(line.split_once('#').map_or(line, |(kept, _)| kept));
            if line.is_empty() {
                continue;
            }
            let at = n + 1;
            let Some((key, value)) = line.split_once('=').map(|(k, v)| (trim(k), trim(v))) else {
                problems.push(format!("line {at}: no `key = value` in {line:?}"));
                continue;
            };
            if seen.contains(&key) {
                problems.push(format!("line {at}: {key} stated twice"));
                continue;
            }
            seen.push(key);
            let read = match key {
                "exemptions" => Exemptions::read(Some(value)).map(|set| limits.exemptions = set),
                "max-header-length" => size(value).map(|n| limits.max_header_length = n),
                "max-uri-length" => size(value).map(|n| limits.max_uri_length = n),
                "max-header-lines" => size(value).map(|n| limits.max_header_lines = n),
                "max-datagram" => size(value).map(|n| max_datagram = n),
                _ => Err("is no key a reading states".to_string()),
            };
            if let Err(problem) = read {
                problems.push(format!("line {at}: {key} {problem}"));
            }
        }
        if problems.is_empty() {
            Ok(Self::of(limits, max_datagram))
        } else {
            Err(problems)
        }
    }

    /// The largest message, in bytes, this reading parses.
    pub fn max_datagram(&self) -> usize {
        self.max_datagram
    }

    /// The bounds and exemptions this reading parses under, for a party that
    /// reads the wire itself (a test agent) to read as the tool does.
    pub fn limits(&self) -> &SipParserLimits {
        &self.limits
    }
}

/// A payload past [`max_datagram`](ToolReading::max_datagram) is refused
/// before it is parsed.
impl SipParser for ToolReading {
    fn name(&self) -> &str {
        "tool-reading"
    }

    fn parse(&self, payload: &[u8]) -> Result<SipMessage, SipParseError> {
        if payload.len() > self.max_datagram {
            return Err(SipParseError::new(format!(
                "datagram of {} bytes exceeds the reading's bound {}",
                payload.len(),
                self.max_datagram
            )));
        }
        self.parser.parse(payload)
    }
}

/// A size: a positive count of bytes or lines.
fn size(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) | Err(_) => Err(format!("wants a positive integer, got {value:?}")),
        Ok(n) => Ok(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE_NO_MAX_FORWARDS: &str = "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-reading\r\n\
From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: reading@192.0.2.5\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@192.0.2.5>\r\n\
Content-Length: 0\r\n\r\n";

    #[test]
    fn an_exemption_the_reading_states_is_read_and_the_rfc_reading_refuses_it() {
        let raw = INVITE_NO_MAX_FORWARDS.as_bytes();
        assert!(ToolReading::plain_rfc().parse(raw).is_err(), "§8.1.1: Max-Forwards is mandatory");
        let stated =
            ToolReading::stated("exemptions = max-forwards:absent # the corpus holds it\n")
                .expect("the reading reads");
        assert!(stated.parse(raw).is_ok());
    }

    #[test]
    fn each_size_bounds_its_reading() {
        let raw = INVITE_NO_MAX_FORWARDS.as_bytes();
        let open = "exemptions = max-forwards:absent\n";
        let bounded = |line: &str| ToolReading::stated(&format!("{open}{line}\n")).expect("reads");
        assert!(bounded("max-datagram = 64").parse(raw).is_err());
        assert!(bounded("max-uri-length = 8").parse(raw).is_err());
        assert!(bounded("max-header-length = 20").parse(raw).is_err());
        assert!(bounded("max-header-lines = 4").parse(raw).is_err());
        let room = format!("max-datagram = {}", raw.len());
        assert!(bounded(&room).parse(raw).is_ok());
    }

    #[test]
    fn every_line_that_does_not_read_is_named_and_nothing_is_read() {
        let problems = ToolReading::stated(
            "exemptions = not-an-exemption\nmax-datagram = 0\nmax-uri-length = big\n\
             max-header-lines\nmax-blank = 3\nmax-datagram = 9\n",
        )
        .expect_err("refused");
        let joined = problems.join("\n");
        for named in ["line 1: exemptions", "line 2: max-datagram", "line 3: max-uri-length"] {
            assert!(joined.contains(named), "{named}: {joined}");
        }
        assert!(joined.contains("line 4: no `key = value`"), "{joined}");
        assert!(joined.contains("line 5: max-blank is no key"), "{joined}");
        assert!(joined.contains("line 6: max-datagram stated twice"), "{joined}");
        assert_eq!(problems.len(), 6, "{joined}");
    }

    #[test]
    fn a_file_with_crlf_line_ends_reads() {
        let stated =
            ToolReading::stated("exemptions = max-forwards:absent\r\nmax-datagram = 300\r\n")
                .expect("CRLF lines read");
        assert_eq!(stated.max_datagram(), 300);
    }

    #[test]
    fn a_blank_exemption_value_is_refused() {
        assert!(ToolReading::stated("exemptions =\n").is_err());
    }
}
