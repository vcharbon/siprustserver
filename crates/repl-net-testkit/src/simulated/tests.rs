use super::*;
use std::sync::Arc as StdArc;
use std::time::Duration;

use sip_clock::testkit::advance_settled;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn data_frame(counter: u64, body: &[u8]) -> Frame {
    Frame::Data {
        at: crate::Watermark::new(1, counter),
        op: crate::Op::Put,
        partition: crate::Partition::Bak,
        call_ref: format!("p|call{counter}|tag"),
        call_gen: 7,
        call_bgen: 0,
        body_ttl_ms: 1000,
        origin_now_ms: 0,
        indexes: vec!["idx".into()],
        body: Some(StdArc::from(body)),
        answered: false,
        incarnation: None,
    }
}

fn noop(counter: u64) -> Frame {
    Frame::Noop { at: crate::Watermark::new(1, counter) }
}

/// The opening frame of a puller that names itself `caller` — what
/// attributes the stream's ephemeral local to the declared node.
fn pull_request(caller: &str) -> Frame {
    Frame::PullRequest {
        proto_ver: 3,
        caller: caller.into(),
        identity: None,
        partition: crate::Partition::Bak,
        since: crate::Watermark::new(1, 0),
    }
}

/// Open a stream from a synthetic local to `b`'s listener and attribute
/// it to the node `caller` names, the way a puller does: `connect`, then a
/// `PullRequest` the server end reads.
async fn attributed_pair(
    net: &SimulatedReplicationNetwork,
    listener: &dyn ReplicationListener,
    b: SocketAddr,
    caller: &str,
) -> (Box<dyn ReplicationConnection>, Box<dyn ReplicationConnection>) {
    let client = net.connect(b).await.unwrap();
    let server = listener.accept().await.unwrap();
    client.send(pull_request(caller)).await.unwrap();
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(server.recv().await, Some(pull_request(caller)));
    (client, server)
}

/// Whether `conn` has a frame to `recv` right now, without waiting.
async fn has_frame(conn: &dyn ReplicationConnection) -> Option<Frame> {
    tokio::time::timeout(Duration::from_micros(1), conn.recv()).await.ok().flatten()
}

/// `send`, then let the delivery actor stamp the frame's deadline at this
/// instant — a deadline is computed when the actor is polled, so a send
/// followed straight by an `advance` would be stamped after the advance.
async fn send_now(conn: &dyn ReplicationConnection, frame: Frame) -> Result<(), SendError> {
    let r = conn.send(frame).await;
    sip_clock::testkit::settle().await;
    r
}

/// Open a connected (client, server) pair from A to B's listener.
async fn connected_pair(
    net: &SimulatedReplicationNetwork,
    a: SocketAddr,
    b: SocketAddr,
) -> (Box<dyn ReplicationConnection>, Box<dyn ReplicationConnection>, Box<dyn ReplicationListener>)
{
    let listener = net.listen(b).await.unwrap();
    let client = net.connect_from(a, b).await.unwrap();
    let server = listener.accept().await.unwrap();
    (client, server, listener)
}

#[tokio::test(start_paused = true)]
async fn connect_send_recv_happy_path_bidirectional() {
    let net = SimulatedReplicationNetwork::with_delay(5);
    let (client, server, _l) = connected_pair(&net, addr(1000), addr(2000)).await;

    // A → B
    let f = noop(1);
    client.send(f.clone()).await.unwrap();
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(server.recv().await, Some(f));

    // B → A (bidirectional, identical frame round-trips through bytes)
    let g = data_frame(2, b"hello-body");
    server.send(g.clone()).await.unwrap();
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(client.recv().await, Some(g));
}

#[tokio::test(start_paused = true)]
async fn ordering_preserved_for_n_frames() {
    let net = SimulatedReplicationNetwork::with_delay(3);
    let (client, server, _l) = connected_pair(&net, addr(1001), addr(2001)).await;

    for i in 0..10 {
        client.send(noop(i)).await.unwrap();
    }
    advance_settled(Duration::from_millis(50)).await;
    for i in 0..10 {
        assert_eq!(server.recv().await, Some(noop(i)));
    }
}

