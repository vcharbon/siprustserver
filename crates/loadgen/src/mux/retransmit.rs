//! Per-call SIP retransmit engine (present only when `--auto-retransmit` is
//! on): records what the harness sends, retransmits it on real timers until
//! acknowledged, and absorbs the duplicate traffic recovery produces so the
//! strict scripted agent never sees a retransmit.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use sip_net::{ReEmitKind, SendTap, UdpEndpoint};

use super::datagram::{key_of, Datagram};
use super::loss::DropModel;
use super::stats::MuxStats;
use super::taps::Taps;
use resender::{spawn_resender, ResendCtl, Sent};

mod resender;

/// RFC 3261 default transaction timers driving the engine.
const T1: Duration = Duration::from_millis(500); // first retransmit interval
const T2: Duration = Duration::from_secs(4); // non-INVITE / 2xx backoff cap
const TXN_TIMEOUT: Duration = Duration::from_secs(32); // Timer B/F/H = 64·T1
/// How long a released Call-ID, or a key whose call ended not ok, is held
/// back: 64·T1, the longest a peer keeps retransmitting an INVITE (Timer B).
pub const RELEASE_HOLD: Duration = TXN_TIMEOUT;

/// Per-call SIP transaction engine.
///
/// METHOD-GENERIC: requests are keyed by their top-Via branch and classified
/// only INVITE vs non-INVITE (Timer A vs Timer E backoff), so PRACK / UPDATE /
/// INFO / any future method ride the same client-txn, reactive-re-answer and
/// duplicate-absorption paths with no per-method code.
///
/// Covered directions (the "full bidirectional" robustness contract):
/// - **our requests**: retransmitted (Timer A/E) until the matching response
///   arrives.
/// - **our INVITE answers** (2xx): retransmitted (Timer G) until the ACK
///   arrives — recovers a lost 2xx OR a lost ACK.
/// - **our RELIABLE provisionals** (1xx with `Require: 100rel` + `RSeq`,
///   RFC 3262 §3): retransmitted until the matching PRACK — unlike the
///   best-effort plain 18x (deliberately NOT retransmitted; the driver gates
///   its delivery rate instead).
/// - **our non-INVITE answers**: re-sent reactively when the peer retransmits
///   the request (its response was lost).
/// - **our ACK to a 2xx**: re-sent when a retransmitted 2xx arrives.
/// - **inbound duplicates**: absorbed, so the scripted agent's strict `expect`
///   never chokes on a retransmit.
pub(super) struct CallTxns {
    endpoint: Arc<dyn UdpEndpoint>,
    drop: Arc<DropModel>,
    stats: Arc<MuxStats>,
    inner: Mutex<TxnInner>,
    /// Send-time recording tap for re-emissions (installed by the recording
    /// decorator on sampled calls; `None` on the non-recording path). Shared as
    /// an `Arc<OnceLock>` so the detached resender tasks can read it at fire
    /// time — recovery frames go on the wire below the recording layer, so this
    /// is the only way they reach the ladder.
    sendtap: Arc<OnceLock<SendTap>>,
    /// The endpoint's taps, told of every re-send.
    taps: Option<Arc<Taps>>,
}

/// A transaction's key: its top-Via branch and its (CSeq) method, hashed. A
/// CANCEL shares its INVITE's branch (RFC 3261 §9.1) and is its own
/// transaction.
fn txn_key(branch: &str, method: &str) -> u64 {
    key_of((branch, method))
}

/// A dialog-scoped key: the Call-ID, hashed, and a sequence number.
fn cid_key<N>(call_id: &str, n: N) -> (u64, N) {
    (key_of(call_id), n)
}

/// A proactive INVITE-final resender, and the key of the final it resends in
/// [`TxnInner::server`].
type FinalResend = (Arc<ResendCtl>, u64);

