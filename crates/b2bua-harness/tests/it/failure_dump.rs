//! A failing B2BUA scene's dump carries what the SUT did: each call's rule
//! trace (the trace-plane facts every harness B2BUA records) and the timer
//! ledger of each call still live at the failure.

use b2bua_harness::B2buaScene;
use scenario_harness::expect_failure;

/// The section of `dump` under the header `── {title} ──`, up to the next.
fn section<'d>(dump: &'d str, title: &str) -> &'d str {
    let header = format!("── {title} ──\n");
    let start = dump.find(&header).map(|i| i + header.len()).unwrap_or_else(|| {
        panic!("no section {title:?} in the dump:\n{dump}");
    });
    let rest = &dump[start..];
    &rest[..rest.find("\n── ").map(|i| i + 1).unwrap_or(rest.len())]
}

/// A scene failing with its call up shows that call's rule trace, in order,
/// one line per fact, and its armed timers with their deadlines and legs.
#[tokio::test(start_paused = true)]
async fn a_failing_scene_shows_each_calls_rule_trace_and_timer_ledger() {
    let failed = expect_failure(async {
        let s = B2buaScene::new("dump-rule-trace").await;
        let _dialog = s.establish().await;
        // The scenario awaits a BYE nobody sends: the call is still up.
        s.bob.receive("BYE").await;
    })
    .await;
    assert!(
        failed.digest().starts_with("bob timed out after 5s waiting for a BYE request"),
        "{}",
        failed.digest()
    );
    let [dump] = failed.dumps.as_slice() else { panic!("one dump: {:?}", failed.dumps) };

    let trace = section(dump, "SUT trace per call");
    let call_header = trace.lines().next().unwrap_or_default();
    assert!(call_header.starts_with("  call Call-ID alice-"), "one block per call: {trace}");
    let invite = trace.find("sip.in").expect("the INVITE as it arrived");
    let fired = trace.find("rule.fired").expect("the rules that handled each event");
    assert!(invite < fired, "the facts are in order: {trace}");
    assert!(trace.contains("INVITE sip:"), "a message fact shows its first line: {trace}");
    assert!(
        trace.lines().skip(1).all(|l| l.starts_with("    [T+")),
        "one compact line per fact: {trace}"
    );

    let ledger = section(dump, "b2bua timer ledger per live call");
    assert!(ledger.contains("  call w0|alice-1@127.0.0.1|"), "one block per live call: {ledger}");
    assert!(ledger.contains("    Keepalive fires in "), "each timer, its deadline: {ledger}");
}
