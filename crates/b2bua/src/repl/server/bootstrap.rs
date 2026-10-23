//! The bootstrap of a cold flow: the flow's keyset snapshotted, each live
//! body streamed as a `Put` stamped at the head read before the snapshot, so
//! the tail resumes warm with nothing skipped (ADR-0014 §8).

use repl_net::frame::{Frame, Partition, Watermark};
use repl_net::transport::ReplicationConnection;

use super::{ReplServer, Unsent};
use crate::store::PartitionRole;

/// Bootstrap store-scan batch size (bodies per `send_batch`).
const BOOTSTRAP_CHUNK: usize = 128;

impl ReplServer {
    /// Bulk store-scan bootstrap for one flow. Snapshots the flow's keyset under
    /// brief locks, captures `W = head` at scan start, streams each live body as
    /// `Data{ op: Put, partition, at: W }` in [`BOOTSTRAP_CHUNK`] batches (one
    /// `send_batch` per chunk — never holding a lock across the socket), and
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
        let keys = match partition {
            // Reclaim: the caller's own calls we hold as backup.
            Partition::Pri => self.source.scan_refs(PartitionRole::Backup, caller),
            // Backup: our own calls the caller backs up (Option B).
            Partition::Bak => self.source.scan_refs_backed_by(&self.self_ordinal, caller),
        };

        for group in keys.chunks(BOOTSTRAP_CHUNK) {
            let frames = self.bootstrap_frames(group, w, partition, role, primary).await;
            match self.bounded(conn.send_batch(frames), caller, flow).await {
                // A refused frame refuses its whole batch unsent: the chunk is
                // read again and sent frame by frame, the refused one skipped.
                Err(Unsent::Refused(_)) => {
                    for frame in self.bootstrap_frames(group, w, partition, role, primary).await {
                        self.send_or_skip(conn, frame, caller, flow).await?;
                    }
                }
                sent => sent?,
            }
        }
        Ok(w)
    }

    /// The bootstrap frames of the keys `group`, each a `Put` at `w`; a key
    /// whose body vanished is left out.
    async fn bootstrap_frames(
        &self,
        group: &[String],
        w: Watermark,
        partition: Partition,
        role: PartitionRole,
        primary: &str,
    ) -> Vec<Frame> {
        let mut frames = Vec::with_capacity(group.len());
        for key in group {
            match self.source.read_entry(role, primary, key).await {
                Some((body, meta)) => frames.push(Frame::Data {
                    at: w,
                    op: repl_net::frame::Op::Put,
                    partition,
                    call_ref: key.clone(),
                    call_gen: meta.call_gen,
                    call_bgen: meta.call_bgen,
                    body_ttl_ms: meta.body_ttl_ms,
                    // Send-time stamp in the frame of the body's instants, as
                    // `drain_since` stamps it.
                    origin_now_ms: meta.origin_now_ms(self.changelog.now_ms()),
                    indexes: meta.indexes,
                    body: Some(body),
                    answered: false,
                    incarnation: meta.incarnation,
                }),
                _ => continue,
            }
        }
        frames
    }
}
