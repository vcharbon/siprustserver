//! [`ConnectNow`] — a request that a puller connect at once, and how that
//! connect went. A dead verdict that meets the Backup flow's stream down or
//! reconnecting asks for one (ADR-0047 D1): it skips the backoff, and a
//! connect in flight when it arrives starts afresh, so the connect that
//! answers starts at or after the request, bounded by the connect bound.

use tokio::sync::{mpsc, oneshot};

/// How the connect a [`ConnectNow`] request asked for went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    /// The peer took the connection: the stream is open.
    Connected,
    /// The connect was refused, found no address, or ran out its bound.
    Failed,
}

/// Asks a running puller for one connect at once. Cheap to clone.
#[derive(Clone, Debug)]
pub struct ConnectNow(mpsc::UnboundedSender<oneshot::Sender<Attempt>>);

/// Where a puller reads the requests a [`ConnectNow`] sends.
pub(super) type AskRx = mpsc::UnboundedReceiver<oneshot::Sender<Attempt>>;

impl ConnectNow {
    /// Ask for one connect now. The receiver resolves to how it went, or to an
    /// error when the puller ended first, cancelled or replaced. Dropping the
    /// receiver withdraws the request.
    pub fn ask(&self) -> oneshot::Receiver<Attempt> {
        let (reply, answer) = oneshot::channel();
        let _ = self.0.send(reply);
        answer
    }
}

/// The puller's side: the requests not answered yet.
#[derive(Default)]
pub(super) struct Asks {
    rx: Option<AskRx>,
    waiting: Vec<oneshot::Sender<Attempt>>,
    /// The last connect failed with a request waiting on it.
    failed: bool,
}

impl Asks {
    /// A request channel: the sender for the supervisor, the receiver for the
    /// puller.
    pub(super) fn channel() -> (ConnectNow, AskRx) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ConnectNow(tx), rx)
    }

    pub(super) fn new(rx: Option<AskRx>) -> Self {
        Self { rx, waiting: Vec::new(), failed: false }
    }

    /// Resolves once a request still asked for arrives, held for the next
    /// answer; never with no sender left. Cancel-safe: a request received is
    /// held before this returns, with no await between.
    pub(super) async fn next(&mut self) {
        loop {
            let Some(rx) = self.rx.as_mut() else {
                return std::future::pending().await;
            };
            match rx.recv().await {
                Some(reply) if !reply.is_closed() => {
                    self.waiting.push(reply);
                    return;
                }
                Some(_) => {}
                None => self.rx = None,
            }
        }
    }

    /// Hold every request already queued, so the connect starting now answers
    /// them.
    pub(super) fn take_queued(&mut self) {
        if let Some(rx) = self.rx.as_mut() {
            while let Ok(reply) = rx.try_recv() {
                self.waiting.push(reply);
            }
        }
    }

    /// Whether a request still asked for waits on the next connect.
    pub(super) fn pending(&mut self) -> bool {
        self.take_queued();
        self.waiting.retain(|reply| !reply.is_closed());
        !self.waiting.is_empty()
    }

    /// Whether the connect that just ended failed for a request, clearing it.
    pub(super) fn take_failed(&mut self) -> bool {
        std::mem::take(&mut self.failed)
    }

    /// Answer every request held with how the connect went.
    pub(super) fn answer(&mut self, attempt: Attempt) {
        self.failed = attempt == Attempt::Failed && !self.waiting.is_empty();
        for reply in std::mem::take(&mut self.waiting) {
            let _ = reply.send(attempt);
        }
    }
}

impl super::Puller {
    /// The handle that asks this puller for one connect at once; a later call
    /// replaces the earlier handle's channel.
    pub fn connect_now(&mut self) -> ConnectNow {
        let (handle, rx) = Asks::channel();
        self.asks = Some(rx);
        handle
    }
}
