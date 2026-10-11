//! A backend breaker gating the OPTIONS self-health: while it is open the
//! worker answers an out-of-dialog OPTIONS `503` not-ready, and `200` again
//! once it closes; the state `/ready` reads stays `Ready` throughout, and a
//! new call is still served.

use b2bua::repl::ReadinessState;
use b2bua_harness::B2buaScene;
use sip_message::generators::OutOfDialogMethod;
use sip_message::header::HeaderName;
use tokio::sync::watch;

/// The SUT's answer to alice's out-of-dialog OPTIONS: status and `Reason`.
async fn options(s: &B2buaScene, expected: u16) -> (u16, Option<String>) {
    let mut options =
        s.alice.request(OutOfDialogMethod::Options, &s.bob).through(s.b2bua.addr).send().await;
    let resp = options.expect(expected).await;
    let reason = resp.raw(HeaderName::Reason).next().map(str::to_string);
    (resp.status(), reason)
}

#[tokio::test(start_paused = true)]
async fn an_open_backend_breaker_turns_options_not_ready_until_it_closes() {
    let s = B2buaScene::new("options-backend-gate").await;
    let (breaker, open) = watch::channel(false);
    s.b2bua.readiness().gate_options_on(open);
    assert_eq!(options(&s, 200).await, (200, None));

    breaker.send_replace(true);
    assert_eq!(
        options(&s, 503).await,
        (503, Some("SIP;cause=503;text=\"not-ready\"".to_string())),
        "open: not-ready"
    );
    assert_eq!(s.b2bua.readiness().state(), ReadinessState::Ready, "/ready is untouched");
    let mut dialog = s.establish().await;
    s.hangup(&mut dialog).await;

    breaker.send_replace(false);
    assert_eq!(options(&s, 200).await, (200, None), "closed: ready again, no latch");
    let _report = s.finish().await;
}
