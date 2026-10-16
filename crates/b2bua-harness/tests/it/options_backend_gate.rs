//! A backend breaker gating the acceptance of new calls: while it is open the
//! worker answers an out-of-dialog OPTIONS `200` refusing new calls
//! (`cause=backend`), and accepting again once it closes; the state `/ready`
//! reads stays `Ready` throughout, and an established call is still served.

use b2bua::repl::ReadinessState;
use b2bua_harness::B2buaScene;
use sip_message::generators::OutOfDialogMethod;
use sip_message::worker_report::{self, Readiness};
use tokio::sync::watch;

/// The SUT's answer to alice's out-of-dialog OPTIONS: status and acceptance.
async fn options(s: &B2buaScene) -> (u16, Option<Readiness>) {
    let mut options =
        s.alice.request(OutOfDialogMethod::Options, &s.bob).through(s.b2bua.addr).send().await;
    let resp = options.expect(200).await;
    (resp.status(), worker_report::readiness(&resp))
}

#[tokio::test(start_paused = true)]
async fn an_open_backend_breaker_refuses_new_calls_until_it_closes() {
    let s = B2buaScene::new("options-backend-gate").await;
    let (breaker, open) = watch::channel(false);
    s.b2bua.readiness().gate_options_on(open);
    let mut dialog = s.establish().await;
    assert_eq!(options(&s).await, (200, Some(Readiness::accept())));

    breaker.send_replace(true);
    assert_eq!(options(&s).await, (200, Some(Readiness::refuse("backend"))), "open: refusing");
    assert_eq!(s.b2bua.readiness().state(), ReadinessState::Ready, "/ready is untouched");
    s.hangup(&mut dialog).await;

    breaker.send_replace(false);
    assert_eq!(options(&s).await, (200, Some(Readiness::accept())), "closed: accepting, no latch");
    let _report = s.finish().await;
}
