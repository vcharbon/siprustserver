//! [`ForwardAllStrategy`] — trivial dev/test
//! strategy: every new dialog goes to one statically-configured backend.
//! Stickiness is a plain `target=host:port` Record-Route param (no HMAC — this
//! backs the transit-only suite). On an in-dialog request it parses that param
//! back to `Forward { target }`; a missing/malformed param yields `Unknown` and
//! the core falls back to `select_for_new_dialog` (still the static target).
//! The param is unsigned, so any sender names the host an in-dialog request is
//! forwarded to: the external face's trust boundary (ADR-0048) assumes a
//! strategy whose cookie is authenticated, which this one is not.

use sip_message::SipMessage;

use crate::addr::ProxyAddr;
use crate::strategy::{
    DecodeResult, RankPlacement, RouteParams, RoutingStrategy, SelectError, SelectOpts,
};

const TARGET_PARAM: &str = "target";

pub struct ForwardAllStrategy {
    target: ProxyAddr,
}

impl ForwardAllStrategy {
    pub fn new(target: ProxyAddr) -> Self {
        Self { target }
    }
}

impl RoutingStrategy for ForwardAllStrategy {
    fn name(&self) -> &str {
        "ForwardAll"
    }

    fn select_for_new_dialog(
        &self,
        _msg: &SipMessage,
        _opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError> {
        Ok(self.target.clone())
    }

    fn placement_by_rank(&self, _msg: &SipMessage) -> RankPlacement {
        RankPlacement::Node(self.target.clone())
    }

    fn decode_stickiness(&self, route_param: &RouteParams, _msg: &SipMessage) -> DecodeResult {
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
    /// The cookie names an address, never a worker id.
    fn stickiness_primary<'p>(&self, _params: &'p RouteParams) -> Option<&'p str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::header::{ParamValue, Uri};
    use sip_message::parser::custom::CustomParser;
    use sip_message::sip_str::SipStr;
    use sip_message::SipParser;

    fn invite() -> SipMessage {
        let raw = b"INVITE sip:bob@10.0.0.3:5070 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK1\r\n\
From: <sip:alice@a>;tag=t1\r\n\
To: <sip:bob@b>\r\n\
Call-ID: c1@h\r\n\
CSeq: 1 INVITE\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n";
        CustomParser::new().parse(raw).unwrap()
    }

    #[test]
    fn selects_static_target_and_round_trips_cookie() {
        let s = ForwardAllStrategy::new(ProxyAddr::new("10.0.0.3", 5070));
        let msg = invite();
        assert_eq!(
            s.select_for_new_dialog(&msg, SelectOpts::default()).unwrap(),
            ProxyAddr::new("10.0.0.3", 5070)
        );

        let params = s.encode_stickiness(&ProxyAddr::new("10.0.0.3", 5070), &msg).unwrap();
        assert_eq!(params.get("target").unwrap(), "10.0.0.3:5070");
        match s.decode_stickiness(&params, &msg) {
            DecodeResult::Forward { target, .. } => {
                assert_eq!(target, ProxyAddr::new("10.0.0.3", 5070))
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    /// An IPv6 target rides the cookie bracketed, which a Route URI's
    /// parameter holds as written (RFC 3261 `param-unreserved`), and decodes
    /// back to the bare address.
    #[test]
    fn an_ipv6_target_round_trips_through_a_route_uri() {
        let target = ProxyAddr::new("2001:db8::7", 5070);
        let s = ForwardAllStrategy::new(target.clone());
        let msg = invite();
        let params = s.encode_stickiness(&target, &msg).unwrap();
        let cookie = params.get("target").unwrap();
        assert_eq!(cookie, "[2001:db8::7]:5070");

        let text = format!("sip:192.0.2.250:5060;target={cookie};lr");
        let route = Uri::parse(&SipStr::owned(&text)).unwrap_or_else(|e| panic!("{}", e.reason));
        let read = route.param("target").and_then(ParamValue::as_str).unwrap();
        let mut carried = RouteParams::new();
        carried.insert("target".into(), read.to_string());
        match s.decode_stickiness(&carried, &msg) {
            DecodeResult::Forward { target: decoded, .. } => assert_eq!(decoded, target),
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn missing_or_bad_cookie_is_unknown() {
        let s = ForwardAllStrategy::new(ProxyAddr::new("10.0.0.3", 5070));
        let msg = invite();
        assert!(matches!(
            s.decode_stickiness(&RouteParams::new(), &msg),
            DecodeResult::Unknown { .. }
        ));
        let mut bad = RouteParams::new();
        bad.insert("target".into(), "garbage".into());
        assert!(matches!(s.decode_stickiness(&bad, &msg), DecodeResult::Unknown { .. }));
    }
}
