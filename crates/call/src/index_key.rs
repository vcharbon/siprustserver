//! The SIP routing index: the keys a call is found by when a message names it
//! only by Call-ID and tag, and the order a lookup probes them in. Pure over
//! the [`Call`] shape, so the live index, the replicated index and every
//! lookup share one grammar.
//!
//! A key's namespace states which side of the B2BUA owns the identity. In a
//! spiral (RFC 3261 §16.3) one call's outgoing leg and another call's
//! incoming leg carry the same Call-ID and From-tag; the namespaces keep the
//! two entries apart, so neither call's index write removes or shadows the
//! other's.

use crate::model::Call;

/// The namespaces of the per-identity keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    /// The incoming (a-) leg's identity: its Call-ID and the caller's From-tag.
    ALeg,
    /// One dialog of an outgoing (b-) leg, by the tag its remote peer chose.
    BDialog,
    /// An outgoing leg's own From-tag. A message whose peer tag is this tag is
    /// that leg's request coming back to this B2BUA; only an incoming leg with
    /// the same identity can be its target.
    BLocal,
    /// An outgoing leg by Call-ID alone (the tag is not part of the key): a
    /// response that names no dialog yet.
    BLeg,
}

/// Write the `kind` key of `(call_id, tag)` into `buf`, replacing its content.
pub(crate) fn write_key(buf: &mut String, kind: KeyKind, call_id: &str, tag: &str) {
    buf.clear();
    let (prefix, tagged) = match kind {
        KeyKind::ALeg => ("a:", true),
        KeyKind::BDialog => ("b:", true),
        KeyKind::BLocal => ("bl:", true),
        KeyKind::BLeg => ("b:", false),
    };
    buf.push_str(prefix);
    buf.push_str(call_id);
    if tagged {
        buf.push('|');
        buf.push_str(tag);
    }
}

/// The length `write_key` needs at most for `(call_id, tag)`.
pub(crate) fn key_capacity(call_id: &str, tag: &str) -> usize {
    "bl:|".len() + call_id.len() + tag.len()
}

fn key(kind: KeyKind, call_id: &str, tag: &str) -> String {
    let mut buf = String::with_capacity(key_capacity(call_id, tag));
    write_key(&mut buf, kind, call_id, tag);
    buf
}

/// The incoming (a-) leg's key.
pub(crate) fn a_leg_key(call_id: &str, caller_tag: &str) -> String {
    key(KeyKind::ALeg, call_id, caller_tag)
}

/// An outgoing dialog's key.
pub(crate) fn b_dialog_key(call_id: &str, peer_tag: &str) -> String {
    key(KeyKind::BDialog, call_id, peer_tag)
}

/// An outgoing leg's Call-ID-only key.
pub(crate) fn b_leg_key(call_id: &str) -> String {
    key(KeyKind::BLeg, call_id, "")
}

/// An outgoing leg's own-tag key.
pub(crate) fn b_local_key(call_id: &str, local_tag: &str) -> String {
    key(KeyKind::BLocal, call_id, local_tag)
}

/// The callback context a decision attached to the call.
pub fn callback_key(ctx: &str) -> String {
    format!("ctx:{ctx}")
}

/// Every index key a call owns.
pub fn call_index_keys(call: &Call) -> Vec<String> {
    let mut keys = vec![a_leg_key(&call.a_leg.call_id, &call.a_leg.from_tag)];
    for b in &call.b_legs {
        keys.push(b_local_key(&b.call_id, &b.from_tag));
        keys.push(b_leg_key(&b.call_id));
        for d in &b.dialogs {
            if !d.sip.remote_tag.is_empty() {
                keys.push(b_dialog_key(&b.call_id, &d.sip.remote_tag));
            }
        }
    }
    if let Some(ctx) = &call.callback_context {
        keys.push(callback_key(ctx));
    }
    keys
}

/// Best-effort structural extraction of [`call_index_keys`] from a schema-tolerant
/// [`serde_json::Value`] projection of a call (e.g. `serde_json::to_value(&call)`).
/// Walks the same field path as [`call_index_keys`]; missing/wrong-typed fields
/// are skipped. A correctly-shaped value always yields the identical key set.
///
/// Field names are the crate's serde names (snake_case) — the msgpack body
/// itself is positional and carries no names, so the schema-tolerant walk only
/// makes sense over this JSON projection.
pub fn call_index_keys_from_unknown(state: &serde_json::Value) -> Vec<String> {
    let obj = match state.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    fn str_field<'v>(v: &'v serde_json::Value, k: &str) -> Option<&'v str> {
        v.get(k).and_then(|x| x.as_str())
    }
    let mut keys = Vec::new();

    if let Some(a_leg) = obj.get("a_leg") {
        if let (Some(call_id), Some(from_tag)) =
            (str_field(a_leg, "call_id"), str_field(a_leg, "from_tag"))
        {
            keys.push(a_leg_key(call_id, from_tag));
        }
    }

    if let Some(b_legs) = obj.get("b_legs").and_then(|v| v.as_array()) {
        for b in b_legs {
            let Some(cid) = str_field(b, "call_id") else { continue };
            if let Some(local_tag) = str_field(b, "from_tag") {
                keys.push(b_local_key(cid, local_tag));
            }
            keys.push(b_leg_key(cid));
            for d in b.get("dialogs").and_then(|v| v.as_array()).into_iter().flatten() {
                let remote_tag = d.get("sip").and_then(|s| str_field(s, "remote_tag"));
                if let Some(remote_tag) = remote_tag.filter(|t| !t.is_empty()) {
                    keys.push(b_dialog_key(cid, remote_tag));
                }
            }
        }
    }

    if let Some(ctx) = obj.get("callback_context").and_then(|v| v.as_str()) {
        keys.push(callback_key(ctx));
    }
    keys
}

