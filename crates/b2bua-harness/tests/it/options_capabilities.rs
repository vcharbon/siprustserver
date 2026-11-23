//! RFC 3261 §11.2: every 200 this stack answers to an OPTIONS states its own
//! capability set — `Allow`, `Supported`, `Accept`. A 200 it relays states
//! that set by default, the back-to-back UA being the UAS the OPTIONS asked;
//! under `RelayedOptionsAdvertisement::AsAnswered` it carries the answerer's
//! lines as they came, none added.

use std::time::Duration;

use b2bua::config::RelayedOptionsAdvertisement;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::{CapabilitySet, InDialogMethod};
use sip_message::header::HeaderName;
use sip_message::{SipResponse, SipStr};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// The `Allow` / `Supported` / `Accept` lines `resp` carries, in order.
fn advertisement(resp: &SipResponse) -> Vec<(HeaderName, String)> {
    [HeaderName::Allow, HeaderName::Supported, HeaderName::Accept]
        .into_iter()
        .flat_map(|name| {
            resp.raw(name.clone()).map(move |v| (name.clone(), v.to_string())).collect::<Vec<_>>()
        })
        .collect()
}

/// bob's advertisement on his 200 to OPTIONS, narrower than the stack's set.
const BOB_STATES: &[(&str, &str)] = &[
    ("Allow", "INVITE, ACK, BYE"),
    ("Supported", "timer"),
    ("Accept", "application/sdp"),
    ("Accept-Encoding", "gzip"),
    ("Accept-Language", "en"),
];

/// The 200 alice receives to an OPTIONS she sends in a confirmed dialog, the
/// B2BUA relaying it under `mode` and bob answering with `stated` (none of
/// his own lines beside them when `bare`). The call ends with a BYE and is
/// reaped; a bare bob is waived on `options-response-echoes`. The B2BUA that
/// relays his bare answer as it came is not charged: the answer is bob's.
async fn relayed_options_200(
    name: &str,
    mode: RelayedOptionsAdvertisement,
    stated: &[(&str, &str)],
    bare: bool,
) -> SipResponse {
    let h = Harness::with_transit_delay(name, 1);
    if bare {
        h.waive(
            WaiverScope::rule("options-response-echoes", "bob answers OPTIONS bare")
                .on_party("bob"),
        );
    }
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .tune(move |c| {
            c.keepalive_interval_sec = 3_600;
            c.relayed_options_advertisement = mode;
        })
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut options = dialog.send_request(InDialogMethod::Options).send().await;
    let mut asked = bob.receive("OPTIONS").await;
    let mut answer = asked.respond(200, "OK");
    for &(name, value) in stated {
        answer = answer.with_header(name, value);
    }
    if bare {
        answer = answer.stating_no_capabilities();
    }
    answer.await;
    let ok = options.expect(200).await;

    dialog.bye().await.expect(200).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
    ok
}

/// alice's OPTIONS in a confirmed dialog is relayed to bob, whose 200 states
/// a narrow set of his own; the 200 alice receives states the stack's set.
#[tokio::test(start_paused = true)]
async fn a_relayed_options_200_states_the_stack_capabilities() {
    let ok = relayed_options_200(
        "options-200-relayed",
        RelayedOptionsAdvertisement::NodeSet,
        BOB_STATES,
        false,
    )
    .await;
    assert_eq!(advertisement(&ok), CapabilitySet::default().lines());
    for name in [HeaderName::AcceptEncoding, HeaderName::AcceptLanguage] {
        assert_eq!(ok.raw(name.clone()).count(), 0, "bob's {name:?} describes bob, not this UA");
    }
}

/// Under `AsAnswered`, the 200 alice receives carries bob's lines, each once
/// and as he stated it.
#[tokio::test(start_paused = true)]
async fn a_relayed_options_200_as_answered_states_the_answerers_lines() {
    let ok = relayed_options_200(
        "options-200-as-answered",
        RelayedOptionsAdvertisement::AsAnswered,
        BOB_STATES,
        false,
    )
    .await;
    for &(name, value) in BOB_STATES {
        let lines: Vec<&str> = ok.raw(HeaderName::of(&SipStr::owned(name))).collect();
        assert_eq!(lines, [value], "bob's {name}, relayed as it came");
    }
}

/// Under `AsAnswered`, bob's bare 200 reaches alice bare.
#[tokio::test(start_paused = true)]
async fn a_bare_relayed_options_200_as_answered_stays_bare() {
    let ok = relayed_options_200(
        "options-200-as-answered-bare",
        RelayedOptionsAdvertisement::AsAnswered,
        &[],
        true,
    )
    .await;
    assert_eq!(advertisement(&ok), Vec::new(), "the B2BUA adds nothing to bob's bare 200");
}

/// bob hangs up; alice's OPTIONS crossing the B2BUA's BYE toward her is
/// answered by the stack in her ending dialog, stating the stack's set.
#[tokio::test(start_paused = true)]
async fn an_options_answered_in_an_ending_dialog_states_the_stack_capabilities() {
    let h = Harness::with_transit_delay("options-200-absorbed", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    uas.dialog().bye().await.expect(200).await;
    let mut ours = alice.receive("BYE").await;

    let ok = dialog.send_request(InDialogMethod::Options).send().await.expect(200).await;
    assert_eq!(advertisement(&ok), CapabilitySet::default().lines());

    h.advance(Duration::from_millis(100)).await;
    ours.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
