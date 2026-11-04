//! A replicated body read as the call it carries in its envelope. A body the
//! codec refuses, malformed or nested past its bound
//! (`call::codec::MAX_DEPTH`), is counted and logged: the call it carries has
//! no usable copy here.

use call::MsgpackCodec;

use crate::metrics::B2buaMetrics;
use crate::store::envelope::{self, Enveloped};

/// `body` decoded; `None`, counted and logged, when the codec refuses it.
pub(crate) fn decode_peer_body(
    codec: &MsgpackCodec,
    metrics: &B2buaMetrics,
    body: &[u8],
) -> Option<Enveloped> {
    match envelope::decode(codec, body) {
        Ok(call) => Some(call),
        Err(e) => {
            metrics.bump_repl_body_undecodable();
            tracing::warn!(bytes = body.len(), error = %e, "replicated call body refused at decode");
            None
        }
    }
}
