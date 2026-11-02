//! A dead verdict meeting the Backup flow's stream down is confirmed by one
//! connect made at once (ADR-0047 D1): it skips the backoff, a connect in
//! flight starts afresh, and it is bounded by its own connect bound. A frame
//! on it speaks for the peer; a refusal or the bound confirms the verdict.
//!
//! ```text
//!   peer takes it           → a frame          → not confirmed, the backoff skipped
//!   open stream cut after the verdict, no frame → the same one connect
//!   peer refuses it         → confirmed at once
//!   peer answers nothing    → confirmed one connect bound after the verdict,
//!                             never at the bound of the connect in flight before it
//!   peer answers 1 ms inside / outside the bound → not confirmed / confirmed
//!   verdict at the instant the backoff ends      → not confirmed, either order
//!   (a stream that ends with no frame confirms it: `failover-harness`'s cells)
//!   multi-thread runtime, real clock             → not confirmed, no backoff waited
//!   a refused asked connect                      → the backoff schedule unchanged
//! ```

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use repl_net::frame::{Frame, Watermark};
use repl_net::transport::{
    ConnectError, ListenError, ReplicationConnection, ReplicationListener, ReplicationNetwork,
    SendError,
};
use sip_clock::Clock;
use tokio::sync::watch;
use tokio::time::Instant;
use topology::Peer;

use super::test_support::{fast_config, one_peer, tick};
use super::{FnPeerResolver, PullerConfig, ReplicatingCallStore, ReplicationSupervisor};

const PEER: &str = "w0";
const CONNECT_BOUND: Duration = Duration::from_millis(2_000);

fn peer_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9990))
}

/// How the scripted peer answers a connect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// It takes the connection: a `Noop` at once, then one every 400 ms.
    Take,
    Refuse,
    /// No answer while it lasts; a connect waiting meanwhile gets the next.
    Silence,
}

/// The peer's side of the network: how it answers, when each connect started,
/// and a hang-up that ends every stream it holds.
struct Script {
    answer: watch::Sender<Answer>,
    connects: Mutex<Vec<Instant>>,
    hang_up: watch::Sender<u64>,
}

impl Script {
    fn answer(&self, answer: Answer) {
        self.answer.send_replace(answer);
    }

    fn hang_up(&self) {
        self.hang_up.send_modify(|n| *n += 1);
    }

    fn last_connect(&self) -> Instant {
        *self.connects.lock().unwrap().last().expect("a connect was made")
    }
}

struct ScriptNet(Arc<Script>);

#[async_trait]
impl ReplicationNetwork for ScriptNet {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        self.0.connects.lock().unwrap().push(Instant::now());
        let mut answer = self.0.answer.subscribe();
        let answered = answer.wait_for(|a| *a != Answer::Silence).await.map(|a| *a);
        match answered {
            Ok(Answer::Take) => {
                let mut hang_up = self.0.hang_up.subscribe();
                hang_up.mark_unchanged();
                Ok(Box::new(Stream { opened: Mutex::new(false), hang_up: Mutex::new(hang_up) }))
            }
            _ => Err(ConnectError::Refused(dst)),
        }
    }

    async fn listen(
        &self,
        _local: SocketAddr,
    ) -> Result<Box<dyn ReplicationListener>, ListenError> {
        unreachable!("the scripted peer is pull-only")
    }
}

/// One stream the scripted peer took.
struct Stream {
    opened: Mutex<bool>,
    hang_up: Mutex<watch::Receiver<u64>>,
}

#[async_trait]
impl ReplicationConnection for Stream {
    async fn send(&self, _frame: Frame) -> Result<(), SendError> {
        if self.hung_up() {
            return Err(SendError::Closed);
        }
        Ok(())
    }

    async fn recv(&self) -> Option<Frame> {
        let first = !std::mem::replace(&mut *self.opened.lock().unwrap(), true);
        let gap = if first { Duration::from_millis(1) } else { Duration::from_millis(400) };
        let mut hang_up = self.hang_up.lock().unwrap().clone();
        tokio::select! {
            () = tokio::time::sleep(gap) => {
                (!self.hung_up()).then_some(Frame::Noop { at: Watermark::new(1, 1) })
            }
            _ = hang_up.changed() => None,
        }
    }

    fn peer_addr(&self) -> SocketAddr {
        peer_addr()
    }

