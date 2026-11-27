//! The bootstrap of a cold flow: the flow's keyset snapshotted, each live
//! body streamed as a `Put` stamped at the head read before the snapshot, so
//! the tail resumes warm with nothing skipped (ADR-0014 §8).

use repl_net::frame::{Frame, Partition, Watermark};
use repl_net::transport::ReplicationConnection;

use super::{ReplServer, Unsent};
use crate::store::PartitionRole;

/// The most bodies one bootstrap `send_batch` carries.
const BOOTSTRAP_CHUNK: usize = 128;

/// The most body bytes one bootstrap `send_batch` carries; a body past it is
/// a batch of its own. Each batch is one send under the server's send stall,
/// so a batch leaves within it on any link that carries this much, or one
/// record, per stall (ADR-0014 §8).
const BOOTSTRAP_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// The scan's next batch: its keys in scan order, their frames, and the
/// bytes of their bodies.
#[derive(Default)]
struct Batch<'k> {
    keys: Vec<&'k str>,
    frames: Vec<Frame>,
    bytes: usize,
}

impl<'k> Batch<'k> {
    /// Whether a body of `len` bytes joins this batch: an empty batch takes
    /// any body, a batch below both bounds one that keeps it within them.
    fn admits(&self, len: usize) -> bool {
        self.frames.is_empty()
            || (self.frames.len() < BOOTSTRAP_CHUNK
                && self.bytes.saturating_add(len) <= BOOTSTRAP_BATCH_BYTES)
    }

    fn push(&mut self, key: &'k str, frame: Frame) {
        self.bytes = self.bytes.saturating_add(body_len(&frame));
        self.keys.push(key);
        self.frames.push(frame);
    }
}

/// The bytes of the body `frame` carries.
fn body_len(frame: &Frame) -> usize {
    match frame {
        Frame::Data { body: Some(body), .. } => body.len(),
        _ => 0,
    }
}

impl ReplServer {
    /// Bulk store-scan bootstrap for one flow. Snapshots the flow's keyset under
    /// brief locks, captures `W = head` at scan start, streams each live body as
    /// `Data{ op: Put, partition, at: W }` in scan order, batched by
    /// [`BOOTSTRAP_CHUNK`] bodies and [`BOOTSTRAP_BATCH_BYTES`] (one
    /// `send_batch` per batch — never holding a lock across the socket), and
    /// returns `W` so the tail resumes warm. A key whose body vanished between
    /// the snapshot and the read is skipped (the call ended), and so is a body
    /// the transport refuses ([`Self::send_or_skip`]). `Err` if the socket
    /// dropped or a send stalled mid-pass.
    pub(super) async fn serve_bootstrap(
        &self,
        conn: &dyn ReplicationConnection,
        caller: &str,
        partition: Partition,
        role: PartitionRole,
        primary: &str,
        flow: &'static str,
    ) -> Result<Watermark, Unsent> {
        // W = head at scan-START (ADR-0014 §8) — read BEFORE the key snapshot.
        // The writer order is body/meta-insert → changelog bump, so a call that
        // lands after this read necessarily bumps a counter > W and the tail
        // drains it; at worst the scan ALSO carries it (harmless double-deliver,
        // the apply side is idempotent). Reading head AFTER the scan opened the
        // reverse window — a call inserted between the scan snapshot and the
        // head read was in neither the scan nor the tail (`Bound::Excluded(W)`),
        // and if it then quiesced the backup never held it.
        let w = self.changelog.head();
        self.changelog.note_scanned(caller, partition, w);
        let keys = match partition {
            // Reclaim: the caller's own calls we hold as backup.
            Partition::Pri => self.source.scan_refs(PartitionRole::Backup, caller),
            // Backup: our own calls the caller backs up (Option B).
            Partition::Bak => self.source.scan_refs_backed_by(&self.self_ordinal, caller),
        };

        let scan = Scan { conn, caller, partition, role, primary, flow, w };
        let mut batch = Batch::default();
        for key in &keys {
            let Some(frame) = self.bootstrap_frame(key, &scan).await else { continue };
            if !batch.admits(body_len(&frame)) {
                self.send_bootstrap_batch(std::mem::take(&mut batch), &scan).await?;
            }
            batch.push(key, frame);
        }
        self.send_bootstrap_batch(batch, &scan).await?;
        Ok(w)
    }

    /// Send `batch` in one `send_batch`. A refused frame refuses its whole
    /// batch unsent: the batch's keys are read again and sent frame by frame,
    /// the refused one skipped.
    async fn send_bootstrap_batch(&self, batch: Batch<'_>, scan: &Scan<'_>) -> Result<(), Unsent> {
        if batch.frames.is_empty() {
            return Ok(());
        }
        match self.bounded(scan.conn.send_batch(batch.frames), scan.caller, scan.flow).await {
            Err(Unsent::Refused(_)) => {
                for key in batch.keys {
                    if let Some(frame) = self.bootstrap_frame(key, scan).await {
                        self.send_or_skip(scan.conn, frame, scan.caller, scan.flow).await?;
                    }
                }
                Ok(())
            }
            sent => sent,
        }
    }

    /// The bootstrap frame of `key`, a `Put` at the scan's `W`, stamped at
    /// send time in the frame of the body's instants as `drain_since` stamps
    /// it; `None` when its body vanished.
    async fn bootstrap_frame(&self, key: &str, scan: &Scan<'_>) -> Option<Frame> {
        let (body, meta) = self.source.read_entry(scan.role, scan.primary, key).await?;
        Some(Frame::Data {
            at: scan.w,
            op: repl_net::frame::Op::Put,
            partition: scan.partition,
            call_ref: key.to_string(),
            call_gen: meta.call_gen,
            call_bgen: meta.call_bgen,
            body_ttl_ms: meta.body_ttl_ms,
            origin_now_ms: meta.origin_now_ms(self.changelog.now_ms()),
            indexes: meta.indexes,
            body: Some(body),
            answered: false,
            incarnation: meta.incarnation,
        })
    }
}

/// One bootstrap pass: the stream it is sent on, the flow it serves and the
/// `W` its frames carry.
struct Scan<'a> {
    conn: &'a dyn ReplicationConnection,
    caller: &'a str,
    partition: Partition,
    role: PartitionRole,
    primary: &'a str,
    flow: &'static str,
    w: Watermark,
}

#[cfg(test)]
mod tests;