#[tokio::test(start_paused = true)]
async fn transit_delay_blocks_until_elapsed_and_zero_is_coerced() {
    // Request 0 → coerced to >= 1 ms: NOT delivered instantly.
    let net = SimulatedReplicationNetwork::with_delay(0);
    let (client, server, _l) = connected_pair(&net, addr(1002), addr(2002)).await;

    client.send(noop(1)).await.unwrap();

    // Nothing before the (coerced 1 ms) delay elapses.
    let early = tokio::time::timeout(Duration::from_micros(1), server.recv()).await;
    assert!(early.is_err(), "delivered before transit delay (0 not coerced?)");

    advance_settled(Duration::from_millis(2)).await;
    assert_eq!(server.recv().await, Some(noop(1)));
}

#[tokio::test(start_paused = true)]
async fn transit_delay_explicit_window() {
    let net = SimulatedReplicationNetwork::with_delay(50);
    let (client, server, _l) = connected_pair(&net, addr(1003), addr(2003)).await;
    client.send(noop(1)).await.unwrap();

    advance_settled(Duration::from_millis(40)).await;
    let before = tokio::time::timeout(Duration::from_micros(1), server.recv()).await;
    assert!(before.is_err(), "delivered before 50ms");

    advance_settled(Duration::from_millis(20)).await;
    assert_eq!(server.recv().await, Some(noop(1)));
}

#[tokio::test(start_paused = true)]
async fn cut_yields_none_and_send_fails() {
    let net = SimulatedReplicationNetwork::with_delay(5);
    let (a, b) = (addr(1004), addr(2004));
    let (client, server, _l) = connected_pair(&net, a, b).await;

    // Cut both directions of this pair.
    net.apply_fault(Fault::Cut { src: a, dst: b });
    net.apply_fault(Fault::Cut { src: b, dst: a });
    advance_settled(Duration::from_millis(10)).await;

    assert_eq!(server.recv().await, None, "recv should yield None after cut");
    assert_eq!(client.recv().await, None);
    // Future send fails.
    let r = client.send(noop(1)).await;
    assert!(matches!(r, Err(SendError::Closed)));
}

#[tokio::test(start_paused = true)]
async fn stall_then_resume_delivers_buffered_in_order() {
    let net = SimulatedReplicationNetwork::with_delay(5);
    let (a, b) = (addr(1005), addr(2005));
    let (client, server, _l) = connected_pair(&net, a, b).await;

    net.apply_fault(Fault::Stall { src: a, dst: b });
    for i in 0..5 {
        client.send(noop(i)).await.unwrap();
    }
    advance_settled(Duration::from_millis(50)).await;
    // Stalled: nothing delivered.
    let blocked = tokio::time::timeout(Duration::from_micros(1), server.recv()).await;
    assert!(blocked.is_err(), "stall leaked a frame");

    net.apply_fault(Fault::Resume { src: a, dst: b });
    advance_settled(Duration::from_millis(50)).await;
    for i in 0..5 {
        assert_eq!(server.recv().await, Some(noop(i)), "out of order after resume");
    }
}

#[tokio::test(start_paused = true)]
async fn partition_cuts_both_then_heal_allows_reconnect() {
    let net = SimulatedReplicationNetwork::with_delay(5);
    let (a, b) = (addr(1006), addr(2006));
    let (client, server, listener) = connected_pair(&net, a, b).await;

    net.apply_fault(Fault::Partition { a, b });
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(client.recv().await, None);
    assert_eq!(server.recv().await, None);

    // New connect blocked while partitioned.
    let blocked = net.connect_from(a, b).await;
    assert!(matches!(blocked, Err(ConnectError::Blocked { .. })));

    // Heal → reconnect succeeds (fresh client/server pair).
    net.apply_fault(Fault::Heal { a, b });
    let client2 = net.connect_from(a, b).await.unwrap();
    let server2 = listener.accept().await.unwrap();
    client2.send(noop(99)).await.unwrap();
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(server2.recv().await, Some(noop(99)));
}

