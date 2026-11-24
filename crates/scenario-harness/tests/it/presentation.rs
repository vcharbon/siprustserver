//! An INVITE's [`Presentation`]: raw display names reach the wire quoted and
//! escaped and read back whole, the name forms spell the stack's headers on
//! the datagram, and the call completes with an RFC audit that charges
//! nothing.

use scenario_harness::realcall::Presentation;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\n";

#[tokio::test(start_paused = true)]
async fn a_presented_invite_states_its_display_names_and_wire_names() {
    let h = Harness::new("presentation-wire").describe(
        "Display names holding quotes, backslashes, a tab and UTF-8 and compact or \
         odd-cased header names reach the callee as stated",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let from_display = "${jndi:ldap://192.0.2.1/a} \"q\" \\ \t Zoë 東京";
    let to_display = "'; DROP TABLE calls;--";
    let presentation = Presentation {
        from_display: Some(from_display.into()),
        to_display: Some(to_display.into()),
        name_forms: vec![
            ("From".into(), "f".into()),
            ("Call-ID".into(), "cALL-iD".into()),
            ("Content-Length".into(), "l".into()),
        ],
    };

    let mut call = presentation.apply(alice.invite(&bob)).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    let req = uas.request().clone();
    let text = String::from_utf8_lossy(req.image()).into_owned();

    for line in ["\r\nf: \"", "\r\ncALL-iD: ", "\r\nl: "] {
        assert!(text.contains(line), "expected {line:?} in:\n{text}");
    }
    assert!(text.contains(r#"\"q\" \\"#), "quotes and backslash escaped:\n{text}");
    assert_eq!(req.from().display(), Some(from_display));
    assert_eq!(req.to().display(), Some(to_display));
    assert!(req.body().starts_with(b"v=0"), "the offer is framed by the compact length");

    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}
