//! The store envelope (ADR-0055 §7, §11): what the store keeps and
//! replicates beside a call record — the worker pair the call was placed on,
//! the `(p,b)` version vector and the state stamp — and the replicated body
//! that carries both. The call record holds none of it.

use call::{Call, CallBodyCodec, CallDecodeError, CallEncodeError, MsgpackCodec, RelayOwner};
use serde::{Deserialize, Serialize};

use crate::owners::Owners;

/// The worker pair stamped at INVITE time plus a per-context **version
/// vector** `(gen, bak_gen)` = `(p, b)` (ADR-0014) and the **state stamp**
/// (ADR-0055 §7).
///
/// `gen` (`p`) is the **primary** counter — bumped only by the call's primary on
/// a local mutation. `bak_gen` (`b`) is the **backup** counter — bumped only by
/// an acting-backup on a takeover mutation. Each node bumps only its own role's
/// counter, so the *other* node's counter in an incoming update is, by
/// construction, the branch point.
///
/// The stamp is the vector of the last write that changed a compared field
/// (`call::helpers::same_state`): a write that changed only exempt fields
/// moves the vector and leaves the stamp. Of two copies of one call
/// incarnation, R contains L when L's stamp is at or below R's vector,
/// component by component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallTopology {
    pub pri: String,
    pub bak: String,
    /// Primary counter `p` of the `(p,b)` version vector.
    pub gen: i64,
    /// Backup counter `b` of the `(p,b)` version vector.
    pub bak_gen: i64,
    /// The `p` of the state stamp.
    pub stamp_gen: i64,
    /// The `b` of the state stamp.
    pub stamp_bak_gen: i64,
}

impl CallTopology {
    /// The topology of a call `pri` serves and `bak` backs up, at `(gen,
    /// bak_gen)`, stamped at that version.
    pub fn new(pri: impl Into<String>, bak: impl Into<String>, gen: i64, bak_gen: i64) -> Self {
        CallTopology {
            pri: pri.into(),
            bak: bak.into(),
            gen,
            bak_gen,
            stamp_gen: gen,
            stamp_bak_gen: bak_gen,
        }
    }

    /// The version vector `(p, b)`.
    pub fn vector(&self) -> (i64, i64) {
        (self.gen, self.bak_gen)
    }

    /// The state stamp `(p, b)`.
    pub fn stamp(&self) -> (i64, i64) {
        (self.stamp_gen, self.stamp_bak_gen)
    }

    /// Stamp the current vector: the write it counts changed a compared field.
    pub fn stamp_vector(&mut self) {
        (self.stamp_gen, self.stamp_bak_gen) = (self.gen, self.bak_gen);
    }

    /// Raise the vector to the component-wise maximum of its own and
    /// `other`'s; the stamp stays.
    pub fn raise_vector(&mut self, other: &CallTopology) {
        self.gen = self.gen.max(other.gen);
        self.bak_gen = self.bak_gen.max(other.bak_gen);
    }

    /// The owner of the call other than `node`: the backup for the primary,
    /// the primary for a copy on the backup; `None` for a call with one owner.
    pub fn other_owner(&self, node: &str) -> Option<&str> {
        let other = if node == self.bak { &self.pri } else { &self.bak };
        (!other.is_empty() && other != node).then_some(other.as_str())
    }

    /// Whether a copy served the call, read on `node`: `node` holds it as its
    /// backup's copy, or the record holds a write a copy made (a backup
    /// counter above zero).
    pub fn copy_served(&self, node: &str) -> bool {
        self.bak == node || self.bak_gen > 0
    }
}

/// The owners the core reads for a turn on a call placed under `topology`,
/// run on the node `this`.
pub fn owners(this: &RelayOwner, topology: Option<&CallTopology>) -> Owners {
    Owners {
        this: this.clone(),
        other: topology.and_then(|t| t.other_owner(&this.ordinal)).map(str::to_string),
        copy_served: topology.is_some_and(|t| t.copy_served(&this.ordinal)),
    }
}

/// A call record in its envelope: as a replicated body carries it, and as
/// a copy of the call met elsewhere is read.
#[derive(Clone, Debug, PartialEq)]
pub struct Enveloped {
    pub call: Call,
    /// `None` for a call placed without a worker pair: it is not replicated.
    pub topology: Option<CallTopology>,
}

impl Enveloped {
    pub fn new(call: Call, topology: Option<CallTopology>) -> Self {
        Self { call, topology }
    }

    /// `self` with the exempt fields of `other` merged in
    /// (`call::helpers::merge_exempt`) and its vector raised to the
    /// component-wise maximum of both; the stamp stays `self`'s.
    pub fn merge_exempt(self, other: &Enveloped) -> Enveloped {
        let call = call::helpers::merge_exempt(self.call, &other.call);
        let mut topology = self.topology;
        if let (Some(m), Some(o)) = (topology.as_mut(), other.topology.as_ref()) {
            m.raise_vector(o);
        }
        Enveloped { call, topology }
    }
}

/// A replicated body: the envelope, then the call body the codec wrote, as
/// a msgpack binary. The call body keeps the codec's own nesting bound.
#[derive(Serialize)]
struct BodyOut<'a>(Option<&'a CallTopology>, #[serde(with = "serde_bytes")] &'a [u8]);

#[derive(Deserialize)]
struct BodyIn<'a>(Option<CallTopology>, #[serde(borrow, with = "serde_bytes")] &'a [u8]);

/// `call` under `topology`, encoded as a replicated body.
pub fn encode(
    codec: &MsgpackCodec,
    topology: Option<&CallTopology>,
    call: &Call,
) -> Result<Vec<u8>, CallEncodeError> {
    let inner = codec.encode(call)?;
    rmp_serde::to_vec(&BodyOut(topology, &inner))
        .map_err(|e| CallEncodeError::Encode(e.to_string()))
}

/// A replicated body read as the call it carries in its envelope.
pub fn decode(codec: &MsgpackCodec, body: &[u8]) -> Result<Enveloped, CallDecodeError> {
    if body.is_empty() {
        return Err(CallDecodeError::Empty);
    }
    let BodyIn(topology, inner) =
        rmp_serde::from_slice(body).map_err(|e| CallDecodeError::Decode(e.to_string()))?;
    Ok(Enveloped { call: codec.decode(inner)?, topology })
}

/// `call` as a replicated body, in the envelope the router places it under
/// at its birth: the worker pair its caller's INVITE cookie names.
#[cfg(any(test, feature = "testkit"))]
pub fn placed_body(call: &Call) -> Result<Vec<u8>, CallEncodeError> {
    let topology = crate::router::topology_cookie::placement(call);
    encode(&MsgpackCodec::new(), topology.as_ref(), call)
}

#[cfg(test)]
mod tests;
