//! The record-size cap at its bound (ADR-0057): a record of exactly the cap
//! lands and is read, one byte more trips, and the cap is off for a write
//! that is not a live call's and for a reaper verdict's turn.

use b2bua_sdk::event::CallEvent;
use call::{Call, MsgpackCodec};

use super::record_cap::hold;
use super::test_support::{invite, node_with, src};
use super::RouterCtx;
use crate::caps::Tripped;
use crate::config::{B2buaConfig, Cap};
use crate::initial_invite::build_initial_call;
use crate::store::CallTopology;

/// A call `w0` serves with `w1` as its backup, resident on `ctx`.
fn resident_call(ctx: &RouterCtx) -> Call {
    let call = build_initial_call(
        &invite("w0", "w1", "record-cap"),
        src(),
        &B2buaConfig { self_ordinal: "w0".into(), ..Default::default() },
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    ctx.state.remove(&call.call_ref);
    ctx.state.create(call.clone(), Some(CallTopology::new("w0", "w1", 1, 0)));
    call
}

/// The length of `call`'s encoded body.
fn body_bytes(call: &Call) -> u64 {
    MsgpackCodec::new().encoded_len(call).expect("the call encodes")
}

/// A reaper verdict on `call_ref`.
fn verdict(call_ref: &str) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call_ref.into(),
        topic: crate::reaper_verdict::REAPER_TOPIC.into(),
        outcome: "fatal-error".into(),
        payload: serde_json::Value::Null,
        body: Vec::new(),
        incarnation: None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_record_of_its_cap_lands_and_one_byte_more_trips() {
    let probe = node_with("w0", |_| {}).await;
    let bytes = body_bytes(&resident_call(probe.core.router_ctx()));

    let at = node_with("w0", |c| c.caps.record_bytes = bytes).await;
    let ctx = at.core.router_ctx();
    let call = resident_call(ctx);
    let measured = hold(ctx, &call, true, None).expect("a record of its cap lands");
    assert_eq!(measured.map(|m| m.bytes()), Some(bytes), "the write's body is the one measured");
    assert_eq!(at.metrics.cap_peak_of(Cap::RecordBytes), bytes);
    assert_eq!(at.metrics.cap_near_of(Cap::RecordBytes), 1, "past half its cap");

    let under = node_with("w0", |c| c.caps.record_bytes = bytes - 1).await;
    let ctx = under.core.router_ctx();
    let call = resident_call(ctx);
    let tripped = hold(ctx, &call, true, None).unwrap_err();
    assert_eq!(tripped, Tripped { cap: Cap::RecordBytes, verdict: false });
    assert_eq!(under.metrics.cap_peak_of(Cap::RecordBytes), 0, "a tripped record reads nothing");
}

#[tokio::test(start_paused = true)]
async fn the_cap_is_off_for_a_write_of_no_live_call_and_for_a_verdict_s_turn() {
    let node = node_with("w0", |c| c.caps.record_bytes = 1).await;
    let ctx = node.core.router_ctx();
    let call = resident_call(ctx);
    assert!(matches!(hold(ctx, &call, false, None), Ok(None)), "no live call's write");
    let verdict = verdict(&call.call_ref);
    assert!(matches!(hold(ctx, &call, true, Some(&verdict)), Ok(None)), "a verdict's turn");
    assert_eq!(node.metrics.cap_peak_of(Cap::RecordBytes), 0);
}
