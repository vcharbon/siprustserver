//! The continuation codec: the peer may return the opaque value under a
//! peer-specific wrapping. The codec wraps the token on emission and unwraps
//! candidates from a request body on the scan; the service holds no progress
//! either way (the unwrapped token is the position).

/// How a continuation token travels in a peer's payload.
///
/// [`wrap`](Self::wrap)'s output stands verbatim where a reply template says
/// `${continuation}`, so it must need no escaping in the surrounding payload.
/// [`unwrap`](Self::unwrap) returns the texts the service scans for tokens
/// besides the raw body, which is always scanned: an unwrapped candidate that
/// holds no valid token is the peer's data. Request fragments (`contains`)
/// always match the body as sent, never an unwrapped candidate.
pub trait HttpContinuationCodec: Send + Sync {
    /// The text standing for `token` in a reply.
    fn wrap(&self, token: &str) -> String;
    /// The unwrapped texts of the values in `body` that may carry a token.
    fn unwrap(&self, body: &[u8]) -> Vec<String>;
}

/// The default: the token travels as minted, found by the raw scan alone.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityCodec;

impl HttpContinuationCodec for IdentityCodec {
    fn wrap(&self, token: &str) -> String {
        token.to_string()
    }

    fn unwrap(&self, _body: &[u8]) -> Vec<String> {
        Vec::new()
    }
}
