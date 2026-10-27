//! One case of the unheld-response lane: a datagram read as a response to a
//! request of ours for a call this node does not hold, answered as the shell
//! answers it (ADR-0055 §4, §12). A final to an INVITE whose via-extension
//! verifies is owed its answer whatever else it carries: a 2xx with a
//! Contact its ACK and BYE, a non-2xx naming a readable Request-URI its ACK.

use b2bua_core::config::B2buaConfig;
use b2bua_core::effects::OutboundSipEffect;
use b2bua_core::stack_identity::read_via_extension;
use b2bua_core::unheld_response::answer_unheld_response;
use sip_ids::{SigningKey, TurnIds};
use sip_message::header::Uri;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser, SipStr};

/// The cluster key every node of the lane signs with.
pub fn key() -> SigningKey {
    SigningKey::new(&[3u8; 32]).expect("a key")
}

/// The worker's configuration: its address and the cluster key.
pub fn config() -> B2buaConfig {
    B2buaConfig {
        sip_local_ip: "10.0.0.9".into(),
        sip_local_port: 5060,
        cluster_key: Some(key()),
        ..B2buaConfig::default()
    }
}

/// What the stack sends for `raw`; panics when a verified final is not
/// answered as it is owed.
pub fn case(raw: &[u8]) -> Vec<OutboundSipEffect> {
    let Ok(SipMessage::Response(resp)) = CustomParser::new().parse(raw) else {
        return Vec::new();
    };
    let config = config();
    let src = ("10.0.0.5".to_string(), 5080);
    let sent = answer_unheld_response(&config, &resp, src, &mut TurnIds::fixed(1), 0);
    let owed = (resp.cseq().method() == Method::Invite && resp.status() >= 200)
        .then(|| read_via_extension(resp.top_via(), "INVITE", Some(&key())))
        .flatten();
    if let Some(ext) = owed {
        if resp.status() < 300 {
            let contacted = !resp.contacts().as_slice().is_empty();
            assert!(!contacted || sent.len() == 2, "a verified 2xx is ACKed and BYEd: {sent:?}");
        } else if Uri::parse(&SipStr::owned(&ext.request_uri)).is_ok() {
            assert_eq!(sent.len(), 1, "a verified non-2xx is ACKed: {sent:?}");
        }
    }
    sent
}
