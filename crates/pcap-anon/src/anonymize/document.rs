//! The document after the passes are compiled: every datagram, its body
//! parts the anonymiser does not understand redacted first, and every model
//! string that mirrors one — a Call-ID and a tag through the identifier
//! mapping as on the wire, every other string through the whole rewrite. What
//! the document derived from the raw bytes goes: its enrichment
//! (`sip_pcap::enrich::strip`, re-derived on the way out) and the reasons of
//! its failed header readings, which quote the raw identities (their counts
//! stay).

use crate::identities::BodyTypes;
use crate::model::{self, Evidence, Flows, Invite, Summary};

use super::bodies;

/// A datagram's rewrite: its bytes, the CSeq value `(seq, method)` it
/// states kept.
pub(super) type BytesRewrite<'a> = &'a dyn Fn(&[u8], Option<(&[u8], &[u8])>) -> Vec<u8>;

/// The rewrites a document goes through.
pub(super) struct Rewrite<'a> {
    /// A datagram's bytes.
    pub(super) bytes: BytesRewrite<'a>,
    /// A model string.
    pub(super) text: &'a dyn Fn(&str) -> String,
    /// A Call-ID or a tag of the model.
    pub(super) identifier: &'a dyn Fn(&str) -> String,
}

/// Rewrite `flows` in place, the body parts of `types` read rather than
/// redacted; the count of body parts redacted.
pub(super) fn rewrite(flows: &mut Flows, r: &Rewrite<'_>, types: &BodyTypes) -> usize {
    let mut redacted_parts = 0;
    sip_pcap::enrich::strip(flows);
    flows.flow_stats.header_failures.clear();
    for leg in &mut flows.legs {
        leg.call_id = (r.identifier)(&leg.call_id);
        for hop in &mut leg.hops {
            hop.a = (r.text)(&hop.a);
            hop.b = (r.text)(&hop.b);
        }
        if let Some(Invite { ruri, from_uri, to_uri, cseq: _ }) = &mut leg.invite {
            *ruri = (r.text)(ruri);
            *from_uri = (r.text)(from_uri);
            *to_uri = (r.text)(to_uri);
        }
        if let Some(t) = &mut leg.terminated_by {
            *t = (r.text)(t);
        }
        for tok in &mut leg.tokens {
            *tok = (r.text)(tok);
        }
        for msg in &mut leg.msgs {
            let cseq = msg.summary.cseq();
            let seq = cseq.seq.to_string();
            let (raw, redacted) = bodies::redact_unknown(&model::wire(msg), types);
            redacted_parts += redacted;
            let rewritten = (r.bytes)(&raw, Some((seq.as_bytes(), cseq.method.as_bytes())));
            model::set_wire(msg, &rewritten);
            msg.src = (r.text)(&msg.src);
            msg.dst = (r.text)(&msg.dst);
            rewrite_summary(&mut msg.summary, r.text, r.identifier);
        }
    }
    for group in &mut flows.groups {
        for ev in &mut group.evidence {
            match ev {
                Evidence::SharedToken { token, .. } => *token = (r.text)(token),
                // The param VALUE is capture data (an IMS icid is globally
                // unique); the header/param NAMES are protocol vocabulary.
                Evidence::SharedHeaderParam { token, .. } => *token = (r.text)(token),
                // The prefix is the relay convention (`1-`), not capture data;
                // the sockets name real hosts and follow the same mapping the
                // legs' own addresses do.
                Evidence::DerivedCallId { as_socket, peer_socket, .. } => {
                    *as_socket = (r.text)(as_socket);
                    *peer_socket = (r.text)(peer_socket);
                }
                Evidence::IdentityAdjacency { shared_host, .. } => {
                    *shared_host = (r.text)(shared_host)
                }
            }
        }
    }
    redacted_parts
}

#[cfg(test)]
mod fields_tests;

/// Apply the substitution to the summary's mirrored strings.
fn rewrite_summary(
    summary: &mut Summary,
    rw: &dyn Fn(&str) -> String,
    rw_id: &dyn Fn(&str) -> String,
) {
    match summary {
        Summary::Request { method, uri, from, to, .. } => {
            *method = rw(method);
            *uri = rw(uri);
            for p in [from, to] {
                p.uri = rw(&p.uri);
                if let Some(tag) = &mut p.tag {
                    *tag = rw_id(tag);
                }
            }
        }
        // The reason phrase is protocol text, never anonymised.
        Summary::Response { from, to, .. } => {
            for p in [from, to] {
                p.uri = rw(&p.uri);
                if let Some(tag) = &mut p.tag {
                    *tag = rw_id(tag);
                }
            }
        }
    }
}