    fn local_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 9991))
    }
}

impl Stream {
    fn hung_up(&self) -> bool {
        self.hang_up.lock().unwrap().has_changed().unwrap_or(true)
    }
}

/// A supervisor on `w1` backing up `w0`, both flows streaming, and the
/// peer's script.
async fn backing_up(config: PullerConfig) -> (ReplicationSupervisor, Arc<Script>) {
    let script = Arc::new(Script {
        answer: watch::Sender::new(Answer::Take),
        connects: Mutex::default(),
        hang_up: watch::Sender::new(0),
    });
    let clock = Clock::test_at(0);
    let store = ReplicatingCallStore::new(1, clock.clone());
    let resolve = Arc::new(FnPeerResolver(|_: &Peer| peer_addr()));
    let net = Arc::new(ScriptNet(script.clone()));
    let sup = ReplicationSupervisor::with_config("w1", net, store, resolve, config);
    sup.start(one_peer(PEER, &clock));
    (sup, script)
}

/// Run `confirms_dead` for a verdict taken now, on a task of its own.
fn verdict(sup: &ReplicationSupervisor) -> tokio::task::JoinHandle<bool> {
    let sup = sup.clone();
    let since = Instant::now();
    tokio::spawn(async move { sup.confirms_dead(PEER, since).await })
}

/// Settle the tasks due at this instant.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// The Backup flow's stream ended, its puller backing off between refused
/// connects; the peer then takes connects again.
async fn into_a_long_backoff(script: &Script) {
    script.answer(Answer::Refuse);
    script.hang_up();
    tick(1_600).await;
    script.answer(Answer::Take);
}

#[tokio::test(start_paused = true)]
async fn a_stream_down_skips_the_backoff_and_a_frame_on_it_refutes_the_verdict() {
    let (sup, script) = backing_up(fast_config()).await;
    tick(1_000).await;
    into_a_long_backoff(&script).await;
    let at = Instant::now();
    let confirmed = verdict(&sup);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(confirmed.is_finished(), "answered by the one connect, not after the backoff");
    assert!(!confirmed.await.unwrap(), "a frame from the peer: not confirmed");
    assert_eq!(script.last_connect(), at, "the connect was made at the verdict");
}

