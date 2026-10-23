//! RFC 5022 §10.2 over the whole stack: a replayed media resource's MSCML
//! response echoes the client-defined id of the request it answers.
//!
//! The caller plays the MSCML client through the demo B2BUA and stamps its
//! requests with ids; the callee replays a media resource whose recorded
//! responses hold no id or the capture's own. Each response reaches the client
//! carrying its own request's id, and every other byte of the recorded body,
//! wherever the request landed: an expect, a background policy, or the
//! initial INVITE that opened the leg.

use crate::run_ladder::{
    api_scene, assert_bundle_is_complete, recorded_lines, recorded_wire, replay,
};

/// The body of each INFO in `datagrams`, in order. A caller hands first
/// sightings only: a retransmission repeats a body, it states no new one.
fn info_bodies(datagrams: impl IntoIterator<Item = Vec<u8>>) -> Vec<String> {
    datagrams
        .into_iter()
        .filter(|wire| wire.starts_with(b"INFO "))
        .map(|wire| {
            let text = String::from_utf8(wire).expect("an MSCML INFO is text");
            let (_, body) = text.split_once("\r\n\r\n").expect("a head and a body");
            body.to_string()
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_replayed_media_resource_echoes_each_request_s_own_id() {
    let scene = api_scene("pivot-mscml-id-echo").await;
    let (outcome, dir) = replay(&scene, "mscml-id-echo.v3.json").await;

    let decl = r#"<?xml version="1.0" encoding="utf-8"?><MediaServerControl version="1.0">"#;
    let expected = [
        format!(r#"{decl}<response id="s-1" request="stop" code="200"/></MediaServerControl>"#),
        format!(
            r#"{decl}<response id="p-1" request="play" code="200" reason="stopped" playduration="1200"/></MediaServerControl>"#
        ),
    ];
    let socket = info_bodies(
        scene.alice.wire_view().into_iter().filter(|entry| !entry.is_repeat()).map(|e| e.raw),
    );
    assert_eq!(
        socket, expected,
        "the stop's response replaces the capture's id with the live one, and the play's \
         takes the play's id, not the stop's"
    );
    let recorded = info_bodies(
        recorded_lines(&dir, "A")
            .iter()
            .filter(|line| line["dir"] == "in" && line["repeat_of"].is_null())
            .map(recorded_wire),
    );
    assert_eq!(recorded, expected, "the caller's recording holds what its socket took");

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 29, "every step ran");
    scene.finish().await;
}

/// The leg a replayed UAS answers is claimed by the initial INVITE itself, so an
/// MSCML request riding beside the offer is remembered there, and the 200's
/// MSCML response echoes it.
#[tokio::test(start_paused = true)]
async fn an_mscml_request_in_the_initial_invite_is_echoed_by_the_answer() {
    let scene = api_scene("pivot-mscml-id-echo-invite").await;
    let (outcome, dir) = replay(&scene, "mscml-id-echo-invite.v3.json").await;

    let answer = recorded_lines(&dir, "B")
        .iter()
        .filter(|line| line["dir"] == "out" && line["repeat_of"].is_null())
        .map(recorded_wire)
        .find(|wire| wire.starts_with(b"SIP/2.0 200 ") && wire.windows(6).any(|w| w == b"INVITE"))
        .expect("the callee answered the INVITE");
    let answer = String::from_utf8(answer).expect("the answer is text");
    assert!(
        answer.contains(r#"<response id="c1" request="configure_leg" code="200" text="OK"/>"#),
        "the answer's MSCML part echoes the INVITE's request id:\n{answer}"
    );

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    scene.finish().await;
}

/// A request a background policy absorbs is still a request the leg received:
/// the callee's later response echoes its id.
#[tokio::test(start_paused = true)]
async fn an_mscml_request_absorbed_by_a_background_policy_is_echoed() {
    let scene = api_scene("pivot-mscml-id-echo-background").await;
    let (outcome, dir) = replay(&scene, "mscml-id-echo-background.v3.json").await;

    let decl = r#"<?xml version="1.0" encoding="utf-8"?><MediaServerControl version="1.0">"#;
    let socket = info_bodies(
        scene.alice.wire_view().into_iter().filter(|entry| !entry.is_repeat()).map(|e| e.raw),
    );
    assert_eq!(
        socket,
        [format!(
            r#"{decl}<response id="p-7" request="play" code="200" reason="stopped" playduration="1200"/></MediaServerControl>"#
        )],
        "the response echoes the absorbed request's id"
    );

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 19, "every step ran");
    scene.finish().await;
}
