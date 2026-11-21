//! How an endpoint frames the bodies it sends on its INVITE transactions: a
//! session description bare under `application/sdp` or inside a `multipart`
//! body beside parts that are no description (RFC 5621), and the body of an
//! INVITE that carries no offer (RFC 3261 §13.2.1, §20.11).
//!
//! Every part that is not the description states `Content-Disposition` with
//! `handling=optional` (RFC 3261 §20.11, RFC 5621 §5): a receiver that does not
//! read it still processes the message. A framed description sits where
//! [`sip_message::multipart`] finds it: the first one of a `mixed` body, the
//! last of an `alternative` one, at any nesting depth it reads.

use sip_message::multipart::{compose, MultipartPart};

use crate::agent::{InDialogRequest, Invite, Respond};

/// A part beside the description that no receiver needs to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extra {
    /// A UTF-8 note, rendered.
    Text,
    /// Opaque bytes, every octet value among them.
    Binary,
    /// An XML document (RFC 5621 §3's URI-list example type).
    Xml,
    /// A signalling payload of the RFC 3204 ISUP type, `signal` disposition.
    Isup,
}

impl Extra {
    pub const ALL: [Extra; 4] = [Extra::Text, Extra::Binary, Extra::Xml, Extra::Isup];

    /// The part's `Content-Type`.
    pub fn content_type(self) -> &'static str {
        match self {
            Extra::Text => "text/plain;charset=utf-8",
            Extra::Binary => "application/octet-stream",
            Extra::Xml => "application/resource-lists+xml",
            Extra::Isup => "application/isup;version=itu-t92+",
        }
    }

    /// The part's `Content-Disposition`: optional to handle.
    pub fn disposition(self) -> &'static str {
        match self {
            Extra::Isup => "signal;handling=optional",
            _ => "render;handling=optional",
        }
    }

    /// The part's content.
    pub fn payload(self) -> Vec<u8> {
        match self {
            Extra::Text => "call note: zoë ✓\r\n".as_bytes().to_vec(),
            Extra::Binary => (0..=255u8).rev().chain(0..=255u8).collect(),
            Extra::Xml => b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n\
<resource-lists xmlns=\"urn:ietf:params:xml:ns:resource-lists\">\r\n\
<list><entry uri=\"sip:carol@example.com\"/></list>\r\n</resource-lists>\r\n"
                .to_vec(),
            Extra::Isup => vec![0x01, 0x00, 0x20, 0x00, 0x0a, 0x00, 0x02, 0x0a, 0x08, 0x83, 0x90],
        }
    }

    fn part(self) -> MultipartPart {
        MultipartPart::new(self.content_type(), self.payload())
            .with_header("Content-Disposition", self.disposition())
    }

    fn label(self) -> &'static str {
        match self {
            Extra::Text => "text",
            Extra::Binary => "binary",
            Extra::Xml => "xml",
            Extra::Isup => "isup",
        }
    }
}

/// How a session description rides its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Framing {
    /// The body is the description, `application/sdp`.
    #[default]
    Bare,
    /// `multipart/mixed`: the description and `extra`, the description first
    /// when `sdp_first`.
    Mixed { extra: Extra, sdp_first: bool },
    /// `multipart/alternative`: `extra`, then the description, the last and so
    /// the preferred alternative (RFC 2046 §5.1.4).
    Alternative { extra: Extra },
    /// `multipart/mixed` holding `extra` and a `multipart/alternative` whose
    /// last part is the description.
    Nested { extra: Extra },
}

/// A body as it goes on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Framed<'a> {
    Sdp(&'a str),
    Body { content_type: String, bytes: Vec<u8> },
}

impl Framed<'_> {
    /// The body's `Content-Type` and bytes.
    pub fn into_body(self) -> (String, Vec<u8>) {
        match self {
            Framed::Sdp(s) => ("application/sdp".to_string(), s.as_bytes().to_vec()),
            Framed::Body { content_type, bytes } => (content_type, bytes),
        }
    }
}