#[tokio::test(start_paused = true)]
async fn a_refused_connect_confirms_the_verdict_at_once() {
    let (sup, script) = backing_up(fast_config()).await;
    tick(1_000).await;
    into_a_long_backoff(&script).await;
    script.answer(Answer::Refuse);
    let confirmed = verdict(&sup);
    settle().await;
    assert!(confirmed.is_finished(), "confirmed by the refusal");
    assert!(confirmed.await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_connect_confirms_one_connect_bound_after_the_verdict() {
    let (sup, script) = backing_up(fast_config()).await;
    tick(1_000).await;
    script.answer(Answer::Silence);
    script.hang_up();
    // The puller's own connect is in flight, 1.5 s into its bound.
    tick(150).await;
    let in_flight = script.last_connect();
    tokio::time::sleep_until(in_flight + Duration::from_millis(1_500)).await;
    let at = Instant::now();
    let confirmed = verdict(&sup);
    tokio::time::sleep_until(in_flight + CONNECT_BOUND + Duration::from_millis(1)).await;
    assert!(!confirmed.is_finished(), "the connect in flight started afresh at the verdict");
    assert_eq!(script.last_connect(), at, "the connect that answers starts at the verdict");
    tokio::time::sleep_until(at + CONNECT_BOUND - Duration::from_millis(1)).await;
    settle().await;
    assert!(!confirmed.is_finished(), "within its bound");
    tokio::time::sleep_until(at + CONNECT_BOUND).await;
    settle().await;
    assert!(confirmed.is_finished(), "at its bound");
    assert!(confirmed.await.unwrap(), "no answer: confirmed");
}

/// The peer answers `after` the verdict, its connect waiting meanwhile.
async fn answered_after(after: Duration) -> bool {
    let (sup, script) = backing_up(fast_config()).await;
    tick(1_000).await;
    script.answer(Answer::Silence);
    script.hang_up();
    tick(300).await;
    let at = Instant::now();
    let confirmed = verdict(&sup);
    tokio::time::sleep_until(at + after).await;
    script.answer(Answer::Take);
    tokio::time::sleep(Duration::from_millis(50)).await;
    settle().await;
    assert!(confirmed.is_finished());
    confirmed.await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_peer_answering_just_inside_the_bound_refutes_the_verdict() {
    assert!(!answered_after(CONNECT_BOUND - Duration::from_millis(1)).await);
}

#[tokio::test(start_paused = true)]
async fn a_peer_answering_just_past_the_bound_confirms_the_verdict() {
    assert!(answered_after(CONNECT_BOUND + Duration::from_millis(1)).await);
}

/// The verdict lands at the very instant the puller's first backoff (its
/// floor, 100 ms) ends and its own connect succeeds, read before or after
/// the puller runs: the peer's frame refutes it either way.
async fn verdict_at_the_backoffs_end(puller_first: bool) -> bool {
    let (sup, script) = backing_up(fast_config()).await;
    tick(1_000).await;
    let cut = Instant::now();
    script.hang_up();
    tokio::time::sleep_until(cut + Duration::from_millis(100)).await;
    if puller_first {
        settle().await;
    }
    let confirmed = verdict(&sup);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(confirmed.is_finished());
    confirmed.await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_verdict_read_as_the_backoff_ends_is_refuted() {
    assert!(!verdict_at_the_backoffs_end(false).await);
}

#[tokio::test(start_paused = true)]
async fn a_verdict_read_once_the_backoffs_connect_ran_is_refuted() {
    assert!(!verdict_at_the_backoffs_end(true).await);
}

/// On a multi-thread runtime and the real clock, with a backoff floor far
/// above the wait: the verdict's connect is made at once and refutes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_a_multi_thread_runtime_the_connect_is_made_at_once() {
    let config = PullerConfig {
        backoff_init_ms: 30_000,
        backoff_max_ms: 30_000,
        connect_timeout_ms: 500,
        ..fast_config()
    };
    let (sup, script) = backing_up(config).await;
    let up = Instant::now();
    while !sup.heard_from_since(PEER, up) && up.elapsed() < CONNECT_BOUND {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(sup.heard_from_since(PEER, up), "the Backup flow streams");
    script.hang_up();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let at = Instant::now();
    let confirmed = tokio::time::timeout(Duration::from_millis(900), verdict(&sup)).await;
    let confirmed = confirmed.expect("answered well before the 30 s backoff").unwrap();
    assert!(!confirmed, "a frame from the peer: not confirmed");
    assert!(script.last_connect() >= at, "the connect was made after the verdict");
}

/// The stream is open at the verdict and cut before any frame since: its end
/// leads to the same one connect, which the peer takes (`true`) or refuses.
async fn cut_after_the_verdict(peer_takes: bool) -> bool {
    let (sup, script) = backing_up(fast_config()).await;
    tick(1_000).await;
    let confirmed = verdict(&sup);
    settle().await;
    if !peer_takes {
        script.answer(Answer::Refuse);
    }
    script.hang_up();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(confirmed.is_finished(), "answered by the one connect, not after the backoff");
    confirmed.await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn an_open_stream_cut_after_the_verdict_and_reconnected_refutes_it() {
    assert!(!cut_after_the_verdict(true).await);
}

#[tokio::test(start_paused = true)]
async fn an_open_stream_cut_after_the_verdict_and_refused_confirms_it() {
    assert!(cut_after_the_verdict(false).await);
}

/// The connect a verdict asks for leaves the backoff where it stood: once it
/// is refused, the Backup flow's puller connects again on its own schedule,
/// at the same instant as the Reclaim flow's, which nothing asked.
#[tokio::test(start_paused = true)]
async fn a_refused_connect_asked_for_leaves_the_backoff_schedule_as_it_stood() {
    let (sup, script) = backing_up(fast_config()).await;
    tick(1_000).await;
    script.answer(Answer::Refuse);
    let cut = Instant::now();
    script.hang_up();
    settle().await;
    let confirmed = verdict(&sup);
    settle().await;
    assert!(confirmed.await.unwrap(), "refused: confirmed");
    tokio::time::sleep_until(cut + Duration::from_millis(150)).await;
    let connects = script.connects.lock().unwrap().clone();
    let at = |t: Instant| connects.iter().filter(|c| **c == t).count();
    assert_eq!(at(cut), 1, "the asked connect, at the verdict");
    assert_eq!(at(cut + Duration::from_millis(100)), 2, "both flows at their first backoff step");
}
