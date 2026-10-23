//! **RFC 3262 §5 — the PRACK of a reliable provisional that carried an offer
//! carries the answer.** Where this stack PRACKs a provisional it showed to
//! no one and no party gives an answer, its own PRACK answers the offer
//! rejecting every stream (RFC 3264 §6) — never a bodiless PRACK that leaves
//! the offer unanswered.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

/// The callee's offer, in its reliable 183 to an offerless INVITE.
const BOB_OFFER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\nm=video 20002 RTP/AVP 96\r\n";

/// A decision that DECLARES `Supported: 100rel` toward the originated face, so
/// the callee answers reliably although the caller offered nothing.
fn decision_offering_100rel_toward_bob(port: u16) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", port);
                r.features.advertise_capabilities =
                    Some(call::features::AdvertiseCapabilitiesFeature {
                        toward_originator: None,
                        toward_originated: Some(call::features::AdvertisedCapabilities {
                            allow: None,
                            supported: Some(vec!["100rel".to_string()]),
                        }),
                    });
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// The caller sends an offerless INVITE offering no `100rel`; the declaration
/// offers it to bob, whose reliable 183 carries the offer. The caller is shown
/// an ordinary 183 and PRACKs nothing; this stack's own PRACK answers bob's
/// offer with every stream rejected.
#[tokio::test(start_paused = true)]
async fn the_stacks_own_prack_answers_an_unshown_offer_rejecting_every_stream() {
    let h = Harness::with_transit_delay("b2bua-prack-rejects-unanswered-offer", 0);
    let alice = h.agent("alice", "127.0.0.1:7570").await;
    let bob = h.agent("bob", "127.0.0.1:7571").await;
    let b2bua = B2buaSut::builder(decision_offering_100rel_toward_bob(7571))
        .start(&h, "b2bua", "127.0.0.1:7572")
        .await;

    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(4711).with_sdp(BOB_OFFER).await;
    call.expect(183).await;
    let mut prack = bob.receive("PRACK").await;
    let answer = String::from_utf8_lossy(prack.request().body()).to_string();
    let media: Vec<&str> = answer.split("\r\n").filter(|l| l.starts_with("m=")).collect();
    assert_eq!(media.len(), 2, "the PRACK answers each stream of the offer: {answer:?}");
    assert!(
        media.iter().all(|m| m.split(' ').nth(1) == Some("0")),
        "every stream rejected, port 0: {media:?}"
    );
    prack.respond(200, "OK").await;

    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    call.expect(486).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