#[tokio::test(start_paused = true)]
async fn buffer_overflow_drop_subscriber() {
    // Tiny buffer, drop-on-overflow armed, no drainer → cut.
    let net = SimulatedReplicationNetwork::new(5, 2);
    let (a, b) = (addr(1007), addr(2007));
    net.apply_fault(Fault::DropOnOverflow { src: a, dst: b });
    let (client, server, _l) = connected_pair(&net, a, b).await;

    // Send more than the buffer holds; never drain `server`, so the bounded
    // buffer fills and the actor drops the subscriber (cuts). Crucially we
    // do NOT recv during the advance — a concurrent drain would keep pace
    // and the buffer would never overflow ("drive the protocol between
    // advances" hazard).
    for i in 0..10 {
        // send itself never blocks (unbounded staging); delivery actor cuts.
        let _ = client.send(noop(i)).await;
    }
    // Let the actor fill the buffer (cap 2) and trip the overflow → cut.
    // Advance past all transit deadlines, then yield (without recv) so the
    // woken actor runs to completion: it fills the bounded buffer and, on
    // the first frame that does not fit, drops the subscriber. Crucially we
    // do NOT recv here — a concurrent drain would free slots and the buffer
    // would never overflow.
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    advance_settled(Duration::from_millis(100)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // The subscriber was dropped: the outbound (c2s) direction is cut, so a
    // further send fails — the load-bearing signal of the overflow drop.
    assert!(
        matches!(client.send(noop(100)).await, Err(SendError::Closed)),
        "drop-on-overflow should have cut the subscriber",
    );

    // Draining the bounded prefix terminates in `None`, and far fewer than
    // the 10 sent frames buffered (proving the buffer was bounded).
    let mut drained = 0;
    while server.recv().await.is_some() {
        drained += 1;
        assert!(drained < 10, "buffer was not bounded; drained {drained}");
    }
}

#[tokio::test(start_paused = true)]
async fn backpressure_does_not_lose_ordering_with_tiny_buffer() {
    // Default (await) backpressure: tiny buffer, drain slowly, all frames
    // arrive in order, none lost.
    let net = SimulatedReplicationNetwork::new(3, 2);
    let (client, server, _l) = connected_pair(&net, addr(1008), addr(2008)).await;

    for i in 0..6 {
        client.send(noop(i)).await.unwrap();
    }
    // Drain one at a time, advancing between each so the parked delivery
    // actor wakes and refills the bounded buffer.
    for i in 0..6 {
        advance_settled(Duration::from_millis(10)).await;
        assert_eq!(server.recv().await, Some(noop(i)));
    }
}

#[tokio::test(start_paused = true)]
async fn connect_refused_when_no_listener() {
    let net = SimulatedReplicationNetwork::with_delay(5);
    let r = net.connect_from(addr(1009), addr(2009)).await;
    assert!(matches!(r, Err(ConnectError::Refused(_))));
}

// --- BlackHole: a connect nobody answers ------------------------------------

#[tokio::test(start_paused = true)]
async fn a_connect_to_a_black_hole_waits_until_healed() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(1010), addr(2010));
    let listener = net.listen(b).await.unwrap();
    net.apply_fault(Fault::BlackHole { dst: b });

    let pending = tokio::spawn({
        let net = net.clone();
        async move { net.connect_from(a, b).await.map(|_| ()) }
    });
    advance_settled(Duration::from_secs(60)).await;
    assert!(!pending.is_finished(), "a black-holed connect neither succeeds nor fails");

    net.apply_fault(Fault::Heal { a: b, b: a });
    advance_settled(Duration::from_millis(10)).await;
    assert!(pending.is_finished(), "a heal naming the address lets the connect through");
    assert!(pending.await.unwrap().is_ok(), "and it is established");
    assert!(listener.accept().await.is_ok(), "the listener takes the stream");
}

// --- AcceptErrors: accepts that fail, the listener kept ---------------------

