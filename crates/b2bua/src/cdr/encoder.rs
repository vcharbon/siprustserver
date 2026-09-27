//! The CDR encoder: the pure function from a terminated [`Call`] to the bytes a
//! transport sink publishes. A sink owns delivery; the encoder owns the format.
//! [`JsonRecordEncoder`] is the default: the [`CdrRecord`](super::CdrRecord) of
//! [`build_record`] as JSON.

use call::Call;

use super::build_record;

/// Why an encoder produced no bytes for a call; a sink counts the record as
/// dropped.
pub type CdrEncodeError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Encodes one terminated call into its published record. Stateless: the same
/// `call` and `terminated_at` yield the same bytes, so a sink may retry or
/// re-encode freely.
///
/// `terminated_at` is the discharging node's raw clock read, passed through
/// unclamped: after a takeover under clock skew it may precede
/// `call.created_at` (minted on the origin node). An encoder that publishes a
/// duration clamps it itself, as [`build_record`] does and flags.
pub trait CdrEncoder: Send + Sync {
    fn encode(&self, call: &Call, terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError>;
}

/// The default format: [`build_record`] serialised as compact JSON.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonRecordEncoder;

impl CdrEncoder for JsonRecordEncoder {
    fn encode(&self, call: &Call, terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
        Ok(serde_json::to_vec(&build_record(call, terminated_at))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::B2buaConfig;
    use crate::initial_invite::build_initial_call;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use std::net::SocketAddr;

    fn a_call(created_at: i64) -> Call {
        let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
            Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-enc\r\n\
            Max-Forwards: 70\r\n\
            From: <sip:alice@example.com>;tag=alicetag\r\n\
            To: <sip:bob@example.com>\r\n\
            Call-ID: cdr-enc@10.0.0.9\r\n\
            CSeq: 1 INVITE\r\n\
            Contact: <sip:alice@10.0.0.9:5060>\r\n\
            Content-Length: 0\r\n\r\n";
        let req = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected a request"),
        };
        let cfg = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        build_initial_call(
            &req,
            SocketAddr::from(([10, 0, 0, 9], 5060)),
            &cfg,
            &sip_txn::IdGen::seeded(1),
            created_at,
        )
    }

    /// The default encoder publishes exactly the bytes of `serde_json::to_vec`
    /// over [`build_record`], in order and on a skew-clamped record alike.
    #[test]
    fn the_default_encoder_is_the_json_record_byte_for_byte() {
        let call = a_call(1_000);
        for terminated_at in [2_000, 500] {
            let expected = serde_json::to_vec(&build_record(&call, terminated_at)).unwrap();
            let got = JsonRecordEncoder.encode(&call, terminated_at).expect("encodes");
            assert_eq!(got, expected, "terminated_at={terminated_at}");
        }
    }

    /// The encoder is a trait object a sink holds behind an `Arc`.
    #[test]
    fn the_default_encoder_is_usable_as_a_shared_trait_object() {
        let enc: std::sync::Arc<dyn CdrEncoder> = std::sync::Arc::new(JsonRecordEncoder);
        let call = a_call(1_000);
        assert_eq!(
            enc.encode(&call, 2_000).expect("encodes"),
            JsonRecordEncoder.encode(&call, 2_000).expect("encodes")
        );
    }
}
