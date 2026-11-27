//! [`SimConnection`] — one end of a simulated connection: it sends on its
//! outbound wire and receives from its inbound one.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{DirState, SimShared, Staged, WireView};
use crate::{decode_frame, Frame, ReplicationConnection, SendError};

pub(super) struct SimConnection {
    pub(super) local: SocketAddr,
    pub(super) peer: SocketAddr,
    /// The fabric, for the owner attribution a `PullRequest` carries.
    pub(super) shared: Arc<SimShared>,
    /// Outbound staging: `send` pushes encoded bytes (ordered).
    pub(super) out: mpsc::UnboundedSender<Staged>,
    /// Outbound direction state — `send` fails fast once it is cut.
    pub(super) out_dir: Arc<DirState>,
    /// The outbound wire's owner pair — `send` fails fast once it is cut.
    pub(super) out_view: Arc<WireView>,
    /// Inbound decoded-frame source.
    pub(super) inbound: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    /// Inbound direction state — `recv` releases its flow-control window slot
    /// (waking a `block`ed writer) on this direction.
    pub(super) in_dir: Arc<DirState>,
    /// The inbound wire's owner pair, taught this end's owner with `out_view`.
    pub(super) in_view: Arc<WireView>,
}

#[async_trait]
impl ReplicationConnection for SimConnection {
    async fn send(&self, frame: Frame) -> Result<(), SendError> {
        // A puller names itself on its opening `PullRequest`: that is what ties
        // this stream's ephemeral local to the node that owns it — on the
        // fabric's map and on both of the stream's wires, before either actor
        // judges a frame on it. Only a client end is attributed: a listener's
        // address is its own owner.
        if let Frame::PullRequest { caller, .. } = &frame {
            if !self.shared.is_listener(self.local) {
                if let Some(owner) = self.shared.attribute(self.local, caller) {
                    self.out_view.learn(self.local, owner);
                    self.in_view.learn(self.local, owner);
                }
            }
        }
        // A simulated network error wins over a clean cut; a cut on the wire
        // and a cut on the owner pair fail the send alike.
        if self.out_dir.errored.load(Ordering::SeqCst) {
            return Err(SendError::Io("simulated network error".into()));
        }
        if self.out_dir.cut.load(Ordering::SeqCst) || self.out_view.node_fault().cut {
            return Err(SendError::Closed);
        }

        // Flow-control black-hole ([`Fault::Block`]): when the peer is not pulling
        // and the in-flight window is full, the write BLOCKS on buffer space —
        // modelling a TCP sender stuck on a full socket buffer with a dead reader.
        // It unblocks when the peer drains (recv releases a slot), the block is
        // cleared (Resume), or the direction is cut/errored. Arming the `Notified`
        // future before the window check avoids a lost wakeup vs `recv`'s notify.
        while self.out_dir.block.load(Ordering::SeqCst) {
            if self.out_dir.errored.load(Ordering::SeqCst) {
                return Err(SendError::Io("simulated network error".into()));
            }
            if self.out_dir.cut.load(Ordering::SeqCst) {
                return Err(SendError::Closed);
            }
            let notified = self.out_dir.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.out_dir.inflight.load(Ordering::SeqCst) < self.out_dir.cap {
                break;
            }
            notified.await;
        }

        // The codec plays through every path: encode to bytes, move bytes.
        let bytes = repl_net::transport::payload(&frame)?;
        let left_at = match &self.out_dir.link {
            Some(link) => link.queue(bytes.len()).await,
            None => tokio::time::Instant::now(),
        };
        // Account the frame against the flow-control window (released by the
        // peer's `recv`). Always tracked so `block` can be armed mid-stream.
        self.out_dir.inflight.fetch_add(1, Ordering::SeqCst);
        self.out.send((left_at, bytes)).map_err(|_| SendError::Closed)
    }

    async fn recv(&self) -> Option<Frame> {
        let bytes = self.inbound.lock().await.recv().await?;
        // Release one flow-control window slot for the sender + wake a blocked
        // writer (`in_dir` == the sender's `out_dir` for this direction).
        if self.in_dir.inflight.load(Ordering::SeqCst) > 0 {
            self.in_dir.inflight.fetch_sub(1, Ordering::SeqCst);
        }
        self.in_dir.wake.notify_waiters();
        // Decode back from bytes — sim moves encoded `Vec<u8>`, never `Frame`.
        decode_frame(&bytes).ok()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

impl Drop for SimConnection {
    fn drop(&mut self) {
        self.shared.forget_stream(self.local);
    }
}