#[tokio::test(start_paused = true)]
async fn armed_accept_errors_fail_that_many_accepts_and_the_queue_waits() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(1011), addr(2011));
    let listener = net.listen(b).await.unwrap();
    net.apply_fault(Fault::AcceptErrors { local: b, count: 2 });
    let _client = net.connect_from(a, b).await.unwrap();
    for _ in 0..2 {
        assert!(
            matches!(listener.accept().await, Err(crate::transport::AcceptError::Io(_))),
            "an armed accept fails with an io error"
        );
    }
    assert!(listener.accept().await.is_ok(), "the queued connection is accepted after");
    drop(listener);
    assert!(net.connect_from(a, b).await.is_err(), "a dropped listener frees the address");
}

// --- Block: half-open / hung peer — writer blocks, no error/close ----------

#[tokio::test(start_paused = true)]
async fn block_fault_blocks_writer_until_peer_pulls() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Window cap 2: the in-flight window holds two unacked frames.
    let net = SimulatedReplicationNetwork::new(1, 2);
    let (a, b) = (addr(1100), addr(2100));
    let (client, server, _l) = connected_pair(&net, a, b).await;
    net.apply_fault(Fault::Block { src: a, dst: b });

    let sent = StdArc::new(AtomicUsize::new(0));
    let s2 = sent.clone();
    let h = tokio::spawn(async move {
        for i in 0..3u64 {
            client.send(noop(i)).await.unwrap();
            s2.fetch_add(1, Ordering::SeqCst);
        }
        client // hand back for cleanup
    });

    // The window holds 2; the 3rd send BLOCKS because the peer isn't pulling.
    advance_settled(Duration::from_millis(20)).await;
    assert_eq!(sent.load(Ordering::SeqCst), 2, "writer blocks once the window fills");
    assert!(!h.is_finished(), "send is parked — no error, no close");

    // The peer pulls one frame → releases a window slot → the 3rd completes.
    assert_eq!(server.recv().await, Some(noop(0)));
    advance_settled(Duration::from_millis(20)).await;
    assert_eq!(sent.load(Ordering::SeqCst), 3, "writer unblocks once the peer drains");
    let _client = h.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn block_fault_cleared_by_resume_unblocks_writer() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let net = SimulatedReplicationNetwork::new(1, 1); // window cap 1
    let (a, b) = (addr(1101), addr(2101));
    let (client, _server, _l) = connected_pair(&net, a, b).await;
    net.apply_fault(Fault::Block { src: a, dst: b });

    let sent = StdArc::new(AtomicUsize::new(0));
    let s2 = sent.clone();
    let h = tokio::spawn(async move {
        for i in 0..2u64 {
            client.send(noop(i)).await.unwrap();
            s2.fetch_add(1, Ordering::SeqCst);
        }
    });

    advance_settled(Duration::from_millis(20)).await;
    assert_eq!(sent.load(Ordering::SeqCst), 1, "blocked at the 1-frame window");

    // Resume clears the block (peer recovers) → the parked write completes
    // without the peer ever pulling.
    net.apply_fault(Fault::Resume { src: a, dst: b });
    advance_settled(Duration::from_millis(20)).await;
    assert_eq!(sent.load(Ordering::SeqCst), 2, "Resume unblocks the writer");
    h.await.unwrap();
}

// --- ErrorAfter: network reset after a delay -------------------------------

#[tokio::test(start_paused = true)]
async fn error_after_delay_faults_established_connection() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(1102), addr(2102));
    let (client, server, _l) = connected_pair(&net, a, b).await;

    client.send(noop(1)).await.unwrap();
    advance_settled(Duration::from_millis(5)).await;
    assert_eq!(server.recv().await, Some(noop(1)), "healthy before the error");

    net.apply_fault(Fault::ErrorAfter { src: a, dst: b, ms: 50 });
    advance_settled(Duration::from_millis(60)).await;

    // After the delay the direction errors. Drive the teardown via the peer's
    // recv first (it parks, letting the error-timer + actor run) → the peer
    // sees a reset; then the local send observes the network error.
    assert_eq!(server.recv().await, None, "the peer sees a reset (recv → None)");
    assert!(
        matches!(client.send(noop(2)).await, Err(SendError::Io(_))),
        "established send returns a network error after the delay"
    );
}

