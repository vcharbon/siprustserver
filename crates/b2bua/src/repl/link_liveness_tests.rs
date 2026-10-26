//! Replication links stay live past bootstrap: a peer that goes silent, a
//! connect nobody answers, and a peer that stops reading are each detected on
//! a bound, and the link recovers through the ordinary backoff from where it
//! stood.
//!
//! ```text
//!   puller   silence ≥ 15 s after the last frame   → reconnect, since = W
//!   puller   connect with no answer                → given up at 2 s, backoff
//!   puller   one send blocked 15 s                 → stream left, backoff
//!   puller   a stream that lasted the idle bound   → backoff from its floor
//!   server   one send blocked 15 s                 → stream dropped, the
//!                                                     peer's log reapable
//!   server   a Noop at least every noop interval   → a quiet stream is kept
//!   server   accepts that fail                     → accepting goes on
//! ```

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use repl_net::frame::{Frame, Partition, Watermark};
use repl_net::transport::{
    ConnectError, ListenError, ReplicationConnection, ReplicationListener, ReplicationNetwork,
};
use repl_net_testkit::{Fault, SimulatedReplicationNetwork};
use sip_clock::Clock;
use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use super::test_support::{cref, fast_config, tick, Node};
use super::{Changelog, Puller, PullerStatus, ReplServer, ReplicatingCallStore, ServeConfig};
use crate::store::{CallStore, PartitionRole, PutOpts};

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9950 + n))
}

/// The watermark a scripted peer announces on its catch-up `Noop`.
const W: Watermark = Watermark { gen: 7, counter: 42 };

/// One `PullRequest` a scripted peer read: when, and from which watermark.
#[derive(Clone, Copy, Debug)]
struct Pull {
    at: Instant,
    since: Watermark,
}

/// A scripted peer: every stream it accepts reads the opening `PullRequest`
/// (recorded); the first stream is answered with one catch-up `Noop{W}` and
/// nothing after it. Each stream ends when the puller closes it, or when the
/// test calls [`hang_up`](Self::hang_up).
struct ScriptedPeer {
    pulls: Arc<Mutex<Vec<Pull>>>,
    /// When the catch-up `Noop` left.
    noop_sent: Arc<Mutex<Option<Instant>>>,
    hang_up: Arc<Notify>,
}

impl ScriptedPeer {
    async fn listen(net: &Arc<SimulatedReplicationNetwork>, at: SocketAddr) -> Self {
        let listener = net.listen(at).await.unwrap();
        let peer = Self {
            pulls: Arc::default(),
            noop_sent: Arc::default(),
            hang_up: Arc::new(Notify::new()),
        };
        let (pulls, noop_sent, hang_up) =
            (peer.pulls.clone(), peer.noop_sent.clone(), peer.hang_up.clone());
        tokio::spawn(async move {
            while let Ok(conn) = listener.accept().await {
                let (pulls, noop_sent, hang_up) =
                    (pulls.clone(), noop_sent.clone(), hang_up.clone());
                tokio::spawn(async move {
                    let Some(Frame::PullRequest { since, .. }) = conn.recv().await else {
                        return;
                    };
                    let first = {
                        let mut pulls = pulls.lock().unwrap();
                        pulls.push(Pull { at: Instant::now(), since });
                        pulls.len() == 1
                    };
                    if first {
                        *noop_sent.lock().unwrap() = Some(Instant::now());
                        let _ = conn.send(Frame::Noop { at: W }).await;
                    }
                    // Read the puller's positions until it closes, or hang up.
                    tokio::select! {
                        () = async { while conn.recv().await.is_some() {} } => {}
                        () = hang_up.notified() => {}
                    }
                });
            }
        });
        peer
    }

    fn pulls(&self) -> Vec<Pull> {
        self.pulls.lock().unwrap().clone()
    }

    fn noop_sent(&self) -> Instant {
        self.noop_sent.lock().unwrap().expect("the catch-up Noop was sent")
    }

    /// Close every stream the peer holds.
    fn hang_up(&self) {
        self.hang_up.notify_waiters();
    }
}

/// How one connect attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    Established,
    Failed,
    /// The caller stopped waiting for it.
    GivenUp,
}

