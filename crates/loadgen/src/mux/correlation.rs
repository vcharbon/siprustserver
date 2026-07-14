//! Per-run correlation strategy: HOW a call's token travels through the SUT.

use regex::Regex;
use scenario_harness::legpick::LegInfo;
use scenario_harness::realcall::CorrelationStamp;
use sip_message::sniff::header_value;

/// How the per-call correlation token is carried through the SUT, with two
/// halves:
/// - **stamp** ([`Correlation::stamp`]): how the token is written into the
///   outgoing INVITE (applied inside `CallEnv::outgoing_invite`).
/// - **extract** (crate-internal `token()`, used by the demux route path): how
///   the token is recovered from a received leg.
///
/// Strategies:
/// - [`Correlation::header`] / [`Correlation::header_templated`] — the token
///   rides a single transparent header (e.g. `X-Loadgen-Id`) the SUT RELAYS
///   onto every originated leg (our b2bua: `B2BUA_RELAY_HEADERS`). The value
///   shape is templated (`${token}` placeholder) so the token can ride
///   structured headers (`"${token};encoding=hex"` for User-to-User,
///   `"icid-value=${token}"` for P-Charging-Vector); extraction is a regex
///   whose FIRST capture group is the token (derived from the template, or
///   overridden).
/// - [`Correlation::to_user`] — the token IS the To-header user-part. A
///   SIP-correct B2BUA copies the To URI onto its originated leg, so this
///   survives a third-party SUT that strips unknown headers (zero cooperation).
#[derive(Debug, Clone)]
pub struct Correlation {
    strategy: Strategy,
}

#[derive(Debug, Clone)]
enum Strategy {
    Header {
        name: String,
        /// Header VALUE template with a `${token}` placeholder.
        template: String,
        /// Extraction regex (first capture group = the token). `None` only for
        /// the untemplated `"${token}"` default → the whole (trimmed) header
        /// value is the token.
        extract: Option<Regex>,
    },
    ToUser,
}

/// What a token looks like inside a structured header value when deriving the
/// extraction regex from a template: unreserved URI characters (covers the
/// minted `lg<uuid-simple>` tokens and any hex/uuid/alnum token).
const TOKEN_PATTERN: &str = "[A-Za-z0-9._~-]+";

impl Correlation {
    /// A single transparent correlation header carrying the bare token
    /// (stamp = the token itself, extract = the whole header value).
    pub fn header(name: impl Into<String>) -> Self {
        Self {
            strategy: Strategy::Header {
                name: name.into(),
                template: "${token}".to_string(),
                extract: None,
            },
        }
    }

    /// A relayed correlation header with a templated VALUE: `template` must
    /// contain a `${token}` placeholder (e.g. `"${token};encoding=hex"`,
    /// `"icid-value=${token}"`). `extract` optionally overrides the extraction
    /// regex — its FIRST capture group is the token; when `None` the regex is
    /// derived from the template (literal parts escaped, the placeholder
    /// replaced by an unreserved-chars capture group). Errors on a template
    /// without the placeholder, an invalid regex, or an override with no
    /// capture group.
    pub fn header_templated(
        name: impl Into<String>,
        template: impl Into<String>,
        extract: Option<&str>,
    ) -> Result<Self, String> {
        let template = template.into();
        let Some((prefix, suffix)) = template.split_once("${token}") else {
            return Err(format!(
                "correlation template {template:?} has no ${{token}} placeholder"
            ));
        };
        let extract = match extract {
            Some(re) => {
                let re = Regex::new(re).map_err(|e| format!("bad correlation extract regex: {e}"))?;
                if re.captures_len() < 2 {
                    return Err(format!(
                        "correlation extract regex {:?} needs a capture group (group 1 = the token)",
                        re.as_str()
                    ));
                }
                Some(re)
            }
            // Plain "${token}" → whole-value extraction (no charset assumption
            // on the token).
            None if prefix.is_empty() && suffix.is_empty() => None,
            None => Some(
                Regex::new(&format!(
                    "{}({TOKEN_PATTERN}){}",
                    regex::escape(prefix),
                    regex::escape(suffix)
                ))
                .expect("derived correlation regex is always valid"),
            ),
        };
        Ok(Self { strategy: Strategy::Header { name: name.into(), template, extract } })
    }

    /// Token embedded as the To-header user-part — zero SUT cooperation needed.
    pub fn to_user() -> Self {
        Self { strategy: Strategy::ToUser }
    }

    /// The STAMP half: how a scenario writes `token` into the outgoing INVITE.
    pub fn stamp(&self, token: &str) -> CorrelationStamp {
        match &self.strategy {
            Strategy::Header { name, template, .. } => CorrelationStamp::Header {
                name: name.clone(),
                value: template.replace("${token}", token),
            },
            Strategy::ToUser => CorrelationStamp::ToUser,
        }
    }

