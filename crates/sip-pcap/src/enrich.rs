//! The enrichment pass: recompute every derived field of a flows document
//! from the message bytes it carries and from its own structure.
//!
//! One pass serves both producers. `sipflow --json` runs it over a freshly
//! built document; a tool that TRANSFORMS a document (anonymizing it,
//! restricting it to selected groups) runs it again afterwards, and the
//! enrichment is right by construction rather than by a second rewrite pass
//! kept in step by hand. Nothing here reads state the document does not carry.

use sip_message::header::HeaderName;
use sip_message::parser::SipParser;
use sip_message::CustomParser;

use crate::callfacts;
use crate::doc::{FlowsDoc, EMIT_SCHEMA_VERSION};
use crate::msgfacts;

/// What the enrichment needs beyond the document itself.
#[derive(Debug, Clone, Default)]
pub struct EnrichOptions {
    /// Headers `msgs[].headers` projects, by identity. Order is preserved into
    /// the document's `emit_headers`.
    pub headers: Vec<HeaderName>,
}

impl EnrichOptions {
    /// Resolve an allow-list given as wire names — casing and compact forms
    /// collapse, so `r` and `Refer-To` name the same header once.
    pub fn with_headers<S: AsRef<str>>(names: &[S]) -> Self {
        let mut headers: Vec<HeaderName> = Vec::new();
        for name in names {
            let name = HeaderName::from(name.as_ref().trim());
            if !headers.iter().any(|h| h.same_header(&name)) {
                headers.push(name);
            }
        }
        Self { headers }
    }
}

/// Recompute `doc`'s schema version and every derived field.
///
/// Fails loudly on a message whose bytes no longer parse: a document that
/// cannot be re-derived must not be emitted with a stale enrichment.
pub fn enrich(doc: &mut FlowsDoc, opts: &EnrichOptions) -> Result<(), String> {
    let parser = CustomParser::new();
    doc.schema = EMIT_SCHEMA_VERSION;
    doc.emit_headers = opts.headers.iter().map(|h| h.as_wire_str().to_string()).collect();

    for (li, leg) in doc.legs.iter_mut().enumerate() {
        for (mi, msg) in leg.msgs.iter_mut().enumerate() {
            let raw = msg.payload.bytes().map_err(|e| at(li, mi, &e))?;
            let parsed = parser.parse(&raw).map_err(|e| at(li, mi, &e.reason))?;
            msgfacts::apply(&parsed, &opts.headers, msg);
        }
        callfacts::mark_repeats(leg);
    }

    let legs = std::mem::take(&mut doc.legs);
    for group in &mut doc.groups {
        callfacts::summarize(&legs, group);
    }
    doc.legs = legs;
    Ok(())
}

/// Re-enrich a serialized document — the entry point for a tool that rewrote
/// one and must bring its derivations back in step with its bytes.
pub fn enrich_str(json: &str, opts: &EnrichOptions) -> Result<String, String> {
    let mut doc: FlowsDoc = serde_json::from_str(json).map_err(|e| format!("flows JSON: {e}"))?;
    enrich(&mut doc, opts)?;
    serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())
}

fn at(leg: usize, msg: usize, reason: &str) -> String {
    format!("leg {leg} msg {msg}: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Summary;
    use crate::flow::{build_flows, FlowConfig};
    use crate::{DecodeStats, Datagram};

    fn dg(ts_us: u64, src: &str, dst: &str, payload: &[u8]) -> Datagram {
        Datagram {
            ts_us,
            src: src.parse().unwrap(),
            dst: dst.parse().unwrap(),
            payload: payload.to_vec(),
        probe: 0,
        }
    }

    const INVITE: &[u8] = b"INVITE sip:+33123@h SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
Max-Forwards: 70\r\n\
From: <sip:0033900@h>;tag=f1\r\n\
To: <sip:+33123@h>\r\n\
Call-ID: enrich-1\r\n\
CSeq: 1 INVITE\r\n\
X-Api-Call: call-9\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 3\r\n\r\nv=0";

    const OK: &[u8] = b"SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
From: <sip:0033900@h>;tag=f1\r\n\
To: <sip:+33123@h>;tag=t1\r\n\
Call-ID: enrich-1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";

    fn doc() -> FlowsDoc {
        let datagrams = vec![
            dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", INVITE),
            dg(9_000, "10.0.0.2:5060", "10.0.0.1:5060", OK),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        crate::emit::flows_to_doc(
            &flows,
            &DecodeStats::default(),
            &EnrichOptions::with_headers(&["X-Api-Call"]),
        )
        .expect("the model enriches")
    }

    /// The emitted document carries the schema version, the allow-list it was
    /// asked for, and the per-call summary every consumer needs.
    #[test]
    fn a_built_document_is_enriched_end_to_end() {
        let d = doc();
        assert_eq!(d.schema, EMIT_SCHEMA_VERSION);
        assert_eq!(d.emit_headers, vec!["X-Api-Call".to_string()]);
        let group = &d.groups[0];
        assert_eq!(group.t0_us, 1_000);
        assert_eq!(group.final_status, Some(200));
        assert_eq!(group.final_us, Some(9_000));
        let initial = group.initial_invite.expect("an INVITE opens the call");
        assert_eq!((initial.leg, initial.msg), (0, 0));
        assert_eq!(group.methods["INVITE"].requests, 1);
        assert_eq!(group.methods["INVITE"].content_types, vec!["application/sdp".to_string()]);
        let m = &d.legs[0].msgs[0];
        assert_eq!(m.headers.len(), 1);
        assert_eq!(m.identities.to.digits.as_deref(), Some("33123"));
        assert_eq!(m.via[0].branch.as_deref(), Some("z9hG4bK1"));
    }

    /// Re-enriching a serialized document reproduces it: the pass is a pure
    /// function of the bytes, so a transformed document can always be brought
    /// back in step.
    #[test]
    fn re_enriching_a_serialized_document_is_idempotent() {
        let opts = EnrichOptions::with_headers(&["X-Api-Call"]);
        let once = serde_json::to_string_pretty(&doc()).unwrap();
        let twice = enrich_str(&once, &opts).unwrap();
        assert_eq!(once, twice);
    }

    /// A document whose enrichment was stripped (a schema-4 producer, or a
    /// rewrite) reads, and the pass puts the derivations back.
    #[test]
    fn a_document_without_enrichment_reads_and_is_re_derived() {
        let mut value: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&doc()).unwrap(),
        )
        .unwrap();
        value["schema"] = serde_json::json!(4);
        for leg in value["legs"].as_array_mut().unwrap() {
            for m in leg["msgs"].as_array_mut().unwrap() {
                let m = m.as_object_mut().unwrap();
                for key in ["via", "headers", "identities", "body", "repeat_of"] {
                    m.remove(key);
                }
            }
        }
        let back: FlowsDoc =
            serde_json::from_str(&enrich_str(&value.to_string(), &EnrichOptions::default()).unwrap())
                .unwrap();
        assert_eq!(back.schema, EMIT_SCHEMA_VERSION);
        assert!(back.emit_headers.is_empty(), "no allow-list was asked for");
        assert_eq!(back.legs[0].msgs[0].identities.from.digits.as_deref(), Some("33900"));
        assert!(matches!(back.legs[0].msgs[0].summary, Summary::Request { .. }));
        assert_eq!(back.groups[0].final_status, Some(200));
    }
}