#[derive(Clone, Copy, Debug)]
struct Attempt {
    started: Instant,
    ended: Option<(Instant, Ended)>,
}

/// A network recording when each `connect` started and how it ended.
struct ConnectProbe {
    inner: Arc<dyn ReplicationNetwork>,
    attempts: Arc<Mutex<Vec<Attempt>>>,
}

/// Records a connect attempt's end, a dropped one as given up.
struct AttemptGuard {
    attempts: Arc<Mutex<Vec<Attempt>>>,
    index: usize,
}

impl AttemptGuard {
    fn end(&self, how: Ended) {
        let mut attempts = self.attempts.lock().unwrap();
        let attempt = &mut attempts[self.index];
        if attempt.ended.is_none() {
            attempt.ended = Some((Instant::now(), how));
        }
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        self.end(Ended::GivenUp);
    }
}

#[async_trait]
impl ReplicationNetwork for ConnectProbe {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        let index = {
            let mut attempts = self.attempts.lock().unwrap();
            attempts.push(Attempt { started: Instant::now(), ended: None });
            attempts.len() - 1
        };
        let guard = AttemptGuard { attempts: self.attempts.clone(), index };
        let result = self.inner.connect(dst).await;
        guard.end(if result.is_ok() { Ended::Established } else { Ended::Failed });
        result
    }

    async fn listen(&self, local: SocketAddr) -> Result<Box<dyn ReplicationListener>, ListenError> {
        self.inner.listen(local).await
    }
}

/// A Reclaim puller from `A` toward `B` at `peer` over `net`, cold, on the
/// fast test tier. The returned sender keeps it running.
fn pull_from(
    net: Arc<dyn ReplicationNetwork>,
    peer: SocketAddr,
) -> (watch::Sender<bool>, watch::Receiver<PullerStatus>) {
    let (puller, status) = Puller::new_at(
        "B",
        "A",
        Partition::Pri,
        peer,
        net,
        ReplicatingCallStore::new(2, Clock::test_at(0)),
        fast_config(),
        Watermark::new(0, 0),
        crate::metrics::B2buaMetrics::new(),
    );
    let (cancel_tx, cancel_rx) = watch::channel(false);
    tokio::spawn(puller.run(cancel_rx));
    (cancel_tx, status)
}

/// Advance the paused clock to `at`, in settled steps of at most 100 ms.
async fn tick_until(at: Instant) {
    while Instant::now() < at {
        let step = (at - Instant::now()).min(Duration::from_millis(100));
        sip_clock::testkit::advance_settled(step).await;
    }
}

const IDLE: Duration = Duration::from_secs(15);
/// The default bound on one blocked send.
const SEND_STALL: Duration = Duration::from_secs(15);
const CONNECT_BOUND: Duration = Duration::from_secs(2);
/// The fast tier's backoff floor ([`fast_config`]).
const BACKOFF_FLOOR: Duration = Duration::from_millis(100);

#[tokio::test(start_paused = true)]
async fn a_peer_silent_after_bootstrap_is_left_after_15_s_and_pulled_again_from_its_watermark() {
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(1);
    let peer = ScriptedPeer::listen(&net, b).await;
    let (_run, _) = pull_from(net, b);

    tick(100).await;
    let pulls = peer.pulls();
    assert_eq!(pulls.len(), 1, "one stream opened");
    assert_eq!(pulls[0].since, Watermark::new(0, 0), "a cold pull");
    let last_frame = peer.noop_sent();

    tick_until(last_frame + IDLE - Duration::from_millis(100)).await;
    assert_eq!(peer.pulls().len(), 1, "a stream quiet for less than 15 s is kept");

    tick_until(last_frame + IDLE + BACKOFF_FLOOR + Duration::from_millis(100)).await;
    let pulls = peer.pulls();
    assert_eq!(
        pulls.len(),
        2,
        "the puller leaves a stream silent for 15 s and opens another after one backoff step"
    );
    let gap = pulls[1].at - last_frame;
    assert!(
        gap >= IDLE && gap <= IDLE + BACKOFF_FLOOR + Duration::from_millis(10),
        "re-pulled {gap:?} after the last frame"
    );
    assert_eq!(pulls[1].since, W, "the new stream resumes from the watermark the Noop announced");
}

