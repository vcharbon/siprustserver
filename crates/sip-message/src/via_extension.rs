//! The via-extension a B2BUA puts on its own Via (RFC 3261 §20.42 allows a
//! `via-extension`): the leg a request belongs to, the request's
//! Request-URI and whether it carried a session offer, with the signature the
//! sender made of them. A response carries the Via back, so a node that holds
//! no record of the request can still CANCEL it (§9.1), ACK its non-2xx
//! (§17.1.1.3), or ACK its 2xx with the answer an offer in the 2xx is owed
//! (§13.2.2.4, RFC 3264 §4).
//!
//! The value is one `token` (§25.1): the leg and the Request-URI
//! base64url-encoded without padding, the offer flag (`o` or `n`) and the
//! signature base64url-encoded, joined by `.`. It is bounded by [`MAX_LEN`]: a
//! request whose fields would not fit carries none. This module only reads and
//! writes the text; the signature is the caller's to make and to check.

use base64::Engine as _;

use crate::header::{ParamValue, Via};
use crate::SipStr;

/// The Via parameter that carries the extension.
pub const PARAM: &str = "vx";

/// The most bytes the parameter's value may hold.
pub const MAX_LEN: usize = 512;

const SEPARATOR: char = '.';

/// What a sender states about its own request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViaExtension {
    /// The leg the request belongs to.
    pub leg: String,
    /// The request's Request-URI, as sent.
    pub request_uri: String,
    /// Whether the request carried a session offer (RFC 3264 §4).
    pub offer: bool,
}

impl ViaExtension {
    /// The parameter value carrying the extension and `signature`, `None`
    /// when it would exceed [`MAX_LEN`].
    pub fn encode(&self, signature: &[u8]) -> Option<String> {
        let engine = &base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let value = [
            engine.encode(self.leg.as_bytes()),
            engine.encode(self.request_uri.as_bytes()),
            self.offer_flag().to_string(),
            engine.encode(signature),
        ]
        .join(&SEPARATOR.to_string());
        (value.len() <= MAX_LEN).then_some(value)
    }

    /// The offer flag as it rides: `o` for an offer, `n` for none.
    pub fn offer_flag(&self) -> &'static str {
        if self.offer {
            "o"
        } else {
            "n"
        }
    }

    /// The extension a parameter value states and the signature it carries,
    /// unverified; `None` for a value that is not two base64url fields of
    /// UTF-8 text, an offer flag and a signature, or exceeds [`MAX_LEN`].
    pub fn decode(value: &str) -> Option<(ViaExtension, Vec<u8>)> {
        if value.len() > MAX_LEN {
            return None;
        }
        let engine = &base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut fields = value.split(SEPARATOR);
        let leg = String::from_utf8(engine.decode(fields.next()?).ok()?).ok()?;
        let request_uri = String::from_utf8(engine.decode(fields.next()?).ok()?).ok()?;
        let offer = match fields.next()? {
            "o" => true,
            "n" => false,
            _ => return None,
        };
        let signature = engine.decode(fields.next()?).ok()?;
        if fields.next().is_some() || leg.is_empty() || request_uri.is_empty() {
            return None;
        }
        Some((ViaExtension { leg, request_uri, offer }, signature))
    }

    /// The Via parameter carrying the extension and `signature`, `None` when
    /// it would exceed [`MAX_LEN`].
    pub fn param(&self, signature: &[u8]) -> Option<ParamValue> {
        self.encode(signature).map(|v| ParamValue::Token(SipStr::owned(&v)))
    }
}

/// The extension `via` carries and its signature, unverified.
pub fn read(via: &Via) -> Option<(ViaExtension, Vec<u8>)> {
    ViaExtension::decode(via.param(PARAM).and_then(ParamValue::as_str)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::char_class::is_token_char;

    fn ext() -> ViaExtension {
        ViaExtension {
            leg: "b-1".into(),
            request_uri: "sip:bob@127.0.0.1:5070;transport=udp".into(),
            offer: true,
        }
    }

    #[test]
    fn an_extension_round_trips_with_its_signature() {
        let value = ext().encode(&[7u8; 16]).expect("fits");
        assert_eq!(ViaExtension::decode(&value), Some((ext(), vec![7u8; 16])));
    }

    #[test]
    fn the_value_is_one_token() {
        let value = ext().encode(&[0xffu8; 16]).expect("fits");
        assert!(value.bytes().all(is_token_char), "{value}");
    }

    #[test]
    fn an_oversized_extension_is_not_written() {
        let long = ViaExtension {
            leg: "b-1".into(),
            request_uri: format!("sip:{}@h", "x".repeat(600)),
            offer: false,
        };
        assert_eq!(long.encode(&[0u8; 16]), None);
    }

    #[test]
    fn a_malformed_value_reads_as_none() {
        for value in ["", "a", "YQ.YQ.YQ", "YQ.YQ.x.YQ", "YQ.YQ.o.YQ.YQ", "!!.YQ.o.YQ", ".YQ.n.YQ"]
        {
            assert_eq!(ViaExtension::decode(value), None, "{value}");
        }
    }

    #[test]
    fn a_via_carries_it_as_a_parameter() {
        let via = Via::udp(SipStr::from_static("10.0.0.1"), 5060)
            .with_param(PARAM, ext().param(&[1u8; 16]).expect("fits"));
        assert_eq!(read(&via), Some((ext(), vec![1u8; 16])));
    }
}
