//! A replicated call body read as a call. A body the codec refuses, malformed
//! or nested past its bound (`call::codec::MAX_DEPTH`), is counted and
//! logged: the call it carries has no usable replica here.

use call::{Call, CallBodyCodec, MsgpackCodec};

use crate::metrics::B2buaMetrics;

/// `body` decoded; `None`, counted and logged, when the codec refuses it.
pub(crate) fn decode_peer_body(
    codec: &MsgpackCodec,
    metrics: &B2buaMetrics,
    body: &[u8],
) -> Option<Call> {
    match codec.decode(body) {
        Ok(call) => Some(call),
        Err(e) => {
            metrics.bump_repl_body_undecodable();
            tracing::warn!(bytes = body.len(), error = %e, "replicated call body refused at decode");
            None
        }
    }
}