#[tokio::test(start_paused = true)]
async fn error_after_delay_rejects_fresh_connect() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(1103), addr(2103));
    let _l = net.listen(b).await.unwrap();
    net.apply_fault(Fault::ErrorAfter { src: a, dst: b, ms: 50 });

    let net2 = net.clone();
    let h = tokio::spawn(async move { net2.connect_from(a, b).await });
    advance_settled(Duration::from_millis(60)).await;

    let r = h.await.unwrap();
    assert!(
        matches!(r, Err(ConnectError::Io(_))),
        "a fresh connect is rejected with a network error after the delay"
    );
}
// --- Directed faults named on the listen pair reach attributed streams ---
//
// A puller opens its stream from an ephemeral local, so a `Delay`, `Stall`
// or `Cut` named on the two listen addresses runs on a pair no wire uses
// unless the fabric reads each end as its owning node. Each test declares
// both endpoints, attributes the stream through a `PullRequest`, then
// names the fault on the listen pair in the server → client direction.

#[tokio::test(start_paused = true)]
async fn attributed_stream_obeys_a_delay_named_on_the_listen_pair() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(3100), addr(3200));
    net.declare_endpoint("A", a);
    net.declare_endpoint("B", b);
    let listener = net.listen(b).await.unwrap();
    let (client, server) = attributed_pair(&net, &*listener, b, "A").await;

    net.apply_fault(Fault::Delay { src: b, dst: a, ms: 50 });
    send_now(&*server, noop(1)).await.unwrap();
    advance_settled(Duration::from_millis(40)).await;
    assert_eq!(
        has_frame(&*client).await,
        None,
        "a delay named on the listen pair did not reach the attributed stream: the frame \
             landed before 50 ms"
    );
    advance_settled(Duration::from_millis(20)).await;
    assert_eq!(client.recv().await, Some(noop(1)), "the frame lands once the delay elapses");
}

#[tokio::test(start_paused = true)]
async fn attributed_stream_obeys_a_stall_named_on_the_listen_pair() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(3101), addr(3201));
    net.declare_endpoint("A", a);
    net.declare_endpoint("B", b);
    let listener = net.listen(b).await.unwrap();
    let (client, server) = attributed_pair(&net, &*listener, b, "A").await;

    net.apply_fault(Fault::Stall { src: b, dst: a });
    for i in 0..3 {
        send_now(&*server, noop(i)).await.unwrap();
    }
    advance_settled(Duration::from_millis(50)).await;
    assert_eq!(
        has_frame(&*client).await,
        None,
        "a stall named on the listen pair did not reach the attributed stream: a frame \
             landed"
    );
    net.apply_fault(Fault::Resume { src: b, dst: a });
    advance_settled(Duration::from_millis(50)).await;
    for i in 0..3 {
        assert_eq!(client.recv().await, Some(noop(i)), "in order after the resume");
    }
}

#[tokio::test(start_paused = true)]
async fn attributed_stream_obeys_a_cut_named_on_the_listen_pair() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(3102), addr(3202));
    net.declare_endpoint("A", a);
    net.declare_endpoint("B", b);
    let listener = net.listen(b).await.unwrap();
    let (client, server) = attributed_pair(&net, &*listener, b, "A").await;

    net.apply_fault(Fault::Cut { src: b, dst: a });
    assert!(
        matches!(send_now(&*server, noop(1)).await, Err(SendError::Closed)),
        "a cut named on the listen pair did not reach the attributed stream: the send after \
             the cut was accepted"
    );
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(
        client.recv().await,
        None,
        "a cut named on the listen pair did not reach the attributed stream: recv did not \
             close"
    );
}

