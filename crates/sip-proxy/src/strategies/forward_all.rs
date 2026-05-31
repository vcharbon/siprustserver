//! [`ForwardAllStrategy`] — port of `strategies/ForwardAll.ts`. Trivial dev/test
//! strategy: every new dialog goes to one statically-configured backend.
//! Stickiness is a plain `target=host:port` Record-Route param (no HMAC — this
//! backs the transit-only suite). On an in-dialog request it parses that param
//! back to `Forward { target }`; a missing/malformed param yields `Unknown` and
//! the core falls back to `select_for_new_dialog` (still the static target).

use async_trait::async_trait;
use sip_message::SipMessage;

use crate::addr::ProxyAddr;
use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};

const TARGET_PARAM: &str = "target";

/// Forwards everything to one static target.
pub struct ForwardAllStrategy {
    target: ProxyAddr,
}

impl ForwardAllStrategy {
    pub fn new(target: ProxyAddr) -> Self {
        Self { target }
    }
}

#[async_trait]
impl RoutingStrategy for ForwardAllStrategy {
    fn name(&self) -> &str {
        "ForwardAll"
    }

    async fn select_for_new_dialog(&self, _msg: &SipMessage, _opts: SelectOpts) -> Result<ProxyAddr, SelectError> {
        Ok(self.target.clone())
    }

    async fn decode_stickiness(&self, route_param: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        match route_param.get(TARGET_PARAM).and_then(|raw| ProxyAddr::parse(raw)) {
            Some(target) => DecodeResult::Forward { target, is_emergency: false },
            None => DecodeResult::Unknown { is_emergency: false },
        }
    }

    fn encode_stickiness(&self, target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        let mut params = RouteParams::new();
        params.insert(TARGET_PARAM.to_string(), target.to_string());
        Some(params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::parser::custom::CustomParser;
    use sip_message::SipParser;

    fn invite() -> SipMessage {
        let raw = b"INVITE sip:bob@10.0.0.3:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK1\r\n\
From: <sip:alice@a>;tag=t1\r\n\
To: <sip:bob@b>\r\n\
Call-ID: c1@h\r\n\
CSeq: 1 INVITE\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n";
        CustomParser::default().parse(raw).unwrap()
    }

    #[tokio::test]
    async fn selects_static_target_and_round_trips_cookie() {
        let s = ForwardAllStrategy::new(ProxyAddr::new("10.0.0.3", 5070));
        let msg = invite();
        assert_eq!(s.select_for_new_dialog(&msg, SelectOpts::default()).await.unwrap(), ProxyAddr::new("10.0.0.3", 5070));

        let params = s.encode_stickiness(&ProxyAddr::new("10.0.0.3", 5070), &msg).unwrap();
        assert_eq!(params.get("target").unwrap(), "10.0.0.3:5070");
        match s.decode_stickiness(&params, &msg).await {
            DecodeResult::Forward { target, .. } => assert_eq!(target, ProxyAddr::new("10.0.0.3", 5070)),
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_or_bad_cookie_is_unknown() {
        let s = ForwardAllStrategy::new(ProxyAddr::new("10.0.0.3", 5070));
        let msg = invite();
        assert!(matches!(s.decode_stickiness(&RouteParams::new(), &msg).await, DecodeResult::Unknown { .. }));
        let mut bad = RouteParams::new();
        bad.insert("target".into(), "garbage".into());
        assert!(matches!(s.decode_stickiness(&bad, &msg).await, DecodeResult::Unknown { .. }));
    }
}