#[tokio::test(start_paused = true)]
async fn a_connect_nobody_answers_after_bootstrap_is_given_up_at_2_s_and_backed_off() {
    let sim = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(2);
    let peer = ScriptedPeer::listen(&sim, b).await;
    let probe = Arc::new(ConnectProbe {
        inner: sim.clone() as Arc<dyn ReplicationNetwork>,
        attempts: Arc::default(),
    });
    let (_run, _) = pull_from(probe.clone(), b);

    tick(100).await;
    assert_eq!(peer.pulls().len(), 1, "bootstrapped over the first stream");

    // The peer's address stops answering and its stream goes.
    sim.apply_fault(Fault::BlackHole { dst: b });
    peer.hang_up();
    tick(10_000).await;

    let attempts = probe.attempts.lock().unwrap().clone();
    assert!(attempts.len() >= 2, "the puller tried the peer again: {attempts:?}");
    let blind = attempts[1];
    let (ended, how) = blind.ended.expect("a connect with no answer is bounded");
    assert_ne!(how, Ended::Established, "{attempts:?}");
    assert_eq!(ended - blind.started, CONNECT_BOUND, "given up at the connect bound");
    assert!(attempts.len() >= 3, "the puller kept trying: {attempts:?}");
    let next = attempts[2];
    assert!(
        next.started > ended,
        "the next attempt waits a backoff step after the failed one: {attempts:?}"
    );
    assert!(
        next.started - ended <= Duration::from_millis(200),
        "the backoff grows from its floor, not from where the stream left it: {attempts:?}"
    );
}