#[tokio::test(start_paused = true)]
async fn a_held_frame_stays_held_after_its_client_handle_is_dropped() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b, c, d) = (addr(3106), addr(3206), addr(3306), addr(3406));
    net.declare_endpoint("A", a);
    net.declare_endpoint("B", b);
    net.declare_endpoint("C", c);
    net.declare_endpoint("D", d);
    let listener = net.listen(b).await.unwrap();
    let (client, server) = attributed_pair(&net, &*listener, b, "A").await;

    // Held on the owner pair, then the client end goes away (a crash drops
    // the handle while the wire still holds the frame).
    net.apply_fault(Fault::Partition { a, b });
    send_now(&*client, noop(1)).await.unwrap();
    drop(client);
    // A node fault on another pair wakes every wire: the held wire re-reads
    // its hold.
    net.apply_fault(Fault::Delay { src: c, dst: d, ms: 5 });
    advance_settled(Duration::from_millis(50)).await;
    assert_eq!(
        has_frame(&*server).await,
        None,
        "a frame held by the partition was delivered once its client handle was dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stream_opened_after_a_listen_pair_fault_obeys_it() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(3103), addr(3203));
    net.declare_endpoint("A", a);
    net.declare_endpoint("B", b);
    let listener = net.listen(b).await.unwrap();
    // An earlier stream exists when the fault is named, as a puller's does.
    let (_client0, _server0) = attributed_pair(&net, &*listener, b, "A").await;

    net.apply_fault(Fault::Delay { src: b, dst: a, ms: 50 });
    let (client, server) = attributed_pair(&net, &*listener, b, "A").await;
    send_now(&*server, noop(1)).await.unwrap();
    advance_settled(Duration::from_millis(40)).await;
    assert_eq!(
        has_frame(&*client).await,
        None,
        "a stream opened after the fault did not inherit the delay named on the listen \
             pair: the frame landed before 50 ms"
    );
    advance_settled(Duration::from_millis(20)).await;
    assert_eq!(client.recv().await, Some(noop(1)));
}

#[tokio::test(start_paused = true)]
async fn a_node_handle_is_refused_a_connect_under_a_partition() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b, c) = (addr(3105), addr(3205), addr(3305));
    net.declare_endpoint("A", a);
    net.declare_endpoint("B", b);
    net.declare_endpoint("C", c);
    let listener = net.listen(b).await.unwrap();

    net.apply_fault(Fault::Partition { a, b });
    assert!(
        matches!(net.as_node(a).connect(b).await, Err(ConnectError::Blocked { .. })),
        "a node behind a partition is refused the connect, not held after it"
    );
    let _other = net.as_node(c).connect(b).await.expect("an unpartitioned node connects");
    let _other_server = listener.accept().await.unwrap();

    // Healed: the connect succeeds and the stream is attributed at once — a
    // delay named on the listen pair governs its first frame, before any
    // `PullRequest` names the caller.
    net.apply_fault(Fault::Heal { a, b });
    let client = net.as_node(a).connect(b).await.unwrap();
    let server = listener.accept().await.unwrap();
    net.apply_fault(Fault::Delay { src: b, dst: a, ms: 50 });
    send_now(&*server, noop(1)).await.unwrap();
    advance_settled(Duration::from_millis(40)).await;
    assert_eq!(has_frame(&*client).await, None, "attributed at connect: the frame waits");
    advance_settled(Duration::from_millis(20)).await;
    assert_eq!(client.recv().await, Some(noop(1)));
}

