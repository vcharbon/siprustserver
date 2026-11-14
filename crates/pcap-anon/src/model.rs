//! The flows document this crate reads and writes: `sip_pcap::doc`
//! (schema 5) under the crate's short names, read loudly and written back
//! through `sip-pcap`'s enrichment.
//!
//! Ingest is loud: an unknown `schema` value, a message carrying other than
//! exactly one payload form, a payload that does not decode, or a structural
//! invariant violation is an error, never a silent partial parse. A message's
//! wire bytes ([`wire`]) are the source of truth; a rewrite sets them
//! ([`set_wire`]) and the payload form follows the bytes.
//!
//! The document's enrichment (structured headers, identities, Via chains,
//! dialog references, body layout, per-call summaries, repeats) is re-derived
//! from the bytes on the way out ([`to_json_string`]), so a transformed
//! document never carries an enrichment its bytes no longer justify.

use sip_pcap::doc::Payload;
pub use sip_pcap::doc::{
    CSeqJson as CSeq, DecodeStatsJson as DecodeStats, Evidence, FlowStatsJson as FlowStats,
    FlowsDoc as Flows, GroupJson as Group, HopJson as Hop, InviteJson as Invite, LegJson as Leg,
    MsgJson as Msg, Party, Summary,
};

use crate::reading::Reading;

/// The one `sip-pcap` emit schema version this crate understands.
pub const SUPPORTED_SCHEMA: u64 = sip_pcap::doc::EMIT_SCHEMA_VERSION as u64;

/// The payload keys of a message, one form per entry.
const PAYLOAD_FORMS: [&[&str]; 3] = [&["raw"], &["head", "body_b64"], &["raw_b64"]];

/// The exact wire bytes of `msg`. [`ingest_str`] refuses a document whose
/// payload does not decode, so every message of an ingested document has them.
pub fn wire(msg: &Msg) -> Vec<u8> {
    msg.payload.bytes().expect("ingest_str refuses a payload that does not decode")
}

/// Set the wire bytes of `msg`; the payload form is the one the bytes take.
pub fn set_wire(msg: &mut Msg, bytes: &[u8]) {
    msg.payload = Payload::of_datagram(bytes);
}

/// Whether two hops are the same unordered socket pair.
pub fn same_pair(x: &Hop, y: &Hop) -> bool {
    (x.a == y.a && x.b == y.b) || (x.a == y.b && x.b == y.a)
}

/// Ingest failure — always loud and specific.
#[derive(Debug)]
pub enum IngestError {
    /// The input is not JSON at all.
    Json(serde_json::Error),
    /// The top-level `schema` value is absent or not [`SUPPORTED_SCHEMA`].
    UnsupportedSchema { found: Option<u64> },
    /// schema matched but the body does not fit the document.
    Model(serde_json::Error),
    /// Parsed cleanly but violates a cross-field invariant of the document.
    Invariant(String),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Json(e) => write!(f, "flows input is not valid JSON: {e}"),
            IngestError::UnsupportedSchema { found: Some(s) } => write!(
                f,
                "unsupported flows schema {s} (this pcap-anon build supports schema {SUPPORTED_SCHEMA} only; \
                 regenerate with a matching sipflow --json)"
            ),
            IngestError::UnsupportedSchema { found: None } => write!(
                f,
                "flows input has no numeric top-level \"schema\" field (expected schema {SUPPORTED_SCHEMA} \
                 from sipflow --json)"
            ),
            IngestError::Model(e) => {
                write!(f, "flows input does not match the schema-{SUPPORTED_SCHEMA} model: {e}")
            }
            IngestError::Invariant(msg) => write!(f, "flows model invariant violated: {msg}"),
        }
    }
}

impl std::error::Error for IngestError {}

/// Parse a `sipflow --json` document, rejecting unknown schema versions and
/// structurally broken inputs.
pub fn ingest_str(json: &str) -> Result<Flows, IngestError> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(IngestError::Json)?;
    let schema = value.get("schema").and_then(serde_json::Value::as_u64);
    if schema != Some(SUPPORTED_SCHEMA) {
        return Err(IngestError::UnsupportedSchema { found: schema });
    }
    one_payload_form_each(&value)?;
    let flows: Flows = serde_json::from_value(value).map_err(IngestError::Model)?;
    validate(&flows)?;
    Ok(flows)
}

/// Every message of the document carries exactly one payload form: its keys
/// are one entry of [`PAYLOAD_FORMS`], whole, and no key of another.
fn one_payload_form_each(value: &serde_json::Value) -> Result<(), IngestError> {
    let legs = value.get("legs").and_then(serde_json::Value::as_array).into_iter().flatten();
    for (li, leg) in legs.enumerate() {
        let msgs = leg.get("msgs").and_then(serde_json::Value::as_array).into_iter().flatten();
        for (mi, msg) in msgs.enumerate() {
            let has = |key: &&str| msg.get(*key).is_some();
            let forms = PAYLOAD_FORMS.iter().filter(|form| form.iter().any(has)).count();
            let whole = PAYLOAD_FORMS.iter().any(|form| form.iter().all(has));
            if forms != 1 || !whole {
                return Err(IngestError::Invariant(format!(
                    "leg {li} msg {mi}: message payload must carry exactly one of `raw` | \
                     `head`+`body_b64` | `raw_b64`"
                )));
            }
        }
    }
    Ok(())
}

/// Cross-field invariants of the document: every payload decodes, every
/// `msg.hop` resolves into its leg's hop chain and every group member is a
/// real leg index.
fn validate(flows: &Flows) -> Result<(), IngestError> {
    for (li, leg) in flows.legs.iter().enumerate() {
        for (mi, msg) in leg.msgs.iter().enumerate() {
            if let Err(e) = msg.payload.bytes() {
                return Err(IngestError::Invariant(format!("leg {li} msg {mi}: payload: {e}")));
            }
            if msg.hop >= leg.hops.len() {
                return Err(IngestError::Invariant(format!(
                    "leg {li} msg {mi}: hop index {} out of range ({} hops)",
                    msg.hop,
                    leg.hops.len()
                )));
            }
        }
    }
    for (gi, group) in flows.groups.iter().enumerate() {
        for &leg in &group.legs {
            if leg >= flows.legs.len() {
                return Err(IngestError::Invariant(format!(
                    "group {gi}: leg index {leg} out of range ({} legs)",
                    flows.legs.len()
                )));
            }
        }
    }
    Ok(())
}

/// The document as a flows document (pretty-printed, trailing newline), its
/// enrichment re-derived from the bytes it now carries under `reading`, the
/// one the capture was extracted under — the one exit from this crate to a
/// flows document, so a rewritten capture's projections are right by
/// construction. Each payload is written in the form its bytes take
/// ([`set_wire`]), whatever form it was read in. [`ingest_str`] reads the
/// output back.
pub fn to_json_string(flows: &Flows, reading: &Reading) -> Result<String, String> {
    let mut doc = flows.clone();
    for msg in doc.legs.iter_mut().flat_map(|leg| leg.msgs.iter_mut()) {
        let bytes = wire(msg);
        set_wire(msg, &bytes);
    }
    let opts =
        sip_pcap::enrich::EnrichOptions::with_headers(&doc.emit_headers, reading.sip.clone());
    sip_pcap::enrich::enrich(&mut doc, &opts)?;
    let mut out = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests;
