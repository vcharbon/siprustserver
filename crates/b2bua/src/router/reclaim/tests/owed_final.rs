//! The final an ended body may still owe its caller, met by the reverse-flush
//! reconcile with no live copy here (ADR-0055 §3): the acting backup wrote
//! the ended body before its final left, and may have died before it did.

use std::time::Duration;

use call::{CallModelState, TerminationCause};
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::SimulatedSignalingNetwork;

use super::{body, reconcile_reverse_flush, unanswered};
use crate::router::test_support::{invite, node_on, src};
use crate::store::{CallStore, PartitionRole, PutOpts};

/// **A rejection reverse-flushed to a primary with no live copy reaches the
/// caller.** The acting backup ended the call with the callee's 486 and wrote
/// the ended body back to this node before the final left; the backup may die
/// before it does. This node discharges the body and sends her the 486 on a
/// Completed transaction (RFC 3261 §17.2.1); a copy that already reached her
/// draws only her re-ACK.
#[tokio::test(start_paused = true)]
async fn a_rejection_reverse_flushed_with_no_live_copy_reaches_the_caller() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w0", |_| {}, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    let mut ended = unanswered(1, 2, CallModelState::Terminated);
    let call = &mut ended.call;
    let busy = generate_response(
        &invite("w0", "w1", "meet"),
        486,
        "Busy Here",
        &GenerateResponseOpts::default(),
    )
    .expect("the caller's 486");
    *call.a_leg.invite_final_sent_mut() = Some(486);
    *call.a_leg.invite_rejection_mut() = Some(call::RetainedEmission::on_trigger(
        busy.image().to_vec(),
        (src().ip().to_string(), src().port()),
        call::Repeated::response("INVITE", 486),
    ));
    call::helpers::record_termination(call, 0, TerminationCause::RemoteFinal, None);
    let call_ref = call.call_ref.clone();
    let opts = PutOpts::default();
    let pri = PartitionRole::Primary;
    n.store.put_call(pri, "w0", &call_ref, body(&ended), &[], 60_000, 1, 2, &opts).await.unwrap();

    reconcile_reverse_flush(ctx, &call_ref).await;

    let heard = tokio::time::timeout(Duration::from_secs(1), caller.recv()).await;
    let packet = heard.ok().flatten().expect("the caller hears her final");
    let parsed = CustomParser::new().parse(&packet.raw);
    let Ok(SipMessage::Response(final_)) = parsed else {
        panic!("a response: {}", String::from_utf8_lossy(&packet.raw))
    };
    assert_eq!(final_.status(), 486);
    assert_eq!(n.cdr.snapshot().len(), 1, "the call's one CDR");
    assert!(ctx.state.peek(&call_ref).is_none(), "no copy is left");
}