#[derive(Default)]
struct TxnInner {
    /// Our in-flight client transactions, by [`txn_key`] → its resender
    /// (stopped when the response arrives).
    client: HashMap<u64, Arc<ResendCtl>>,
    /// Our proactive 2xx (INVITE server txn) resenders, keyed by (Call-ID, CSeq
    /// number) so the inbound ACK — which carries a *different* branch — can stop them.
    invite_2xx: HashMap<(u64, u32), FinalResend>,
    /// Our proactive NON-2xx INVITE-final resenders (RFC 3261 §17.2.1: a real UAS
    /// retransmits its non-2xx final via Timer G until the ACK arrives, giving up
    /// at Timer H = 64·T1), keyed by the INVITE's [`txn_key`]: the hop-ACK is on
    /// the INVITE's own branch (§17.1.1.3), so a merged request's other
    /// transactions (§8.2.2.2) are left alone. Symmetric to `invite_2xx` except
    /// NOT drained at `shutdown` — see the note there.
    invite_non2xx: HashMap<u64, Arc<ResendCtl>>,
    /// Our proactive RELIABLE-1xx resenders (RFC 3262 §3: retransmit until
    /// PRACKed), keyed by (Call-ID, RSeq) so the inbound PRACK — whose RAck
    /// response-num carries that RSeq but whose branch is its own — can stop them.
    reliable_1xx: HashMap<(u64, u64), Arc<ResendCtl>>,
    /// The last response we sent per server txn ([`txn_key`]), for a reactive
    /// re-answer when the peer retransmits the request. An INVITE's final goes
    /// with its ACK: a retransmitted INVITE is then absorbed (RFC 3261
    /// §17.2.1 Confirmed, RFC 6026 Accepted).
    server: HashMap<u64, Sent>,
    /// ACKs we sent, keyed by (Call-ID, CSeq number), for re-ACK on a duplicate 2xx.
    acks: HashMap<(u64, u32), Sent>,
    /// Inbound datagrams already delivered, by branch and discriminator,
    /// hashed — duplicate detection.
    seen_in: HashSet<u64>,
    /// Call ended: stop tracking and reject new resenders.
    closed: bool,
}

impl CallTxns {
    pub(super) fn new(
        endpoint: Arc<dyn UdpEndpoint>,
        drop: Arc<DropModel>,
        stats: Arc<MuxStats>,
        taps: Option<Arc<Taps>>,
    ) -> Self {
        Self {
            endpoint,
            drop,
            stats,
            inner: Mutex::new(TxnInner::default()),
            sendtap: Arc::new(OnceLock::new()),
            taps,
        }
    }

    /// Install the re-emission recording tap (first installer wins). Called by
    /// the recording decorator via `MuxEndpoint::install_send_tap`.
    pub(super) fn set_sendtap(&self, tap: SendTap) {
        let _ = self.sendtap.set(tap);
    }

    /// Best-effort fire-and-forget send for the reactive resends (re-ACK /
    /// re-answer) that run on the inbound (sync, lock-holding) path — re-applies
    /// the loss model. The actual transmission is a detached task (the fabric's
    /// `send_to` is async). `kind` classifies the re-emission for the ladder; a
    /// frame that passed the loss model is reported to the send tap (a dropped
    /// one is invisible, like a dropped first transmission).
    fn send(&self, sent: Sent, kind: ReEmitKind) {
        let Sent { bytes, dst, count } = sent;
        if let Some(count) = &count {
            count.record();
        }
        if self.drop.drops() {
            self.stats.dropped_out.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Some(tap) = self.sendtap.get() {
            tap(&bytes, dst, kind);
        }
        let endpoint = self.endpoint.clone();
        tokio::spawn(async move {
            let _ = endpoint.as_ref().send_to(&bytes, dst).await;
        });
    }

    /// Record an outbound datagram and arm any retransmission it needs.
    pub(super) fn on_outbound(&self, d: &Datagram<'_>, dst: SocketAddr) {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return;
        }
        let id = d.id();
        if let Some(status) = id.status {
            let Some(branch) = id.branch else { return };
            let method = id.cseq_method.unwrap_or_default();
            let server = txn_key(branch, method);
            g.server.insert(server, self.sent(d, dst));
            if method != "INVITE" {
                return;
            }
            // A final ends the INVITE's provisionals: none is retransmitted
            // after it (RFC 3262 §3, the give-up's 5xx included).
            if status >= 200 {
                if let Some(cid) = id.call_id.map(key_of) {
                    g.reliable_1xx.retain(|(c, _), ctl| {
                        let stays = *c != cid;
                        if !stays {
                            ctl.stop();
                        }
                        stays
                    });
                }
            }
            let (Some(cid), Some(cseq)) = (id.call_id, id.cseq) else { return };
            // Proactive 2xx-until-ACK for an INVITE answer (Timer G).
            if (200..300).contains(&status) {
                g.invite_2xx
                    .entry(cid_key(cid, cseq))
                    .or_insert_with(|| (self.spawn_response_resender(d, dst), server));
            }
            // Proactive non-2xx-until-ACK for an INVITE reject (Timer G, §17.2.1):
            // a real UAS retransmits its final until the hop-ACK arrives, so a lost
            // SUT hop-ACK is RECOVERED (the SUT's txn layer re-ACKs each resend,
            // §17.1.1.2) instead of stranding the reject as an unACKed final. In
            // no-loss runs the ACK arrives first and this never fires.
            if (300..700).contains(&status) {
                g.invite_non2xx
                    .entry(server)
                    .or_insert_with(|| self.spawn_response_resender(d, dst));
            }
            // Proactive reliable-1xx-until-PRACK (RFC 3262 §3): a provisional we
            // send with `Require: 100rel` + `RSeq` is guaranteed-delivery. Without
            // this, a dropped reliable 183 is unrecoverable: the peer's INVITE
            // resender already stopped on the 100 Trying, so nobody would resend
            // anything. (A plain 18x stays best-effort by design.)
            if (101..200).contains(&status) && id.reliable {
                if let Some(rseq) = id.rseq {
                    g.reliable_1xx
                        .entry(cid_key(cid, rseq))
                        .or_insert_with(|| self.spawn_response_resender(d, dst));
                }
            }
            return;
        }
        // Request.
        let method = id.method.unwrap_or_default();
        if method == "ACK" {
            // ACK is not a retransmitting transaction; remember it for re-ACK.
            if let (Some(cid), Some(cseq)) = (id.call_id, id.cseq) {
                g.acks.insert(cid_key(cid, cseq), self.sent(d, dst));
            }
            return;
        }
        let Some(branch) = id.branch else { return };
        let invite = method == "INVITE";
        let ctl = ResendCtl::new();
        if let Some(old) = g.client.insert(txn_key(branch, method), ctl.clone()) {
            old.stop();
        }
        spawn_resender(
            ctl,
            self.endpoint.clone(),
            self.drop.clone(),
            self.stats.clone(),
            self.sent(d, dst),
            invite,
            self.sendtap.clone(),
        );
    }

