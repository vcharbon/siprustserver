//! The continuation token: an instance's position, minted into a reply and
//! scanned back from the next request body.
//!
//! Wire form: `~hc.` + base64url(checksum ‖ JSON payload) + `.~`. None of
//! those characters needs escaping in a JSON string or is rewritten by a JSON
//! serialiser that escapes `/`. A scan hit whose checksum fails is the peer's
//! data, never a token.

use std::collections::BTreeMap;
use std::hash::{BuildHasher, RandomState};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};

const OPEN: &str = "~hc.";
const CLOSE: &str = ".~";

/// A decoded token.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct HttpContinuation {
    /// The minting service's nonce.
    #[serde(rename = "n")]
    pub nonce: u64,
    /// The instance within that service.
    #[serde(rename = "i")]
    pub instance: u64,
    /// Where the instance stands.
    #[serde(rename = "a")]
    pub at: TokenAt,
}

/// An instance's position.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k")]
pub(crate) enum TokenAt {
    /// A reified script: the next step index (past the end when complete) and
    /// the captures taken so far.
    #[serde(rename = "r")]
    Reified {
        #[serde(rename = "p")]
        position: usize,
        #[serde(rename = "c", default, skip_serializing_if = "BTreeMap::is_empty")]
        captures: BTreeMap<String, String>,
    },
    /// A code step: the state it returned (`None` when complete).
    #[serde(rename = "c")]
    Code {
        #[serde(rename = "s")]
        state: Option<serde_json::Value>,
    },
}

impl HttpContinuation {
    /// The wire form. Deterministic: the same token renders the same bytes.
    pub(crate) fn render(&self) -> String {
        let json = serde_json::to_vec(self).expect("a token always serialises");
        let mut payload = checksum(&json).to_be_bytes().to_vec();
        payload.extend_from_slice(&json);
        format!("{OPEN}{}{CLOSE}", URL_SAFE_NO_PAD.encode(payload))
    }

    /// Every valid token in `body`, in order.
    pub(crate) fn scan(body: &str) -> Vec<Self> {
        let mut found = Vec::new();
        for (at, _) in body.match_indices(OPEN) {
            let start = at + OPEN.len();
            let len = body[start..]
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
                .count();
            if !body[start + len..].starts_with(CLOSE) {
                continue;
            }
            if let Some(token) = decode(&body[start..start + len]) {
                found.push(token);
            }
        }
        found
    }
}

fn decode(text: &str) -> Option<HttpContinuation> {
    let payload = URL_SAFE_NO_PAD.decode(text).ok()?;
    let (sum, json) = payload.split_at_checked(8)?;
    if checksum(json).to_be_bytes() != sum {
        return None;
    }
    serde_json::from_slice(json).ok()
}

/// FNV-1a, 64 bits: tells a token from look-alike peer data.
fn checksum(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3))
}

/// A nonce distinct per service and per process.
pub(crate) fn random_nonce() -> u64 {
    RandomState::new().hash_one(std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> HttpContinuation {
        HttpContinuation {
            nonce: 7,
            instance: 3,
            at: TokenAt::Reified {
                position: 1,
                captures: BTreeMap::from([("id".into(), r#"a\"b"#.into())]),
            },
        }
    }

    #[test]
    fn a_rendered_token_scans_back_from_surrounding_text() {
        let body = format!(r#"{{"x":"~hc.","ctx":"{}","y":".~"}}"#, token().render());
        assert_eq!(HttpContinuation::scan(&body), vec![token()]);
        assert_eq!(token().render(), token().render(), "deterministic");
    }

    #[test]
    fn a_look_alike_with_a_bad_checksum_is_not_a_token() {
        let good = token().render();
        let forged = format!("~hc.{}.~", URL_SAFE_NO_PAD.encode(b"\0\0\0\0\0\0\0\0{}"));
        assert!(HttpContinuation::scan(&forged).is_empty());
        let clipped = &good[..good.len() - 4];
        assert!(HttpContinuation::scan(clipped).is_empty());
    }

    #[test]
    fn the_wire_form_needs_no_json_escaping() {
        let wire = token().render();
        assert!(wire.bytes().all(|b| b.is_ascii_graphic() && !b"\"\\/".contains(&b)), "{wire}");
    }
}