/// What a message names its call by, when it carries no `callRef` of ours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexLookup<'a> {
    /// A CANCEL: the Call-ID and From-tag of the INVITE it cancels (RFC 3261
    /// §9.1). Only an INVITE this B2BUA received can be cancelled toward it,
    /// so the From-tag is a remote party's tag: the caller's on the incoming
    /// leg, or an outgoing dialog's peer tag for a re-INVITE sent from there.
    /// In a spiral (§16.3) another call's outgoing leg carries this Call-ID
    /// and From-tag as its own; it is never the CANCEL's target.
    Cancel { call_id: &'a str, from_tag: &'a str },
    /// An in-dialog request (its From-tag) or a response (its To-tag): the tag
    /// the remote peer of the dialog owns, on either side of the B2BUA.
    Peer { call_id: &'a str, tag: &'a str },
}

/// A call an index lookup found, and the namespace of the key that matched:
/// which side of the B2BUA owns the identity the message named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexHit {
    pub call_ref: String,
    pub kind: KeyKind,
}

/// One step of an index lookup, probed in order until one decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// A hit on this key is the answer.
    Resolve(KeyKind),
    /// A hit on this key ends the lookup with no call.
    Refuse(KeyKind),
}

const CANCEL_PROBES: [Probe; 2] = [Probe::Resolve(KeyKind::ALeg), Probe::Resolve(KeyKind::BDialog)];
const PEER_PROBES: [Probe; 4] = [
    Probe::Resolve(KeyKind::ALeg),
    Probe::Resolve(KeyKind::BDialog),
    Probe::Refuse(KeyKind::BLocal),
    Probe::Resolve(KeyKind::BLeg),
];

impl IndexLookup<'_> {
    /// The probes that resolve this lookup, in order. A CANCEL never resolves
    /// by an outgoing leg's Call-ID alone or by its own tag. A peer tag that
    /// is an outgoing leg's own tag stops before the Call-ID-only key: the
    /// message belongs to the incoming leg with that identity, and no such
    /// leg is indexed.
    pub fn probes(&self) -> &'static [Probe] {
        match self {
            IndexLookup::Cancel { .. } => &CANCEL_PROBES,
            IndexLookup::Peer { .. } => &PEER_PROBES,
        }
    }

    /// Write this lookup's `kind` key into `buf`, replacing its content.
    pub fn write_key(&self, kind: KeyKind, buf: &mut String) {
        let (call_id, tag) = self.identity();
        write_key(buf, kind, call_id, tag);
    }

    /// A buffer that holds any of this lookup's keys without growing.
    pub fn key_buffer(&self) -> String {
        let (call_id, tag) = self.identity();
        String::with_capacity(key_capacity(call_id, tag))
    }

    /// The leg of `call` that owns this lookup's `kind` key: the incoming
    /// leg for [`KeyKind::ALeg`], the outgoing leg on this Call-ID for
    /// [`KeyKind::BLeg`], and for [`KeyKind::BDialog`] the outgoing leg on
    /// this Call-ID holding a dialog whose remote tag is this tag. `None`
    /// when `call` holds no such leg, or for [`KeyKind::BLocal`], which
    /// never resolves a call.
    pub fn owning_leg<'c>(&self, kind: KeyKind, call: &'c Call) -> Option<&'c str> {
        let (call_id, tag) = self.identity();
        let mut outgoing = call.b_legs.iter().filter(|b| b.call_id == call_id);
        let leg = match kind {
            KeyKind::ALeg => {
                let a = &call.a_leg;
                return (a.call_id == call_id && a.from_tag == tag).then_some(a.leg_id.as_str());
            }
            KeyKind::BDialog => {
                outgoing.find(|b| b.dialogs.iter().any(|d| d.sip.remote_tag == tag))
            }
            KeyKind::BLeg => outgoing.next(),
            KeyKind::BLocal => None,
        };
        leg.map(|b| b.leg_id.as_str())
    }

    fn identity(&self) -> (&str, &str) {
        match *self {
            IndexLookup::Cancel { call_id, from_tag } => (call_id, from_tag),
            IndexLookup::Peer { call_id, tag } => (call_id, tag),
        }
    }
}
