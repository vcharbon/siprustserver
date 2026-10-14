//! A failing two-node cell's dump names the node behind every line: each fact
//! of the per-call SUT trace carries the incarnation that recorded it
//! (`b1#g1`), and the SUT log lines name their node rather than the process's.
//!
//! The call is served by both nodes while both live: the proxy misroutes a
//! re-INVITE to the backup, whose copy serves the round (ADR-0014), and the
//! healed route takes the BYE back to the primary. The call ends cleanly; the
//! scenario then fails on purpose so the dump is written.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, WorkerHealth};
use scenario_harness::expect_failure;
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{
    accept_takeover_cseq_overlap, establish, Established, ANSWER, OFFER,
};

/// The dump section under the header `── {title} ──`, up to the next one.
fn section<'d>(dump: &'d str, title: &str) -> &'d str {
    let start = dump
        .find(&format!("── {title}"))
        .and_then(|at| dump[at..].find('\n').map(|nl| at + nl + 1))
        .unwrap_or_else(|| panic!("no section {title:?} in the dump:\n{dump}"));
    let rest = &dump[start..];
    &rest[..rest.find("\n── ").map(|i| i + 1).unwrap_or(rest.len())]
}

/// The node a fact line names: the word after its `[T+…]` timestamp.
fn fact_node(line: &str) -> Option<&str> {
    let after = line.trim_start().strip_prefix('[')?;
    let (_at, rest) = after.split_once("] ")?;
    rest.split_whitespace().next()
}

#[tokio::test(start_paused = true)]
async fn a_two_node_dump_names_the_node_of_every_trace_fact_and_log_line() {
    let failed = expect_failure(async {
        let Established {
            mut fh,
            alice,
            bob,
            proxy,
            store,
            lh: _lh,
            w_b1,
            w_b2,
            primary_ord,
            call_ref,
            mut dialog,
            ..
        } = establish("dump-names-the-node").await;
        accept_takeover_cseq_overlap(&mut fh);

        // The backup's copy serves a re-INVITE round while the primary lives.
        proxy.set_health(&primary_ord, WorkerHealth::Dead);
        fh.advance(Duration::from_millis(200)).await;
        let mut round = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
        bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
        round.expect(200).await;
        dialog.ack(None).await;
        bob.receive("ACK").await;
        fh.advance(Duration::from_millis(500)).await;

        // The healed route takes the BYE back to the primary.
        proxy.set_health(&primary_ord, WorkerHealth::Alive);
        fh.advance(Duration::from_millis(200)).await;
        let mut bye = dialog.bye().await;
        bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
        bye.expect_tolerating(200, &["OPTIONS"]).await;
        let drained = fh
            .settle_terminal(async || {
                store.stats().current_total == 0
                    && w_b1.memory_clean()
                    && w_b2.memory_clean()
                    && !w_b1.holds_any_trace(&call_ref).await
                    && !w_b2.holds_any_trace(&call_ref).await
            })
            .await;
        assert!(drained, "the call drains cleanly");
        fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
        assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;

        panic!("the scenario fails on purpose so its dump is written");
    })
    .await;
    assert!(failed.digest().starts_with("the scenario fails on purpose"), "{}", failed.digest());
    let [dump] = failed.dumps.as_slice() else { panic!("one dump: {:?}", failed.dumps) };
    let nodes = ["b1#g1", "b2#g1"];

    let trace = section(dump, "SUT trace per call");
    let facts: Vec<&str> = trace.lines().filter(|l| l.starts_with("    [")).collect();
    assert!(!facts.is_empty(), "the call was traced: {trace}");
    for fact in &facts {
        let node = fact_node(fact);
        assert!(
            node.is_some_and(|n| nodes.contains(&n)),
            "every fact names the node that recorded it, got {node:?}: {fact}"
        );
    }
    for node in nodes {
        assert!(
            facts.iter().any(|f| fact_node(f) == Some(node)),
            "both nodes recorded facts of the call, none from {node}: {trace}"
        );
    }

    let log = section(dump, "SUT log");
    assert!(!log.contains("node=unknown"), "every log line names its node: {log}");
    for node in nodes {
        assert!(log.contains(&format!("node={node}")), "{node} names itself in the log: {log}");
    }
}
