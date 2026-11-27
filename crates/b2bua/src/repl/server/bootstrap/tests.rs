//! The batches a bootstrap scan sends: at most [`BOOTSTRAP_CHUNK`] bodies and
//! [`BOOTSTRAP_BATCH_BYTES`] each, a body past the byte bound alone, every
//! live body once, and no batch closed while the next body would still fit.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use repl_net::frame::{Frame, Partition};
use repl_net::transport::{ReplicationConnection, SendError};
use sip_clock::Clock;

use super::{BOOTSTRAP_BATCH_BYTES, BOOTSTRAP_CHUNK};
use crate::repl::test_support::{cref, fwd};
use crate::repl::{Changelog, ReplServer, ReplicatingCallStore};
use crate::store::{CallStore, PartitionRole};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// The call refs and body lengths of one `send_batch`.
type SentBatch = Vec<(String, usize)>;

/// A connection that records each batch it is handed and sends nothing.
#[derive(Default)]
struct BatchLog(Mutex<Vec<SentBatch>>);

impl BatchLog {
    fn batches(&self) -> Vec<SentBatch> {
        self.0.lock().unwrap().clone()
    }
}

fn entry(frame: &Frame) -> (String, usize) {
    match frame {
        Frame::Data { call_ref, body, .. } => {
            (call_ref.clone(), body.as_ref().map_or(0, |body| body.len()))
        }
        other => panic!("a bootstrap sends Data alone: {other:?}"),
    }
}

#[async_trait]
impl ReplicationConnection for BatchLog {
    async fn send(&self, frame: Frame) -> Result<(), SendError> {
        self.0.lock().unwrap().push(vec![entry(&frame)]);
        Ok(())
    }
    async fn send_batch(&self, frames: Vec<Frame>) -> Result<(), SendError> {
        self.0.lock().unwrap().push(frames.iter().map(entry).collect());
        Ok(())
    }
    async fn recv(&self) -> Option<Frame> {
        std::future::pending().await
    }
    fn peer_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 2))
    }
    fn local_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 1))
    }
}

/// The batches A's Backup-flow bootstrap to B sends, A holding one call per
/// body of `bodies`.
async fn scan_of(bodies: &[usize]) -> Vec<SentBatch> {
    let clock = Clock::test_at(0);
    let changelog = Changelog::new(1, clock.clone());
    let store = ReplicatingCallStore::with_changelog(changelog.clone(), clock);
    for (n, len) in bodies.iter().enumerate() {
        let call_ref = cref("A", &format!("c{n}"));
        let opts = fwd("B");
        let primary = PartitionRole::Primary;
        store.put_call(primary, "A", &call_ref, vec![0; *len], &[], 0, 1, 0, &opts).await.unwrap();
    }
    let server = ReplServer::new("A", changelog, Arc::new(store));
    let conn = BatchLog::default();
    let (role, primary) = (PartitionRole::Primary, "A");
    server.serve_bootstrap(&conn, "B", Partition::Bak, role, primary, "backup").await.unwrap();
    conn.batches()
}

fn bytes(batch: &SentBatch) -> usize {
    batch.iter().map(|(_, len)| len).sum()
}

/// Each batch within both bounds or a lone body, every call sent once, and
/// each batch closed only when the next body would pass a bound.
fn assert_batched(batches: &[SentBatch], calls: usize) {
    for batch in batches {
        assert!(
            batch.len() == 1
                || (batch.len() <= BOOTSTRAP_CHUNK && bytes(batch) <= BOOTSTRAP_BATCH_BYTES),
            "a batch of {} bodies and {} bytes passes a bound",
            batch.len(),
            bytes(batch),
        );
    }
    let mut sent: Vec<String> =
        batches.iter().flatten().map(|(call_ref, _)| call_ref.clone()).collect();
    sent.sort();
    let mut all: Vec<String> = (0..calls).map(|n| cref("A", &format!("c{n}"))).collect();
    all.sort();
    assert_eq!(sent, all, "every live body is sent once");
    for pair in batches.windows(2) {
        let (closed, next) = (&pair[0], &pair[1]);
        let next_body = next[0].1;
        assert!(
            closed.len() == BOOTSTRAP_CHUNK || bytes(closed) + next_body > BOOTSTRAP_BATCH_BYTES,
            "a batch of {} bodies and {} bytes closed before a {next_body}-byte body that fits",
            closed.len(),
            bytes(closed),
        );
    }
}

#[tokio::test]
async fn small_bodies_go_by_the_count_bound() {
    let batches = scan_of(&[KIB; 300]).await;
    let sizes: Vec<usize> = batches.iter().map(Vec::len).collect();
    assert_eq!(sizes, vec![128, 128, 44]);
    assert_batched(&batches, 300);
}

#[tokio::test]
async fn large_bodies_go_by_the_byte_bound_and_one_past_it_alone() {
    let mut bodies = vec![1536 * KIB; 8];
    bodies.extend([6 * MIB, 6 * MIB, KIB, KIB]);
    let batches = scan_of(&bodies).await;
    assert_batched(&batches, bodies.len());
    for batch in &batches {
        if batch.iter().any(|(_, len)| *len > BOOTSTRAP_BATCH_BYTES) {
            assert_eq!(batch.len(), 1, "a body past the byte bound goes alone: {batch:?}");
        }
    }
    assert!(batches.len() >= 6, "eight 1.5 MiB bodies take four batches or more: {batches:?}");
}
