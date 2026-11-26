//! An in-dialog request whose Request-URI states no `callRef` or `leg` of
//! ours (an element on the path rewrote it) finds its call through the SIP
//! index, and its source leg is the leg that owns the matched key: the
//! incoming leg for the caller's request, the outgoing leg for the callee's.
//! The node serving the call reads its live index; an acting backup that
//! holds only the replica reads the replicated one and binds the leg once the
//! takeover copy is in hand.

use call::{call_index_keys, Call, Dialog, DialogState, Direction, KeyKind, OutboundInvite};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::materialise::{materialise, Materialised, Origin};
use super::resolve::{attributed_on_resolution, replica_takeover, resolve};
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
    let mut call = build_initial_call(
        &invite(pri, bak, cid),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let mut b = call.a_leg.clone();
    b.leg_id = "b-1".into();
    b.call_id = B_CALL_ID.into();
    b.invite = Some(call::RequestRecord::dialled(1, None, OutboundInvite::new(B_LOCAL_TAG, 0)));
    b.dialogs = vec![Dialog::new(B_LOCAL_TAG, BOB_TAG, DialogState::Confirmed, 1)].into();
    call::helpers::add_b_leg(&mut call, b);
    call
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
    CallEvent::sip(message, src())
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
    ctx.state.create_placed(call.clone());

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
    let body = crate::store::envelope::placed_body(&call).unwrap();
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
    let Materialised::Served { call: copy, .. } = materialise(ctx, &r, Origin::Takeover).await
    else {
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
    ctx.state.create_placed(call.clone());

    let event = bye_to("sip:127.0.0.2:5080;leg=a", B_CALL_ID, BOB_TAG, B_LOCAL_TAG);
    let mut res = resolve(ctx, &event);
    assert_eq!(res.call_ref.as_deref(), Some(call.call_ref.as_str()), "found by the index");
    assert_eq!(res.indexed, None, "the request stated its leg");
    res.bind_indexed_leg(&call, &event);
    assert_eq!(res.source_leg_id, "a");
    assert_eq!(res.direction, Direction::FromA);
}

/// `method` on `request_uri`, in a dialog when `to_tag` is given.
fn request(method: &str, request_uri: &str, to_tag: Option<&str>) -> sip_message::SipRequest {
    let to_tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    let raw = format!(
        "{method} {request_uri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-attr\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:peer@example.com>;tag=peer-tag\r\n\
         To: <sip:b2bua@example.com>{to_tag}\r\n\
         Call-ID: attr@10.0.0.9\r\n\
         CSeq: 2 {method}\r\n\
         Contact: <sip:peer@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(req) => req,
        SipMessage::Response(_) => panic!("a request"),
    }
}

/// The router attributes the server transaction of a request it resolves
/// only where the Request-URI states no `callRef` and the request opens a
/// transaction of the call: never an ACK's (none opens), a CANCEL's, or an
/// out-of-dialog INVITE's, which opens or copies a call.
#[test]
fn only_a_dialogs_request_stating_no_call_ref_is_attributed_on_resolution() {
    let bare = "sip:127.0.0.2:5080";
    let stamped = "sip:127.0.0.2:5080;callRef=w0%7Ccid%7Ctag;leg=a";
    assert!(attributed_on_resolution(&request("BYE", bare, Some("t"))));
    assert!(attributed_on_resolution(&request("INVITE", bare, Some("t"))));
    assert!(attributed_on_resolution(&request("INFO", bare, Some("t"))));
    assert!(!attributed_on_resolution(&request("BYE", stamped, Some("t"))), "the layer did");
    assert!(!attributed_on_resolution(&request("ACK", bare, Some("t"))));
    assert!(!attributed_on_resolution(&request("CANCEL", bare, None)));
    assert!(!attributed_on_resolution(&request("INVITE", bare, None)));
}