/// Two live incarnations of one ordinal, each on its own listen address: the
/// ordinal declared again at the successor's re-points `PullRequest`
/// attribution, yet a stream the elder opens through its own handle stays the
/// elder's — a partition named on the successor holds none of it, one named
/// on the elder holds it, and a heal on the elder's address lifts it.
#[tokio::test(start_paused = true)]
async fn an_elder_incarnation_keeps_its_streams_beside_its_successor() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (elder, successor, b) = (addr(3108), addr(3308), addr(3208));
    net.declare_endpoint("A", elder);
    net.declare_endpoint("B", b);
    let listener = net.listen(b).await.unwrap();
    let elder_net = net.as_node(elder);
    net.declare_endpoint("A", successor);

    net.apply_fault(Fault::Partition { a: successor, b });
    let client = elder_net.connect(b).await.expect("the elder is not partitioned");
    let server = listener.accept().await.unwrap();
    client.send(pull_request("A")).await.unwrap();
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(
        server.recv().await,
        Some(pull_request("A")),
        "the elder's stream was read as its successor's: its PullRequest re-attributed it"
    );
    send_now(&*server, noop(1)).await.unwrap();
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(client.recv().await, Some(noop(1)), "and it flows both ways");

    net.apply_fault(Fault::Partition { a: elder, b });
    send_now(&*server, noop(2)).await.unwrap();
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(has_frame(&*client).await, None, "a partition on the elder holds its stream");
    net.apply_fault(Fault::Heal { a: elder, b });
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(client.recv().await, Some(noop(2)), "a heal on the elder releases it");
}

#[tokio::test(start_paused = true)]
async fn a_handle_dropped_before_the_actor_polls_leaves_its_frame_held() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(3107), addr(3207));
    net.declare_endpoint("A", a);
    net.declare_endpoint("B", b);
    let listener = net.listen(b).await.unwrap();
    net.apply_fault(Fault::Stall { src: a, dst: b });

    // Bare fabric: the stream is attributed by its `PullRequest`, and the
    // handle goes away before the wire actor ever polled — no await between
    // the send and the drop.
    let client = net.connect(b).await.unwrap();
    let server = listener.accept().await.unwrap();
    client.send(pull_request("A")).await.unwrap();
    drop(client);
    // The actor's first poll — and the frame's stamping — happen here, with
    // the handle already gone; then the transit elapses.
    sip_clock::testkit::settle().await;
    advance_settled(Duration::from_millis(50)).await;
    assert_eq!(
        has_frame(&*server).await,
        None,
        "a frame on a stall named on the listen pair was delivered: the wire lost its \
             owner when the handle dropped before the actor's first poll"
    );
}

#[tokio::test(start_paused = true)]
async fn a_pull_request_from_a_server_end_attributes_nothing() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(3108), addr(3208));
    net.declare_endpoint("A", a);
    net.declare_endpoint("B", b);
    let listener = net.listen(b).await.unwrap();
    let (client, server) = attributed_pair(&net, &*listener, b, "A").await;

    // The listener's end names A on a `PullRequest`: its address stays its
    // own owner, and the client's attribution stands.
    server.send(pull_request("A")).await.unwrap();
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(client.recv().await, Some(pull_request("A")));
    let owners = net.shared.stream_owner.lock().unwrap().clone();
    assert!(!owners.contains_key(&b), "the listener end was attributed: {owners:?}");
    assert_eq!(owners.get(&client.local_addr()), Some(&a), "the client end keeps its owner");
}

#[tokio::test(start_paused = true)]
async fn an_undeclared_pair_keeps_per_direction_semantics() {
    let net = SimulatedReplicationNetwork::with_delay(1);
    let (a, b) = (addr(3104), addr(3204));
    let listener = net.listen(b).await.unwrap();
    // An explicit local IS the directed pair the fault names.
    let explicit = net.connect_from(a, b).await.unwrap();
    let explicit_server = listener.accept().await.unwrap();
    // A synthetic local with no declaration stands for itself.
    let (synthetic, synthetic_server) = attributed_pair(&net, &*listener, b, "A").await;

    net.apply_fault(Fault::Delay { src: b, dst: a, ms: 50 });
    send_now(&*explicit_server, noop(1)).await.unwrap();
    send_now(&*synthetic_server, noop(2)).await.unwrap();
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(
        has_frame(&*explicit).await,
        None,
        "the explicit-local pair is the direction named: its frame waits the delay"
    );
    assert_eq!(
        synthetic.recv().await,
        Some(noop(2)),
        "an undeclared synthetic-local stream is not the direction named: its frame lands \
             on the default transit"
    );
    advance_settled(Duration::from_millis(50)).await;
    assert_eq!(explicit.recv().await, Some(noop(1)));
}