impl Framing {
    /// `sdp` under this framing. A composition the multipart writer refuses
    /// (none of these parts can be) falls back to the bare description.
    pub fn frame(self, sdp: &str) -> Framed<'_> {
        let description = || MultipartPart::new("application/sdp", sdp.as_bytes().to_vec());
        let composed = match self {
            Framing::Bare => return Framed::Sdp(sdp),
            Framing::Mixed { extra, sdp_first } => {
                let mut parts = vec![extra.part(), description()];
                if sdp_first {
                    parts.reverse();
                }
                compose("multipart/mixed", &parts)
            }
            Framing::Alternative { extra } => {
                compose("multipart/alternative", &[extra.part(), description()])
            }
            Framing::Nested { extra } => {
                compose("multipart/alternative", &[Extra::Text.part(), description()]).and_then(
                    |inner| {
                        let inner = MultipartPart::new(inner.content_type, inner.body);
                        compose("multipart/mixed", &[extra.part(), inner])
                    },
                )
            }
        };
        match composed {
            Ok(c) => Framed::Body { content_type: c.content_type, bytes: c.body },
            Err(_) => Framed::Sdp(sdp),
        }
    }

    /// A short name for a draw log.
    pub fn label(self) -> String {
        match self {
            Framing::Bare => "bare".to_string(),
            Framing::Mixed { extra, sdp_first } => {
                format!("mixed-{}-{}", extra.label(), if sdp_first { "sdp1" } else { "sdp2" })
            }
            Framing::Alternative { extra } => format!("alt-{}", extra.label()),
            Framing::Nested { extra } => format!("nested-{}", extra.label()),
        }
    }
}

/// The body of an INVITE that makes no offer: content that is no session
/// description, so the offer comes from the callee (RFC 3261 §13.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoOffer {
    /// `extra` alone, its disposition on the message.
    Bare(Extra),
    /// `multipart/mixed` of two parts, neither a description.
    Mixed(Extra, Extra),
}

impl NoOffer {
    /// The body's `Content-Type`, its bytes, and the message's
    /// `Content-Disposition` when the body is one part.
    pub fn body(self) -> (String, Vec<u8>, Option<&'static str>) {
        match self {
            NoOffer::Bare(extra) => {
                (extra.content_type().to_string(), extra.payload(), Some(extra.disposition()))
            }
            NoOffer::Mixed(a, b) => match compose("multipart/mixed", &[a.part(), b.part()]) {
                Ok(c) => (c.content_type, c.body, None),
                Err(_) => (a.content_type().to_string(), a.payload(), Some(a.disposition())),
            },
        }
    }

    /// A short name for a draw log.
    pub fn label(self) -> String {
        match self {
            NoOffer::Bare(e) => format!("body-{}", e.label()),
            NoOffer::Mixed(a, b) => format!("body-mixed-{}-{}", a.label(), b.label()),
        }
    }
}

/// A message builder that carries a body.
pub(super) trait CarriesBody: Sized {
    fn sdp(self, sdp: &str) -> Self;
    fn body(self, content_type: &str, bytes: Vec<u8>) -> Self;

    /// `sdp` under `framing`.
    fn framed(self, framing: Framing, sdp: &str) -> Self {
        match framing.frame(sdp) {
            Framed::Sdp(s) => self.sdp(s),
            Framed::Body { content_type, bytes } => self.body(&content_type, bytes),
        }
    }
}

macro_rules! carries_body {
    ($($t:ty),*) => {$(
        impl CarriesBody for $t {
            fn sdp(self, sdp: &str) -> Self {
                self.with_sdp(sdp)
            }
            fn body(self, content_type: &str, bytes: Vec<u8>) -> Self {
                self.with_body(content_type, bytes)
            }
        }
    )*};
}

carries_body!(Invite<'_>, Respond<'_>, InDialogRequest<'_>);

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::header::HeaderValue;

    const SDP: &str = crate::OFFER_SDP;

    /// Where a message typed `ct` under `disposition` holds its description.
    fn description(
        ct: &str,
        disposition: Option<&str>,
        body: &[u8],
    ) -> Option<std::ops::Range<usize>> {
        let media = sip_message::header::MediaType::parse(&sip_message::SipStr::owned(ct)).ok()?;
        sip_message::multipart::sdp_range(&media, disposition, body)
    }

    fn framings() -> Vec<Framing> {
        let mut all = vec![Framing::Bare];
        for extra in Extra::ALL {
            all.push(Framing::Mixed { extra, sdp_first: true });
            all.push(Framing::Mixed { extra, sdp_first: false });
            all.push(Framing::Alternative { extra });
            all.push(Framing::Nested { extra });
        }
        all
    }

    #[test]
    fn every_framing_carries_the_description_where_a_reader_finds_it() {
        for f in framings() {
            let (ct, bytes) = match f.frame(SDP) {
                Framed::Sdp(s) => ("application/sdp".to_string(), s.as_bytes().to_vec()),
                Framed::Body { content_type, bytes } => (content_type, bytes),
            };
            let found = description(&ct, None, &bytes).map(|r| &bytes[r]);
            assert_eq!(found, Some(SDP.as_bytes()), "{}", f.label());
        }
    }

    #[test]
    fn a_no_offer_body_holds_no_description() {
        for a in Extra::ALL {
            for body in [NoOffer::Bare(a), NoOffer::Mixed(a, Extra::Text)] {
                let (ct, bytes, disposition) = body.body();
                assert_eq!(description(&ct, disposition, &bytes), None, "{}", body.label());
            }
        }
    }
}
