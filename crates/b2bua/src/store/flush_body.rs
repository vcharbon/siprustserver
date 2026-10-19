//! The body a replication flush writes: the call encoded with the store's
//! authoritative topology embedded, not only in the frame meta. The `Call` a
//! flush is handed may be a pre-`update` clone whose `(p,b)` counters are one
//! bump stale, and a node that hydrates from the body branches at the
//! topology the body holds (ADR-0014).

use call::{Call, CallBodyCodec, CallTopology, MsgpackCodec};

use crate::metrics::B2buaMetrics;

/// `call` encoded with `stored` as its topology when the two differ; `None`,
/// counted and logged, when the body does not encode or nests past the bound
/// every peer decodes under, and the flush is skipped.
pub(super) fn encode(
    codec: &MsgpackCodec,
    metrics: &B2buaMetrics,
    call: &Call,
    stored: &Option<CallTopology>,
) -> Option<Vec<u8>> {
    let encoded = match (call.topology.as_ref(), stored.as_ref()) {
        (Some(t), Some(s)) if t != s => {
            let mut consistent = call.clone();
            consistent.topology = Some(s.clone());
            codec.encode(&consistent)
        }
        _ => codec.encode(call),
    };
    match encoded {
        Ok(body) => Some(body),
        Err(e) => {
            metrics.bump_repl_flush_unencodable();
            tracing::error!(call_ref = %call.call_ref, error = %e, "flush skipped: the call body did not encode");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call whose service JSON nests past the bound is not flushed, and
    /// the skip is counted.
    #[test]
    fn a_body_nested_past_the_bound_is_counted_and_skipped() {
        let metrics = B2buaMetrics::new();
        let codec = MsgpackCodec::new();
        let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let invite = crate::router::test_support::invite("w0", "w1", "deep");
        let src = crate::router::test_support::src();
        let ids = sip_txn::IdGen::seeded(1);
        let base = crate::initial_invite::build_initial_call(&invite, src, &config, &ids, 0);
        let deep = (0..200).fold(serde_json::Value::Null, |v, _| serde_json::json!([v]));
        let call = call::helpers::set_call_ext(base.clone(), "svc", Some(deep));
        assert_eq!(encode(&codec, &metrics, &call, &None), None);
        assert_eq!(metrics.repl_flush_unencodable_total(), 1);
        let shallow = call::helpers::set_call_ext(base, "svc", Some(1.into()));
        assert!(encode(&codec, &metrics, &shallow, &None).is_some());
        assert_eq!(metrics.repl_flush_unencodable_total(), 1);
    }
}