/// A peer whose streams each deliver one frame and close, from the
/// `lasting`-th stream on (0-based) keeping that one open 16 s with a `Noop`
/// every 5 s. Records when each stream was opened and closed.
async fn closing_peer(
    net: &Arc<SimulatedReplicationNetwork>,
    at: SocketAddr,
    lasting: usize,
) -> Arc<Mutex<Vec<(Instant, Instant)>>> {
    let listener = net.listen(at).await.unwrap();
    let streams: Arc<Mutex<Vec<(Instant, Instant)>>> = Arc::default();
    let seen = streams.clone();
    tokio::spawn(async move {
        let mut n = 0;
        while let Ok(conn) = listener.accept().await {
            if let Some(Frame::PullRequest { .. }) = conn.recv().await {
                let opened = Instant::now();
                let _ = conn.send(Frame::Noop { at: W }).await;
                if n == lasting {
                    for _ in 0..3 {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let _ = conn.send(Frame::Noop { at: W }).await;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                } else {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                seen.lock().unwrap().push((opened, Instant::now()));
            }
            n += 1;
        }
    });
    streams
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_sends_one_frame_and_closes_is_redialled_with_a_growing_backoff() {
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(3);
    let streams = closing_peer(&net, b, usize::MAX).await;
    let (_run, _) = pull_from(net, b);

    tick(3_000).await;
    let opened: Vec<Instant> = streams.lock().unwrap().iter().map(|s| s.0).collect();
    let gaps: Vec<Duration> = opened.windows(2).map(|p| p[1] - p[0]).collect();
    assert!(gaps.len() >= 4, "the puller redials each closed stream: {gaps:?}");
    for pair in gaps.windows(2).take(3) {
        assert!(pair[1] > pair[0], "a stream that delivered once resets nothing: {gaps:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_lasted_the_idle_bound_resets_the_backoff_to_its_floor() {
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(5);
    let streams = closing_peer(&net, b, 5).await;
    let (_run, _) = pull_from(net, b);

    tick(25_000).await;
    let streams = streams.lock().unwrap().clone();
    assert!(streams.len() >= 7, "the puller redialled after the lasting stream: {streams:?}");
    let (opened, closed) = streams[5];
    assert!(closed - opened >= IDLE, "the sixth stream lasted the idle bound");
    let short_gap = streams[5].0 - streams[4].0;
    assert!(short_gap > BACKOFF_FLOOR * 4, "the backoff had grown before: {short_gap:?}");
    let redial = streams[6].0 - closed;
    assert!(
        redial <= BACKOFF_FLOOR + Duration::from_millis(20),
        "after a stream that lasted the idle bound the backoff starts from its floor: {redial:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_stops_reading_is_dropped_after_15_s_and_its_log_becomes_reapable() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(4);
    let node = Node::spawn("B", b, 3, &net, &clock).await;
    // B holds more of A's calls than one in-flight window carries.
    for i in 0..20 {
        let call_ref = cref("A", &i.to_string());
        node.store
            .put_call(
                PartitionRole::Backup,
                "A",
                &call_ref,
                b"body".to_vec(),
                &[],
                0,
                1,
                0,
                &PutOpts::default(),
            )
            .await
            .unwrap();
    }

    // A opens its Reclaim stream and then reads nothing: B's sends fill the
    // window and block.
    let a_local = addr(40);
    net.apply_fault(Fault::Block { src: b, dst: a_local });
    let client = net.connect_from(a_local, b).await.unwrap();
    client
        .send(Frame::PullRequest {
            proto_ver: repl_net::PROTO_VER,
            caller: "A".into(),
            identity: None,
            partition: Partition::Pri,
            since: Watermark::new(0, 0),
        })
        .await
        .unwrap();
    let opened = Instant::now();
    let changelog = node.store.changelog().clone();
    let far_future = i64::MAX / 2;

    tick_until(opened + Duration::from_secs(14)).await;
    changelog.reap(far_future);
    assert!(changelog.has_peer("A"), "a stream blocked for less than 15 s is still served");

    tick_until(opened + Duration::from_millis(15_500)).await;
    changelog.reap(far_future);
    assert!(
        !changelog.has_peer("A"),
        "the stream blocked for 15 s is dropped and A's log is reapable again"
    );

    // What B did send is still delivered; then the stream ends.
    let mut delivered = 0;
    let closed = loop {
        match tokio::time::timeout(Duration::from_secs(1), client.recv()).await {
            Ok(Some(_)) => delivered += 1,
            Ok(None) => break true,
            Err(_) => break false,
        }
    };
    assert!(closed, "B closed the stream after {delivered} frames");
}

#[tokio::test(start_paused = true)]
async fn a_quiet_live_stream_keeps_one_connection_across_several_idle_bounds() {
    let clock = Clock::test_at(0);
    let sim = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(7);
    let node = Node::spawn("B", b, 3, &sim, &clock).await;
    for i in 0..600 {
        let call_ref = cref("A", &i.to_string());
        node.store
            .put_call(
                PartitionRole::Backup,
                "A",
                &call_ref,
                vec![7u8; 512],
                &[],
                600_000,
                1,
                0,
                &PutOpts::default(),
            )
            .await
            .unwrap();
    }
    let probe = Arc::new(ConnectProbe {
        inner: sim.clone() as Arc<dyn ReplicationNetwork>,
        attempts: Arc::default(),
    });
    let (_run, status) = pull_from(probe.clone(), b);
    tick_until(Instant::now() + Duration::from_secs(120)).await;
    assert!(status.borrow().current, "the puller caught up");
    let attempts = probe.attempts.lock().unwrap().clone();
    assert_eq!(attempts.len(), 1, "a live quiet stream is never left: {attempts:?}");
}

#[tokio::test(start_paused = true)]
async fn a_served_stream_carries_a_noop_at_least_every_noop_interval_during_and_after_bootstrap() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(8);
    let changelog = Changelog::new(3, clock.clone());
    let store = ReplicatingCallStore::with_changelog(changelog.clone(), clock.clone());
    for i in 0..300 {
        let call_ref = cref("A", &i.to_string());
        store
            .put_call(
                PartitionRole::Backup,
                "A",
                &call_ref,
                vec![7u8; 64],
                &[],
                600_000,
                1,
                0,
                &PutOpts::default(),
            )
            .await
            .unwrap();
    }
    // A configured interval other than the default: the server keeps the one
    // it is given.
    let interval = Duration::from_secs(3);
    let config = ServeConfig { noop_interval: interval, send_stall: Duration::from_secs(15) };
    let server = ReplServer::new("B", changelog, Arc::new(store.clone())).with_config(config);
    tokio::spawn(server.run(net.listen(b).await.unwrap()));

    let client = net.connect_from(addr(41), b).await.unwrap();
    client
        .send(Frame::PullRequest {
            proto_ver: repl_net::PROTO_VER,
            caller: "A".into(),
            identity: None,
            partition: Partition::Pri,
            since: Watermark::new(0, 0),
        })
        .await
        .unwrap();
    let opened = Instant::now();
    let frames: Arc<Mutex<Vec<(Instant, bool)>>> = Arc::default();
    let seen = frames.clone();
    tokio::spawn(async move {
        while let Some(frame) = client.recv().await {
            seen.lock().unwrap().push((Instant::now(), matches!(frame, Frame::Noop { .. })));
        }
    });
    tick_until(opened + Duration::from_secs(30)).await;

    let frames = frames.lock().unwrap().clone();
    let data = frames.iter().filter(|f| !f.1).count();
    assert_eq!(data, 300, "the bootstrap scan was served");
    let mut last = opened;
    for (at, _) in &frames {
        assert!(*at - last <= interval + Duration::from_millis(200), "a gap of {:?}", *at - last);
        last = *at;
    }
    assert!(
        opened + Duration::from_secs(30) - last <= interval + Duration::from_millis(200),
        "the stream went quiet {:?} before the end",
        opened + Duration::from_secs(30) - last
    );
    let noops = frames.iter().filter(|f| f.1).count();
    assert!(noops >= 9, "a Noop at least every {interval:?}: {noops} in 30 s");
}

#[tokio::test(start_paused = true)]
async fn accepts_that_fail_never_stop_the_replication_server() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(9);
    net.apply_fault(Fault::AcceptErrors { local: b, count: 3 });
    let _node = Node::spawn("B", b, 3, &net, &clock).await;

    let (_first, first) = pull_from(net.clone(), b);
    tick(1_000).await;
    assert!(first.borrow().current, "the stream is served once the failing accepts are past");

    net.apply_fault(Fault::AcceptErrors { local: b, count: 2 });
    let (_second, second) = pull_from(net, b);
    tick(1_000).await;
    assert!(second.borrow().current, "a later episode is ridden out as well");
}

/// A connection whose sends past the opening `PullRequest` never complete:
/// a full socket buffer toward a peer that stopped reading.
struct DeafConn(Box<dyn ReplicationConnection>);

#[async_trait]
impl ReplicationConnection for DeafConn {
    async fn send(&self, frame: Frame) -> Result<(), repl_net::transport::SendError> {
        if matches!(frame, Frame::PullRequest { .. }) {
            return self.0.send(frame).await;
        }
        std::future::pending().await
    }
    async fn recv(&self) -> Option<Frame> {
        self.0.recv().await
    }
    fn peer_addr(&self) -> SocketAddr {
        self.0.peer_addr()
    }
    fn local_addr(&self) -> SocketAddr {
        self.0.local_addr()
    }
}

/// A network whose connections are [`DeafConn`]s.
struct DeafNet(Arc<SimulatedReplicationNetwork>);

#[async_trait]
impl ReplicationNetwork for DeafNet {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        Ok(Box::new(DeafConn(self.0.connect(dst).await?)))
    }
    async fn listen(&self, local: SocketAddr) -> Result<Box<dyn ReplicationListener>, ListenError> {
        self.0.listen(local).await
    }
}

#[tokio::test(start_paused = true)]
async fn a_puller_whose_position_send_stays_blocked_leaves_the_stream_after_15_s() {
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(10);
    let peer = ScriptedPeer::listen(&net, b).await;
    let (_run, _) = pull_from(Arc::new(DeafNet(net)), b);

    tick(100).await;
    let blocked_from = peer.noop_sent();
    tick_until(blocked_from + SEND_STALL - Duration::from_millis(100)).await;
    assert_eq!(peer.pulls().len(), 1, "a send blocked for less than 15 s keeps the stream");
    tick_until(blocked_from + SEND_STALL + BACKOFF_FLOOR + Duration::from_millis(100)).await;
    assert_eq!(peer.pulls().len(), 2, "the blocked stream is left and pulled again");
}
