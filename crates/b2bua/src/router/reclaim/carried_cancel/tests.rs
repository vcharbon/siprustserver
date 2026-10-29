//! A takeover copy that plays on from its primary's copy sends the CANCEL
//! that copy records on an INVITE the copy dialled: the primary decided it,
//! and the copy alone holds the INVITE's transaction (RFC 3261 §9.1).

use std::time::Duration;

use call::{Call, MsgpackCodec};
use sip_message::{SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_txn::TxnSeed;

use super::super::{meet_refused_flush, FlushDirection};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, node_on, src};
use crate::store::{envelope, CallTopology, Enveloped, MaterialiseOrigin};

/// The callee the copy dialled.
const CALLEE: &str = "127.0.0.3:5070";

/// The INVITE the copy sent the callee, as it left.
fn dialled_invite(cid: &str) -> String {
    format!(
        "INVITE sip:callee@{CALLEE} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.2:5080;branch=z9hG4bK-dialled\r\n\
         Max-Forwards: 70\r\nFrom: <sip:b2bua@127.0.0.2>;tag=dialled-tag\r\n\
         To: <sip:callee@{CALLEE}>\r\nCall-ID: {cid}-c1@x\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:b2bua@127.0.0.2:5080>\r\nContent-Length: 0\r\n\r\n"
    )
}

/// A call `w0` owns and `w1` backs up, with a callee leg `c-1` worker `by`
/// dialled, ringing, at `(p, b)`.
fn ringing_callee(cid: &str, by: &call::RelayOwner, p: i64, b: i64) -> Enveloped {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call: Call = build_initial_call(
        &invite("w0", "w1", cid),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let mut leg = call::testkit::dialled_leg("c-1", "dialled-tag");
    let (host, port) = CALLEE.split_once(':').expect("an address");
    call::testkit::dial_with(
        &mut leg,
        call::SentRequest {
            branch: "z9hG4bK-dialled".into(),
            image: dialled_invite(cid).into_bytes(),
            destination: call::HostPort { host: host.into(), port: port.parse().unwrap() },
        },
    );
    if let Some(record) = leg.invite.as_mut() {
        record.relayed_by = by.clone();
    }
    leg.dialogs.insert(call::Dialog::new("dialled-tag", "callee-tag", call::DialogState::Early, 1));
    call.b_legs.push(leg);
    Enveloped::new(call, Some(CallTopology::new("w0", "w1", p, b)))
}

/// The request lines `peer` receives within `within`.
async fn heard(peer: &dyn UdpEndpoint, within: Duration) -> Vec<String> {
    let mut lines = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    while let Ok(Some(p)) = tokio::time::timeout_at(deadline, peer.recv()).await {
        let text = String::from_utf8_lossy(&p.raw).to_string();
        lines.push(text.lines().next().unwrap_or_default().to_string());
    }
    lines
}

#[tokio::test(start_paused = true)]
async fn a_copy_playing_on_sends_the_cancel_its_primary_recorded_on_its_invite() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w1", |_| {}, Vec::new()).await;
    let callee = net.bind_udp(BindUdpOpts::new(CALLEE.parse().unwrap(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    let by = ctx.relay_owner.clone();
    let copy = ringing_callee("carried", &by, 4, 2);
    let call_ref = copy.call.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(copy.clone(), MaterialiseOrigin::Takeover));
    ctx.state.mark_takeover(&call_ref);
    let SipMessage::Request(sent) = sip_message::parser::custom::CustomParser::new()
        .parse(dialled_invite("carried").as_bytes())
        .unwrap()
    else {
        panic!("the dialled INVITE parses")
    };
    let seed = TxnSeed::ClientInvite {
        invite: sent,
        dest: CALLEE.parse().unwrap(),
        provisional: true,
        cancel: None,
        ack_ahead: None,
    };
    assert_eq!(ctx.txn.seed(&call_ref, vec![seed]).await.unwrap(), 1, "the copy holds the INVITE");

    let mut primary = ringing_callee("carried", &by, 5, 2);
    call::helpers::record_cancel_sent(&mut primary.call, "c-1", Vec::new());
    let body =
        envelope::encode(&MsgpackCodec::new(), primary.topology.as_ref(), &primary.call).unwrap();
    meet_refused_flush(ctx, FlushDirection::Forward, &call_ref, &body, 0).await;
    sip_clock::testkit::settle().await;

    assert!(ctx.state.is_takeover(&call_ref), "the copy plays on: the INVITE's final comes to it");
    let lines = heard(callee.as_ref(), Duration::from_millis(200)).await;
    assert!(
        lines.iter().any(|l| l.starts_with("CANCEL ")),
        "the copy sends the primary's CANCEL: {lines:?}"
    );
}
