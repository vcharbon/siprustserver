//! Captured corpus cases named by `PIVOT_CORPUS_CASES`.

use super::*;

/// The scene a CAPTURED document replays against: the system routes every call
/// to the callee socket and relays the Request-URI userpart the document dialled
/// onto the b-leg, which is what a captured `ruri-pos` claim matches.
async fn capture_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await
}

/// The `index`-th corpus case `PIVOT_CORPUS_CASES` names (case ids separated by
/// whitespace), replayed on its own scene.
///
/// Each case gets its OWN test — and therefore its own paused runtime and its
/// own simulated network. Several scenes inside one test share one clock, and
/// the previous scene's still-armed timers keep its auto-advance alive, so a
/// later scene's run grinds instead of converging.
///
/// The caller names cases with the property that makes a captured document
/// replayable on a FOREIGN system: `lanes["upstream-fake"] == "ok"`, one call,
/// one attempt, an answered linear flow, and every expect `check: "record"`, so
/// nothing asserts a header only the capture's own platform ever emitted.
async fn replay_corpus(index: usize) {
    let named = std::env::var("PIVOT_CORPUS_CASES").unwrap_or_default();
    let Some(case_id) = named.split_whitespace().nth(index) else {
        eprintln!("SKIPPED corpus rung {index}: PIVOT_CORPUS_CASES names no case {index}");
        return;
    };
    let Some(case) = corpus_case(case_id) else {
        eprintln!("SKIPPED {case_id}: set PIVOT_CORPUS_DIRS to the regenerated v3 corpus");
        return;
    };
    let scene = capture_scene(&format!("pivot-corpus-{index}")).await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert!(dir.join("verdict.json").is_file(), "{case_id} left its bundle");
    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::Ok,
        "{case_id}\nfailures: {:#?}\nrecording: {:#?}",
        outcome.verdict.failures,
        outcome.recording.legs()
    );
    assert_eq!(outcome.verdict.completed_steps.len(), 14, "{case_id}: every step ran");
    assert!(outcome.timing.settled_at_ms.is_some(), "{case_id} settled");
    let legs = outcome.recording.legs();
    assert!(!legs["A"].is_empty() && !legs["B"].is_empty(), "{case_id}: {legs:#?}");
    scene.finish().await;
}

#[tokio::test(start_paused = true)]
async fn rung_four_corpus_case_0() {
    replay_corpus(0).await;
}

#[tokio::test(start_paused = true)]
async fn rung_four_corpus_case_1() {
    replay_corpus(1).await;
}

#[tokio::test(start_paused = true)]
async fn rung_four_corpus_case_2() {
    replay_corpus(2).await;
}

#[tokio::test(start_paused = true)]
async fn rung_four_corpus_case_3() {
    replay_corpus(3).await;
}

#[tokio::test(start_paused = true)]
async fn rung_four_corpus_case_4() {
    replay_corpus(4).await;
}