    /// The EXTRACT half: the correlation token carried by `raw`, if present.
    pub(super) fn token(&self, raw: &[u8]) -> Option<String> {
        match &self.strategy {
            Strategy::Header { name, extract, .. } => {
                let value = header_value(raw, name)?;
                match extract {
                    None => Some(value),
                    Some(re) => re.captures(&value)?.get(1).map(|m| m.as_str().to_string()),
                }
            }
            Strategy::ToUser => LegInfo::new(raw).to_user(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal initial INVITE carrying the given To header + optional extra
    /// header line, for extraction tests.
    fn invite(to: &str, extra: &str) -> Vec<u8> {
        format!(
            "INVITE sip:x@127.0.0.1 SIP/2.0\r\nCall-ID: c1@h\r\n{extra}To: {to}\r\n\
             From: <sip:a@h>;tag=1\r\nCSeq: 1 INVITE\r\n\r\n"
        )
        .into_bytes()
    }

    fn stamp_header(c: &Correlation, token: &str) -> (String, String) {
        match c.stamp(token) {
            CorrelationStamp::Header { name, value } => (name, value),
            other => panic!("expected a Header stamp, got {other:?}"),
        }
    }

    /// The untuned default: stamp value == the bare token, extract == the whole
    /// (trimmed) header value, whatever its charset.
    #[test]
    fn plain_header_default_is_whole_value() {
        let c = Correlation::header("X-Loadgen-Id");
        let (name, value) = stamp_header(&c, "lgdeadbeef");
        assert_eq!((name.as_str(), value.as_str()), ("X-Loadgen-Id", "lgdeadbeef"));

        let raw = invite("<sip:bob@127.0.0.1>", "X-Loadgen-Id: lgdeadbeef\r\n");
        assert_eq!(c.token(&raw).as_deref(), Some("lgdeadbeef"));

        // Whole-value semantics: a value outside the derived-token charset is
        // still returned verbatim (no regex narrowing on the untuned default).
        let odd = invite("<sip:bob@127.0.0.1>", "X-Loadgen-Id: weird/value+x\r\n");
        assert_eq!(c.token(&odd).as_deref(), Some("weird/value+x"));

        // And header_templated with the bare "${token}" template is the same.
        let c2 = Correlation::header_templated("X-Loadgen-Id", "${token}", None).unwrap();
        assert_eq!(c2.token(&odd).as_deref(), Some("weird/value+x"));
    }

    /// UUI-shaped template (RFC 7433 User-to-User): the token rides
    /// `User-to-User: <token>;encoding=hex`; the derived regex recovers it.
    #[test]
    fn uui_shaped_template_renders_and_extracts() {
        let c =
            Correlation::header_templated("User-to-User", "${token};encoding=hex", None).unwrap();
        let (name, value) = stamp_header(&c, "lg0a1b2c");
        assert_eq!((name.as_str(), value.as_str()), ("User-to-User", "lg0a1b2c;encoding=hex"));

        let raw = invite("<sip:bob@127.0.0.1>", "User-to-User: lg0a1b2c;encoding=hex\r\n");
        assert_eq!(c.token(&raw).as_deref(), Some("lg0a1b2c"));

        // A leg without the header yields no token (orphan path).
        assert_eq!(c.token(&invite("<sip:bob@127.0.0.1>", "")), None);
    }

    /// PCV-shaped template (P-Charging-Vector `icid-value=`): rendered inside a
    /// param list; the derived regex recovers the token even when the SUT
    /// appends further params after it.
    #[test]
    fn pcv_shaped_template_renders_and_extracts() {
        let c =
            Correlation::header_templated("P-Charging-Vector", "icid-value=${token}", None)
                .unwrap();
        let (name, value) = stamp_header(&c, "lgfeed01");
        assert_eq!(
            (name.as_str(), value.as_str()),
            ("P-Charging-Vector", "icid-value=lgfeed01")
        );

        let relayed = invite(
            "<sip:bob@127.0.0.1>",
            "P-Charging-Vector: icid-value=lgfeed01;icid-generated-at=10.0.0.1\r\n",
        );
        assert_eq!(c.token(&relayed).as_deref(), Some("lgfeed01"));
    }

    /// The CLI extraction override: an explicit regex (first capture group =
    /// the token) beats the derived one; invalid overrides are rejected.
    #[test]
    fn explicit_extract_override() {
        let c = Correlation::header_templated(
            "User-to-User",
            "${token};encoding=hex",
            Some(r"^\s*([0-9a-fx]+)\s*;"),
        )
        .unwrap();
        let raw = invite("<sip:bob@127.0.0.1>", "User-to-User: 0xabc ;encoding=hex\r\n");
        assert_eq!(c.token(&raw).as_deref(), Some("0xabc"));

        // No capture group → config error, not a silent mis-extraction.
        assert!(Correlation::header_templated("H", "${token}", Some("nogroup")).is_err());
        // Invalid regex → error.
        assert!(Correlation::header_templated("H", "${token}", Some("(")).is_err());
        // Template without the placeholder → error.
        assert!(Correlation::header_templated("H", "no-placeholder", None).is_err());
    }

    /// To-user strategy: stamp is [`CorrelationStamp::ToUser`]; extraction
    /// recovers the token from the To user-part in both name-addr and bare-URI
    /// shapes — no loadgen header involved.
    #[test]
    fn to_user_strategy_extracts_from_to_header() {
        let c = Correlation::to_user();
        assert!(matches!(c.stamp("lg123"), CorrelationStamp::ToUser));

        let name_addr = invite("\"Bee\" <sip:lg123abc@10.0.0.9:5070>", "");
        assert_eq!(c.token(&name_addr).as_deref(), Some("lg123abc"));

        let bare = invite("sip:lg456def@10.0.0.9", "");
        assert_eq!(c.token(&bare).as_deref(), Some("lg456def"));

        // A relayed loadgen header is IGNORED by this strategy (extract is
        // To-user only), and a userless To yields no token.
        let userless = invite("<sip:10.0.0.9:5070>", "X-Loadgen-Id: lg999\r\n");
        assert_eq!(c.token(&userless), None);
    }
}
