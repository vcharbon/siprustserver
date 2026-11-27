//! A rated link: 1 MiB/s, a 1 ms transit delay, frames of a 1 MiB body
//! (about 1 s each on the link).
//!
//! ```text
//!   no buffer   frame 1 sent at 0      → lands at ~1.001 s, never before 1 s
//!   no buffer   frame 2 sent at 0      → its send blocks until frame 1 left (~1 s)
//!   1 MiB buf   frame 2 sent at 0      → its send returns at once
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::testkit::advance_settled;

use crate::SimulatedReplicationNetwork;
use crate::{Frame, LinkRate, ReplicationConnection, ReplicationNetwork};

const MIB: u64 = 1024 * 1024;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// A frame of a 1 MiB body.
fn mib_frame(counter: u64) -> Frame {
    Frame::Data {
        at: crate::Watermark::new(1, counter),
        op: crate::Op::Put,
        partition: crate::Partition::Bak,
        call_ref: format!("p|call{counter}|tag"),
        call_gen: 1,
        call_bgen: 0,
        body_ttl_ms: 1000,
        origin_now_ms: 0,
        indexes: Vec::new(),
        body: Some(Arc::from(vec![7u8; MIB as usize])),
        answered: false,
        incarnation: None,
        discharged: false,
    }
}

/// A client and server end over a fabric at 1 MiB/s with a sender's buffer
/// of `buffer_bytes`.
async fn rated_pair(
    buffer_bytes: u64,
) -> (Box<dyn ReplicationConnection>, Box<dyn ReplicationConnection>) {
    let rate = LinkRate { bytes_per_sec: MIB, buffer_bytes };
    let net = SimulatedReplicationNetwork::with_link_rate(1, rate);
    let listener = net.listen(addr(7001)).await.unwrap();
    let client = net.connect_from(addr(7000), addr(7001)).await.unwrap();
    let server = listener.accept().await.unwrap();
    (client, server)
}

/// Whether `conn` has a frame to `recv` right now, without waiting.
async fn has_frame(conn: &dyn ReplicationConnection) -> bool {
    tokio::time::timeout(Duration::from_micros(1), conn.recv()).await.ok().flatten().is_some()
}

#[tokio::test(start_paused = true)]
async fn a_frame_lands_once_its_last_byte_has_left() {
    let (client, server) = rated_pair(0).await;
    client.send(mib_frame(1)).await.unwrap();
    advance_settled(Duration::from_millis(990)).await;
    assert!(!has_frame(server.as_ref()).await, "1 MiB at 1 MiB/s has not left at 990 ms");
    advance_settled(Duration::from_millis(20)).await;
    assert!(has_frame(server.as_ref()).await, "it lands its transit delay after it left");
}

#[tokio::test(start_paused = true)]
async fn a_send_blocks_while_more_than_the_buffer_is_queued_ahead() {
    let (client, _server) = rated_pair(0).await;
    client.send(mib_frame(1)).await.unwrap();
    let client: Arc<dyn ReplicationConnection> = Arc::from(client);
    let second = tokio::spawn({
        let client = client.clone();
        async move { client.send(mib_frame(2)).await }
    });
    advance_settled(Duration::from_millis(990)).await;
    assert!(!second.is_finished(), "the second send waits for the first frame's bytes");
    advance_settled(Duration::from_millis(20)).await;
    assert!(second.is_finished(), "it returns once the first frame left");
    second.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_send_that_fits_the_buffer_returns_at_once() {
    let (client, _server) = rated_pair(2 * MIB).await;
    client.send(mib_frame(1)).await.unwrap();
    let sent = tokio::time::timeout(Duration::from_millis(1), client.send(mib_frame(2))).await;
    assert!(sent.is_ok(), "1 MiB queued ahead fits a 2 MiB buffer");
}
