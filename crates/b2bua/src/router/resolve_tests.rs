//! An in-dialog request whose Request-URI states no `callRef` or `leg` of
//! ours (an element on the path rewrote it) finds its call through the SIP
//! index, and its source leg is the leg that owns the matched key: the
//! incoming leg for the caller's request, the outgoing leg for the callee's.
//! The node serving the call reads its live index; an acting backup that
//! holds only the replica reads the replicated one and binds the leg once the
//! takeover copy is in hand.

use call::helpers::{make_empty_dialog, MakeDialogLegCtx};
use call::{call_index_keys, Call, CallBodyCodec, Direction, KeyKind, MsgpackCodec};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::materialise::{materialise, Materialised, Origin};
use super::resolve::{replica_takeover, resolve};
use super::test_support::{invite, node, src};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::store::{CallStore, PartitionRole, PutOpts};
use b2bua_sdk::event::CallEvent;

const B_CALL_ID: &str = "b-cid@10.0.0.2";
const B_LOCAL_TAG: &str = "b2bua-b-tag";
const BOB_TAG: &str = "bob-tag";

/// A call `pri` owns and `bak` backs up, its outgoing leg `b-1` confirmed
/// with bob.
fn answered_call(pri: &str, bak: &str, cid: &str) -> Call {
    let config = B2buaConfig { self_ordinal: pri.into(), ..Default::default() };
    let call =
        build_initial_call(&invite(pri, bak, cid), src(), &config, &sip_txn::IdGen::seeded(1), 0);
    let mut b = call.a_leg.clone();
    b.leg_id = "b-1".into();
    b.call_id = B_CALL_ID.into();
    b.from_tag = B_LOCAL_TAG.into();
    b.dialogs = vec![make_empty_dialog(
        &MakeDialogLegCtx {
            call_id: B_CALL_ID,
            local_uri: "sip:alice@example.com",
            remote_uri: "sip:bob@example.com",
            local_tag: B_LOCAL_TAG,
            remote_tag: BOB_TAG,
        },
        1,
    )];
    call::helpers::add_b_leg(call, b)
}

/// A BYE on `(call_id, from_tag → to_tag)` aimed at the node's bare address.
fn bye(call_id: &str, from_tag: &str, to_tag: &str) -> CallEvent {
    bye_to("sip:127.0.0.2:5080", call_id, from_tag, to_tag)
}

/// [`bye`] with the Request-URI `request_uri`.
fn bye_to(request_uri: &str, call_id: &str, from_tag: &str, to_tag: &str) -> CallEvent {
    let raw = format!(
        "BYE {request_uri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-bye-{from_tag}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:peer@example.com>;tag={from_tag}\r\n\
         To: <sip:b2bua@example.com>;tag={to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 2 BYE\r\n\
         Content-Length: 0\r\n\r\n"
    );
    let message = CustomParser::new().parse(raw.as_bytes()).unwrap();
    assert!(matches!(message, SipMessage::Request(_)));
    CallEvent::Sip { message: Box::new(message), src: src(), matched_client_txn: false }
}

fn callee_bye() -> CallEvent {
    bye(B_CALL_ID, BOB_TAG, B_LOCAL_TAG)
}

fn caller_bye(cid: &str) -> CallEvent {
    bye(&format!("{cid}@10.0.0.9"), "alicetag", "b2bua-a-tag")
}

#[tokio::test(start_paused = true)]
async fn the_serving_node_binds_the_leg_owning_the_matched_key() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let call = answered_call("w0", "w1", "cid-live");
    ctx.state.create(call.clone());

    let event = callee_bye();
    let mut res = resolve(ctx, &event);
    assert_eq!(res.call_ref.as_deref(), Some(call.call_ref.as_str()));
    assert_eq!(res.indexed, Some(KeyKind::BDialog));
    res.bind_indexed_leg(&call, &event);
    assert_eq!(res.source_leg_id, "b-1", "the callee's BYE came on the outgoing leg");
    assert_eq!(res.direction, Direction::FromB);

    let event = caller_bye("cid-live");
    let mut res = resolve(ctx, &event);
    assert_eq!(res.indexed, Some(KeyKind::ALeg));
    res.bind_indexed_leg(&call, &event);
    assert_eq!(res.source_leg_id, "a", "the caller's BYE came on the incoming leg");
    assert_eq!(res.direction, Direction::FromA);
}

#[tokio::test(start_paused = true)]
async fn an_acting_backup_binds_the_leg_from_the_replicated_index() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = answered_call("w0", "w1", "cid-takeover");
    let r = call.call_ref.clone();
    let body = MsgpackCodec::new().encode(&call);
    n.store
        .put_call(
            PartitionRole::Backup,
            "w0",
            &r,
            body,
            &call_index_keys(&call),
            60_000,
            1,
            0,
            &PutOpts::default(),
        )
        .await
        .unwrap();

    let event = callee_bye();
    let mut res = resolve(ctx, &event);
    assert_eq!(res.call_ref, None, "nothing live here");
    let hit = replica_takeover(ctx, &event).await.unwrap().expect("the replica is indexed");
    res.found_by_index(hit, &event);
    assert_eq!(res.call_ref.as_deref(), Some(r.as_str()));

    let _guard = ctx.state.lock(&r).await;
    let Materialised::Served(copy) = materialise(ctx, &r, Origin::Takeover).await else {
        panic!("the replica is taken over");
    };
    res.bind_indexed_leg(&copy, &event);
    assert_eq!(res.source_leg_id, "b-1", "the callee's BYE came on the outgoing leg");
    assert_eq!(res.direction, Direction::FromB);
}

/// A Request-URI that states our `leg` but no `callRef` is found through the
/// index, and the leg it states is the one the request is taken on.
#[tokio::test(start_paused = true)]
async fn a_stated_leg_wins_over_the_index() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let call = answered_call("w0", "w1", "cid-stated");
    ctx.state.create(call.clone());

    let event = bye_to("sip:127.0.0.2:5080;leg=a", B_CALL_ID, BOB_TAG, B_LOCAL_TAG);
    let mut res = resolve(ctx, &event);
    assert_eq!(res.call_ref.as_deref(), Some(call.call_ref.as_str()), "found by the index");
    assert_eq!(res.indexed, None, "the request stated its leg");
    res.bind_indexed_leg(&call, &event);
    assert_eq!(res.source_leg_id, "a");
    assert_eq!(res.direction, Direction::FromA);
}
