//! RFC 5022 §10.2 over the whole stack: a replayed media resource's MSCML
//! response echoes the client-defined id of the request it answers.
//!
//! The caller plays the MSCML client through the demo B2BUA and stamps its
//! `<play>` and `<stop>` with ids; the callee replays a media resource whose
//! recorded responses hold no id (the play's) or the capture's own (the
//! stop's). Each response reaches the client carrying its own request's id,
//! and every other byte of the recorded body.

use crate::run_ladder::{
    api_scene, assert_bundle_is_complete, recorded_lines, recorded_wire, replay,
};

/// The body of each INFO in `datagrams`, in order.
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
    let socket = info_bodies(scene.alice.wire_view().into_iter().map(|entry| entry.raw));
    assert_eq!(
        socket, expected,
        "the stop's response replaces the capture's id with the live one, and the play's \
         takes the play's id, not the stop's"
    );
    let recorded = info_bodies(
        recorded_lines(&dir, "A").iter().filter(|line| line["dir"] == "in").map(recorded_wire),
    );
    assert_eq!(recorded, expected, "the caller's recording holds what its socket took");

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 29, "every step ran");
    scene.finish().await;
}
