//! The continuation codec seam.

/// The peer may return the opaque value under a peer-specific wrapping.
pub trait HttpContinuationCodec: Send + Sync {
    /// The text standing for `token` in a reply.
    fn wrap(&self, token: &str) -> String;
    /// The unwrapped candidates `body` carries.
    fn unwrap(&self, body: &[u8]) -> Vec<String>;
}

/// No wrapping.
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
