//! The core's two loops, the transaction owner and the router: an idle core
//! never reads stalled, a minute idle against a 5 s threshold, and a crashed
//! core's loops read ended within one supervision poll.

use std::time::Duration;

use super::unwired_tests::unwired_core;
use crate::loops::ENDED_POLL;

const THRESHOLD: Duration = Duration::from_secs(5);

#[tokio::test(start_paused = true)]
async fn an_idle_core_never_reads_stalled() {
    let core = unwired_core("w0").await;
    tokio::time::sleep(THRESHOLD * 12).await;
    assert!(core.loops().stalled(THRESHOLD).is_empty());
    assert_eq!(core.loops().ended(), None);
}

#[tokio::test(start_paused = true)]
async fn a_crashed_cores_loops_read_ended_within_one_poll() {
    let mut core = unwired_core("w0").await;
    let loops = core.loops().clone();
    core.abort();
    let ended = tokio::time::timeout(ENDED_POLL * 2, loops.wait_ended()).await;
    assert!(ended.is_ok(), "a loop task ended");
}