    /// Spawn a proactive resender (non-INVITE backoff: Timer G/E shape) for one
    /// of our responses and return its stop-control for the caller's table.
    fn spawn_response_resender(&self, d: &Datagram<'_>, dst: SocketAddr) -> Arc<ResendCtl> {
        let ctl = ResendCtl::new();
        spawn_resender(
            ctl.clone(),
            self.endpoint.clone(),
            self.drop.clone(),
            self.stats.clone(),
            self.sent(d, dst),
            false,
            self.sendtap.clone(),
        );
        ctl
    }

    /// `d`, sent to `dst`, as a store keeps it for a re-send.
    fn sent(&self, d: &Datagram<'_>, dst: SocketAddr) -> Sent {
        let count = self.taps.as_ref().and_then(|t| t.resend_count(d.id()));
        Sent { bytes: d.shared(), dst, count }
    }

    /// Process an inbound datagram. Returns `true` to deliver it to the app,
    /// `false` to ABSORB it (a duplicate the strict agent must not see — any
    /// re-ACK / re-answer has already been sent here).
    pub(super) fn on_inbound(&self, d: &Datagram<'_>, _src: SocketAddr) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return true;
        }
        let id = d.id();
        let branch = id.branch.unwrap_or_default();
        if let Some(status) = id.status {
            let method = id.cseq_method.unwrap_or_default();
            let is_invite = method == "INVITE";
            // Stop our client resender: INVITE Timer A stops on the FIRST provisional
            // (incl. `100 Trying`, RFC 3261 §17.1.1.2); non-INVITE only on a final.
            // We deliberately do NOT keep retransmitting the INVITE to force the UAS
            // to resend a lost 18x — a non-PRACK provisional is best-effort and may
            // be lost (the caller absorbs a variable 1xx count, and the driver gates
            // the cross-call 18x delivery rate instead).
            let stop = if is_invite { status >= 100 } else { status >= 200 };
            if stop {
                if let Some(ctl) = g.client.remove(&txn_key(branch, method)) {
                    ctl.stop();
                }
            }
            // Dedup discriminator includes the To-tag: a TRUE fork (RFC 3261
            // §12.1.2) carries a DISTINCT To-tag per 18x on ONE branch, and each
            // fork's provisional must reach the body; only a same-tag repeat is a
            // genuine retransmission (or a B2BUA collapsing forks onto one a-leg
            // tag — on the wire it IS one, which makes byte-identical "ring again"
            // unobservable from a load body; see the shape-authoring caveat in
            // `crate::scenarios`).
            let key = key_of((branch, "r", status, id.to_tag.unwrap_or_default()));
            if !g.seen_in.insert(key) {
                // Duplicate response. A retransmitted INVITE 2xx means our ACK was
                // lost → re-ACK it.
                if (200..300).contains(&status) && is_invite {
                    if let (Some(cid), Some(cseq)) = (id.call_id, id.cseq) {
                        if let Some(ack) = g.acks.get(&cid_key(cid, cseq)).cloned() {
                            self.send(ack, ReEmitKind::ReAck);
                        }
                    }
                }
                return false;
            }
            return true;
        }
        // Inbound request.
        let method = id.method.unwrap_or_default();
        if method == "ACK" {
            // The ACK confirms our INVITE final → stop its proactive resender
            // (2xx §13.3.1 or non-2xx §17.2.1); the final is no longer owed to a
            // retransmitted INVITE. A non-2xx's ACK is hop-by-hop, on the branch
            // of an INVITE we received (§17.1.1.3); a 2xx's ACK is a transaction
            // of its own, on a new branch, matched by Call-ID and CSeq.
            let acked = if g.seen_in.contains(&key_of((branch, "q", "INVITE"))) {
                let server = txn_key(branch, "INVITE");
                g.invite_non2xx.remove(&server).map(|ctl| (ctl, server))
            } else {
                let key = id.call_id.zip(id.cseq).map(|(cid, cseq)| cid_key(cid, cseq));
                key.and_then(|key| g.invite_2xx.remove(&key))
            };
            if let Some((ctl, server)) = acked {
                ctl.stop();
                g.server.remove(&server);
            }
            return g.seen_in.insert(key_of((branch, "q", method))); // false: duplicate ACK
        }
        if method == "PRACK" {
            // The PRACK acknowledges our reliable 1xx (RAck response-num = its
            // RSeq, RFC 3262 §7.2) → stop that proactive resender. Idempotent, so
            // it runs before the duplicate check below.
            if let (Some(cid), Some(rseq)) = (id.call_id, id.rack_rseq) {
                if let Some(ctl) = g.reliable_1xx.remove(&cid_key(cid, rseq)) {
                    ctl.stop();
                }
            }
        }
        if !g.seen_in.insert(key_of((branch, "q", method))) {
            // The peer retransmitted this request → our response was lost; re-send it.
            if let Some(resp) = g.server.get(&txn_key(branch, method)).cloned() {
                self.send(resp, ReEmitKind::ReAnswer);
            }
            return false;
        }
        true
    }

    /// Park until every client transaction open now has ended: its final
    /// arrived, or Timer B/F expired. Retransmission runs meanwhile, so a
    /// teardown CANCEL or BYE lost on the wire still reaches its peer.
    pub(super) async fn clients_ended(&self) {
        let open: Vec<Arc<ResendCtl>> =
            self.inner.lock().unwrap().client.values().cloned().collect();
        for ctl in open {
            ctl.ended().await;
        }
    }

    /// Stop every resender and drop tracked state (call ended).
    pub(super) fn shutdown(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        for (_, ctl) in g.client.drain() {
            ctl.stop();
        }
        for (_, (ctl, _)) in g.invite_2xx.drain() {
            ctl.stop();
        }
        // NOT `invite_non2xx`: a rejected leg's server txn is independent of the
        // call it belonged to (a reroute/REFER abandons the leg immediately, but a
        // real UAS keeps retransmitting its non-2xx final until ACKed, §17.2.1). We
        // leave those resenders running so a lost hop-ACK is still recovered; each
        // self-terminates on its inbound ACK or its own `TXN_TIMEOUT` (Timer H), so
        // lifetime stays bounded. Dropping the map here releases only CallTxns's ctl
        // clones — the detached tasks hold their own.
        for (_, ctl) in g.reliable_1xx.drain() {
            ctl.stop();
        }
        g.server.clear();
        g.acks.clear();
        g.seen_in.clear();
    }
}

// The engine must stay method-generic (Timer E for ANY non-INVITE request,
// duplicate absorption keyed by (branch, method)) and cover the RFC 3262
// reliable-1xx-until-PRACK server obligation. These tests drive `CallTxns`
// directly over a loopback UDP pair on the real clock (T1 = 500 ms, so each
// stays a few seconds).
#[cfg(test)]
mod teardown_tests;
#[cfg(test)]
mod tests;
