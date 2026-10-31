//! A leg's dialling INVITE the other owner sent, met here: the meeting that
//! takes it seeds and retires this node's client transaction of it, and
//! writes the call once so that owner's copy stands down (ADR-0047 D7).

use super::*;

/// `copy` with a callee leg `c-1` worker `by` dialled, its INVITE's final
/// `final_received`.
fn dialled_by(mut copy: Enveloped, by: call::RelayOwner, final_received: Option<u16>) -> Enveloped {
    let mut leg = call::testkit::dialled_leg("c-1", "dialled-tag");
    if let Some(record) = leg.invite.as_mut() {
        record.relayed_by = by;
    }
    if let Some(status) = final_received {
        leg.outbound_invite_mut().expect("the dialled INVITE").record_final(status);
    }
    copy.call.b_legs.push(leg);
    copy
}

/// **The primary writes once when the copy's own INVITE takes its final.**
/// The backup's copy played on while it awaited the final of an INVITE it
/// dialled; its write that carries the final is taken whole here, and the
/// primary writes the call once, the covering write the copy stands down on
/// (ADR-0047 D7). A taken copy whose dialled INVITE was final already is no
/// write of this node's.
#[tokio::test(start_paused = true)]
async fn a_taken_final_of_the_copys_own_invite_draws_one_covering_write() {
    let n = node("w0").await;
    let copy_worker = call::RelayOwner::new("w1", "w1-boot-1");
    let live = dialled_by(answered(4, 1, CallModelState::Active), copy_worker.clone(), None);
    let copy = dialled_by(answered(4, 2, CallModelState::Active), copy_worker.clone(), Some(200));
    meet(&n, &live, &copy, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;
    assert_eq!(live_versions(&n, &live.call.call_ref).0, (5, 2), "one write of the primary's");

    let n = node("w0").await;
    let live = dialled_by(answered(4, 1, CallModelState::Active), copy_worker.clone(), Some(200));
    let copy = dialled_by(answered(4, 2, CallModelState::Active), copy_worker, Some(200));
    meet(&n, &live, &copy, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;
    assert_eq!(live_versions(&n, &live.call.call_ref).0, (4, 2), "no write: nothing changed hands");
}

/// **The primary writes once when it takes a leg its copy dialled.** The
/// backup's copy dialled a leg the primary's own copy lacks, its INVITE
/// still ringing: the INVITE's responses reach the primary, which takes it
/// over and writes the call once so the copy stands down (ADR-0047 D7).
#[tokio::test(start_paused = true)]
async fn a_taken_dial_of_the_copys_own_draws_one_covering_write() {
    let n = node("w0").await;
    let copy_worker = call::RelayOwner::new("w1", "w1-boot-1");
    let live = answered(4, 1, CallModelState::Active);
    let copy = dialled_by(answered(4, 2, CallModelState::Active), copy_worker, None);
    meet(&n, &live, &copy, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;
    assert_eq!(live_versions(&n, &live.call.call_ref).0, (5, 2), "one write of the primary's");
}

/// `copy` with a callee leg `c-1` the copy's worker dialled on
/// `z9hG4bK-met-dial`, as it left, its INVITE's final `final_received`.
fn dialled_with_its_image(copy: Enveloped, final_received: Option<u16>) -> Enveloped {
    let mut copy = dialled_by(copy, call::RelayOwner::new("w1", "w1-boot-1"), final_received);
    let image = "INVITE sip:callee@127.0.0.3:5070 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.2:5080;branch=z9hG4bK-met-dial\r\n\
         Max-Forwards: 70\r\nFrom: <sip:b2bua@127.0.0.2>;tag=dialled-tag\r\n\
         To: <sip:callee@127.0.0.3>\r\nCall-ID: met-dial@x\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:b2bua@127.0.0.2:5080>\r\nContent-Length: 0\r\n\r\n";
    let leg = copy.call.b_legs.last_mut().expect("the dialled leg");
    call::testkit::dial_with(
        leg,
        call::SentRequest {
            branch: "z9hG4bK-met-dial".into(),
            image: image.as_bytes().to_vec(),
            destination: call::HostPort { host: "127.0.0.3".into(), port: 5070 },
        },
    );
    copy
}

/// **An armed primary takes over the INVITE its copy dialled.** It meets the
/// copy's write carrying a leg the primary's own copy lacks, its INVITE with
/// no final: the primary seeds that INVITE's client transaction, whose
/// responses reach it (ADR-0055 §8). It then meets the copy's write carrying
/// that INVITE's final, taken by the copy, and retires the transaction, which
/// no final reaches here any more (ADR-0047 D7).
#[tokio::test(start_paused = true)]
async fn a_met_dial_is_seeded_and_retired_once_the_copy_took_its_final() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let live = answered(4, 1, CallModelState::Active);
    let ringing = dialled_with_its_image(answered(4, 2, CallModelState::Active), None);
    meet(&n, &live, &ringing, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;
    assert!(ctx.txn.holds_client("z9hG4bK-met-dial").await.unwrap(), "the dial is seeded here");

    let held = ctx.state.peek_enveloped(&live.call.call_ref).expect("the call is live");
    let answered_there = dialled_with_its_image(answered(4, 3, CallModelState::Active), Some(200));
    meet_refused_flush(
        ctx,
        FlushDirection::Reverse,
        &held.call.call_ref,
        &body(&answered_there),
        0,
    )
    .await;
    sip_clock::testkit::settle().await;
    assert!(
        !ctx.txn.holds_client("z9hG4bK-met-dial").await.unwrap(),
        "the dial whose final the copy took is retired here"
    );
}
