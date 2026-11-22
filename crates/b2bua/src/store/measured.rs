//! A turn's record measured for the record-size cap (ADR-0057) before its
//! write: by the codec's body of the record when the call has a backup, the
//! body the write then carries, so the record is encoded once; by a count of
//! that body, which builds nothing, when the write replicates nothing.

use std::sync::Arc;

use call::{Call, CallBodyCodec};

use super::CallState;

/// The size of a record's body as the codec encodes it, with the body itself
/// when the record's write carries one.
#[derive(Debug)]
pub struct Measured {
    bytes: u64,
    body: Option<Vec<u8>>,
}

impl Measured {
    /// The body's length in bytes.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl CallState {
    /// `call`, a turn's record of a resident call, measured as its write
    /// encodes it. A record the codec refuses is counted without its depth
    /// bound, and its write's flush meets the refusal; one the serializer
    /// refuses outright reads `0`.
    pub fn measure(&self, call: &Call) -> Measured {
        if self.backup_of(&call.call_ref).is_some() {
            if let Ok(body) = self.codec.encode(call) {
                let bytes = u64::try_from(body.len()).unwrap_or(u64::MAX);
                return Measured { bytes, body: Some(body) };
            }
        }
        Measured { bytes: self.codec.encoded_len(call).unwrap_or(0), body: None }
    }

    /// [`write`](Self::write) `call`, the record `measured` measured: a flush
    /// carries the body the measure built.
    pub fn write_measured(&self, call: impl Into<Arc<Call>>, measured: Measured) -> bool {
        let call = call.into();
        self.update(Arc::clone(&call));
        let backed = self.backup_of(&call.call_ref).is_some();
        if backed {
            self.flush_carrying(&call, self.replicated_ttl_ms, measured.body);
        }
        backed
    }
}
