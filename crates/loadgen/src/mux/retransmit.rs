//! Per-call SIP retransmit engine (present only when `--auto-retransmit` is
//! on): records what the harness sends, retransmits it on real timers until
//! acknowledged, and absorbs the duplicate traffic recovery produces so the
//! strict scripted agent never sees a retransmit.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use sip_message::sniff::{
    call_id, cseq_method_label, cseq_number, is_response, rack_rseq, req_method,
    require_has_100rel, resp_status, rseq_of, to_tag, via_branch,
};
use sip_net::{ReEmitKind, SendTap, UdpEndpoint};

use super::loss::DropModel;
use super::stats::MuxStats;

/// RFC 3261 default transaction timers driving the engine.
const T1: Duration = Duration::from_millis(500); // first retransmit interval
const T2: Duration = Duration::from_secs(4); // non-INVITE / 2xx backoff cap
const TXN_TIMEOUT: Duration = Duration::from_secs(32); // Timer B/F/H = 64·T1

/// Stop-control shared between a spawned resender task and the [`CallTxns`] engine
/// that owns it. The engine flips `stop` (and wakes the task) when the transaction
/// is acknowledged or the call ends; the flag is authoritative (the wake is only a
/// latency optimisation — a lost `Notify` wake is caught by the post-sleep check).
struct ResendCtl {
    stop: AtomicBool,
    notify: tokio::sync::Notify,
}

impl ResendCtl {
    fn new() -> Arc<Self> {
        Arc::new(Self { stop: AtomicBool::new(false), notify: tokio::sync::Notify::new() })
    }
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// Spawn a resender: retransmit `bytes` to `dst` on the SIP timer schedule until
/// `ctl` is stopped or the transaction times out. `invite` selects the backoff —
/// INVITE Timer A doubles unbounded (until Timer B); non-INVITE Timer E / 2xx
/// Timer G doubles but caps at T2. Each retransmit re-applies the loss model, so a
/// retransmit can itself be dropped (the point of the robustness test).
#[allow(clippy::too_many_arguments)]
fn spawn_resender(
    ctl: Arc<ResendCtl>,
    endpoint: Arc<dyn UdpEndpoint>,
    drop: Arc<DropModel>,
    stats: Arc<MuxStats>,
    bytes: Vec<u8>,
    dst: SocketAddr,
    invite: bool,
    sendtap: Arc<OnceLock<SendTap>>,
) {
    tokio::spawn(async move {
        let mut interval = T1;
        let deadline = tokio::time::Instant::now() + TXN_TIMEOUT;
        loop {
            tokio::select! {
                _ = ctl.notify.notified() => return,
                _ = tokio::time::sleep(interval) => {}
            }
            if ctl.stopped() || tokio::time::Instant::now() >= deadline {
                return;
            }
            if drop.drops() {
                stats.dropped_out.fetch_add(1, Ordering::Relaxed);
            } else {
                let _ = endpoint.send_to(&bytes, dst).await;
                // Record the re-emission (a frame that reached the wire) on the
                // ladder — a proactive Timer A/E/G retransmit of our own frame.
                if let Some(tap) = sendtap.get() {
                    tap(&bytes, dst, ReEmitKind::Retransmit);
                }
            }
            interval = if invite {
                interval.saturating_mul(2)
            } else {
                interval.saturating_mul(2).min(T2)
            };
        }
    });
}

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
}

#[derive(Default)]
struct TxnInner {
    /// Our in-flight client transactions, keyed by the request's top-Via branch →
    /// its resender (stopped when the response arrives).
    client: HashMap<String, Arc<ResendCtl>>,
    /// Our proactive 2xx (INVITE server txn) resenders, keyed by (Call-ID, CSeq
    /// number) so the inbound ACK — which carries a *different* branch — can stop them.
    invite_2xx: HashMap<(String, u32), Arc<ResendCtl>>,
    /// Our proactive NON-2xx INVITE-final resenders (RFC 3261 §17.2.1: a real UAS
    /// retransmits its non-2xx final via Timer G until the ACK arrives, giving up
    /// at Timer H = 64·T1), keyed by (Call-ID, CSeq number) so the inbound hop-ACK
    /// — which reuses the INVITE CSeq (§17.1.1.3) — can stop them. Symmetric to
    /// `invite_2xx` except NOT drained at `shutdown` — see the note there.
    invite_non2xx: HashMap<(String, u32), Arc<ResendCtl>>,
    /// Our proactive RELIABLE-1xx resenders (RFC 3262 §3: retransmit until
    /// PRACKed), keyed by (Call-ID, RSeq) so the inbound PRACK — whose RAck
    /// response-num carries that RSeq but whose branch is its own — can stop them.
    reliable_1xx: HashMap<(String, u64), Arc<ResendCtl>>,
    /// The last response we sent per server txn (request branch → bytes+dst), for a
    /// reactive re-answer when the peer retransmits the request.
    server: HashMap<String, (Vec<u8>, SocketAddr)>,
    /// ACKs we sent, keyed by (Call-ID, CSeq number), for re-ACK on a duplicate 2xx.
    acks: HashMap<(String, u32), (Vec<u8>, SocketAddr)>,
    /// Inbound `(branch, discriminator)` already delivered — duplicate detection.
    seen_in: HashSet<(String, String)>,
    /// Call ended: stop tracking and reject new resenders.
    closed: bool,
}

impl CallTxns {
    pub(super) fn new(endpoint: Arc<dyn UdpEndpoint>, drop: Arc<DropModel>, stats: Arc<MuxStats>) -> Self {
        Self {
            endpoint,
            drop,
            stats,
            inner: Mutex::new(TxnInner::default()),
            sendtap: Arc::new(OnceLock::new()),
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
    fn send(&self, bytes: &[u8], dst: SocketAddr, kind: ReEmitKind) {
        if self.drop.drops() {
            self.stats.dropped_out.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let endpoint = self.endpoint.clone();
        let owned = bytes.to_vec();
        tokio::spawn(async move {
            let _ = endpoint.send_to(&owned, dst).await;
        });
        if let Some(tap) = self.sendtap.get() {
            tap(bytes, dst, kind);
        }
    }

    /// Record an outbound datagram and arm any retransmission it needs.
    pub(super) fn on_outbound(&self, raw: &[u8], dst: SocketAddr) {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return;
        }
        if is_response(raw) {
            let Some(branch) = via_branch(raw) else { return };
            g.server.insert(branch, (raw.to_vec(), dst));
            let status = resp_status(raw).unwrap_or(0);
            if cseq_method_label(raw) != "INVITE" {
                return;
            }
            // Proactive 2xx-until-ACK for an INVITE answer (Timer G).
            if (200..300).contains(&status) {
                if let (Some(cid), Some(cseq)) = (call_id(raw), cseq_number(raw)) {
                    g.invite_2xx
                        .entry((cid, cseq))
                        .or_insert_with(|| self.spawn_response_resender(raw, dst));
                }
            }
            // Proactive non-2xx-until-ACK for an INVITE reject (Timer G, §17.2.1):
            // a real UAS retransmits its final until the hop-ACK arrives, so a lost
            // SUT hop-ACK is RECOVERED (the SUT's txn layer re-ACKs each resend,
            // §17.1.1.2) instead of stranding the reject as an unACKed final. In
            // no-loss runs the ACK arrives first and this never fires.
            if (300..700).contains(&status) {
                if let (Some(cid), Some(cseq)) = (call_id(raw), cseq_number(raw)) {
                    g.invite_non2xx
                        .entry((cid, cseq))
                        .or_insert_with(|| self.spawn_response_resender(raw, dst));
                }
            }
            // Proactive reliable-1xx-until-PRACK (RFC 3262 §3): a provisional we
            // send with `Require: 100rel` + `RSeq` is guaranteed-delivery. Without
            // this, a dropped reliable 183 is unrecoverable: the peer's INVITE
            // resender already stopped on the 100 Trying, so nobody would resend
            // anything. (A plain 18x stays best-effort by design.)
            if (101..200).contains(&status) && require_has_100rel(raw) {
                if let (Some(cid), Some(rseq)) = (call_id(raw), rseq_of(raw)) {
                    g.reliable_1xx
                        .entry((cid, rseq))
                        .or_insert_with(|| self.spawn_response_resender(raw, dst));
                }
            }
            return;
        }
        // Request.
        let method = req_method(raw).unwrap_or_default();
        if method == "ACK" {
            // ACK is not a retransmitting transaction; remember it for re-ACK.
            if let (Some(cid), Some(cseq)) = (call_id(raw), cseq_number(raw)) {
                g.acks.insert((cid, cseq), (raw.to_vec(), dst));
            }
            return;
        }
        let Some(branch) = via_branch(raw) else { return };
        if let Some(old) = g.client.remove(&branch) {
            old.stop();
        }
        let invite = method == "INVITE";
        let ctl = ResendCtl::new();
        g.client.insert(branch, ctl.clone());
        spawn_resender(
            ctl,
            self.endpoint.clone(),
            self.drop.clone(),
            self.stats.clone(),
            raw.to_vec(),
            dst,
            invite,
            self.sendtap.clone(),
        );
    }

    /// Spawn a proactive resender (non-INVITE backoff: Timer G/E shape) for one
    /// of our responses and return its stop-control for the caller's table.
    fn spawn_response_resender(&self, raw: &[u8], dst: SocketAddr) -> Arc<ResendCtl> {
        let ctl = ResendCtl::new();
        spawn_resender(
            ctl.clone(),
            self.endpoint.clone(),
            self.drop.clone(),
            self.stats.clone(),
            raw.to_vec(),
            dst,
            false,
            self.sendtap.clone(),
        );
        ctl
    }

    /// Process an inbound datagram. Returns `true` to deliver it to the app,
    /// `false` to ABSORB it (a duplicate the strict agent must not see — any
    /// re-ACK / re-answer has already been sent here).
    pub(super) fn on_inbound(&self, raw: &[u8], _src: SocketAddr) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return true;
        }
        let branch = via_branch(raw).unwrap_or_default();
        if is_response(raw) {
            let status = resp_status(raw).unwrap_or(0);
            let is_invite = cseq_method_label(raw) == "INVITE";
            // Stop our client resender: INVITE Timer A stops on the FIRST provisional
            // (incl. `100 Trying`, RFC 3261 §17.1.1.2); non-INVITE only on a final.
            // We deliberately do NOT keep retransmitting the INVITE to force the UAS
            // to resend a lost 18x — a non-PRACK provisional is best-effort and may
            // be lost (the caller absorbs a variable 1xx count, and the driver gates
            // the cross-call 18x delivery rate instead).
            let stop = if is_invite { status >= 100 } else { status >= 200 };
            if stop {
                if let Some(ctl) = g.client.remove(&branch) {
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
            let key = (branch, format!("r{status}:{}", to_tag(raw)));
            if g.seen_in.contains(&key) {
                // Duplicate response. A retransmitted INVITE 2xx means our ACK was
                // lost → re-ACK it.
                if (200..300).contains(&status) && is_invite {
                    if let (Some(cid), Some(cseq)) = (call_id(raw), cseq_number(raw)) {
                        if let Some((ack, dst)) = g.acks.get(&(cid, cseq)).cloned() {
                            self.send(&ack, dst, ReEmitKind::ReAck);
                        }
                    }
                }
                return false;
            }
            g.seen_in.insert(key);
            return true;
        }
        // Inbound request.
        let method = req_method(raw).unwrap_or_default();
        if method == "ACK" {
            // The ACK confirms our INVITE final → stop its proactive resender
            // (2xx §13.3.1 or non-2xx §17.2.1; both keyed by the INVITE CSeq, which
            // the hop-ACK reuses per §17.1.1.3).
            if let (Some(cid), Some(cseq)) = (call_id(raw), cseq_number(raw)) {
                let key = (cid, cseq);
                if let Some(ctl) = g.invite_2xx.remove(&key) {
                    ctl.stop();
                }
                if let Some(ctl) = g.invite_non2xx.remove(&key) {
                    ctl.stop();
                }
            }
            let key = (branch, "qACK".to_string());
            if g.seen_in.contains(&key) {
                return false; // duplicate ACK
            }
            g.seen_in.insert(key);
            return true;
        }
        if method == "PRACK" {
            // The PRACK acknowledges our reliable 1xx (RAck response-num = its
            // RSeq, RFC 3262 §7.2) → stop that proactive resender. Idempotent, so
            // it runs before the duplicate check below.
            if let (Some(cid), Some(rseq)) = (call_id(raw), rack_rseq(raw)) {
                if let Some(ctl) = g.reliable_1xx.remove(&(cid, rseq)) {
                    ctl.stop();
                }
            }
        }
        let key = (branch.clone(), format!("q{method}"));
        if g.seen_in.contains(&key) {
            // The peer retransmitted this request → our response was lost; re-send it.
            if let Some((resp, dst)) = g.server.get(&branch).cloned() {
                self.send(&resp, dst, ReEmitKind::ReAnswer);
            }
            return false;
        }
        g.seen_in.insert(key);
        true
    }

    /// Stop every resender and drop tracked state (call ended).
    pub(super) fn shutdown(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        for (_, ctl) in g.client.drain() {
            ctl.stop();
        }
        for (_, ctl) in g.invite_2xx.drain() {
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
mod tests {
    use super::*;
    use sip_net::{BindUdpOpts, RealSignalingNetwork, SignalingNetwork};
    use std::net::SocketAddr;
    use tokio::net::UdpSocket as TokioUdp;
    use tokio::time::{timeout, Duration as TokioDuration};

    async fn txn_rig() -> (Arc<CallTxns>, TokioUdp, SocketAddr) {
        // The engine sends through the same fabric seam the mux binds on.
        let endpoint: Arc<dyn UdpEndpoint> = Arc::from(
            RealSignalingNetwork::new()
                .bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 64))
                .await
                .unwrap(),
        );
        let peer = TokioUdp::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let txns = Arc::new(CallTxns::new(
            endpoint,
            Arc::new(DropModel::new(0.0, 1, None)),
            Arc::new(MuxStats::new(4)),
        ));
        (txns, peer, peer_addr)
    }

    async fn recv_one(peer: &TokioUdp, window_ms: u64) -> Option<String> {
        let mut buf = vec![0u8; 2048];
        match timeout(TokioDuration::from_millis(window_ms), peer.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(String::from_utf8_lossy(&buf[..n]).to_string()),
            _ => None,
        }
    }

    fn update_req(branch: &str) -> Vec<u8> {
        format!(
            "UPDATE sip:b@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: 2 UPDATE\r\n\r\n"
        )
        .into_bytes()
    }

    fn resp(status: u16, branch: &str, cseq: &str, extra: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: {cseq}\r\n{extra}\r\n"
        )
        .into_bytes()
    }

    fn prack_req(branch: &str, rack: &str) -> Vec<u8> {
        format!(
            "PRACK sip:b@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5061;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n\r\n"
        )
        .into_bytes()
    }

    /// Timer E is METHOD-GENERIC: an outbound UPDATE (a non-INVITE the engine has
    /// no per-method code for) is retransmitted after ~T1 and the resender stops
    /// on its final response; a duplicate of that response is then absorbed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calltxns_timer_e_is_method_generic_for_update() {
        let (txns, peer, peer_addr) = txn_rig().await;

        txns.on_outbound(&update_req("z9hG4bK-u1"), peer_addr);
        let got = recv_one(&peer, 2_000).await.expect("Timer E retransmit of the UPDATE");
        assert!(got.starts_with("UPDATE "), "retransmit is the UPDATE: {got}");

        // The 200 (UPDATE) stops the resender…
        let ok = resp(200, "z9hG4bK-u1", "2 UPDATE", "");
        assert!(txns.on_inbound(&ok, peer_addr), "first 200 (UPDATE) is delivered");
        // …and its duplicate is absorbed (method-generic dedup).
        assert!(!txns.on_inbound(&ok, peer_addr), "duplicate 200 (UPDATE) absorbed");
        assert!(
            recv_one(&peer, 1_500).await.is_none(),
            "UPDATE resender must stop on the final response"
        );
        txns.shutdown();
    }

    /// Duplicate absorption + reactive re-answer are method-generic: a
    /// retransmitted inbound PRACK is absorbed and our recorded 200 (PRACK) is
    /// re-sent (the peer's copy was evidently lost).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calltxns_absorbs_duplicate_prack_and_reanswers() {
        let (txns, peer, peer_addr) = txn_rig().await;

        let prack = prack_req("z9hG4bK-p1", "1 1 INVITE");
        assert!(txns.on_inbound(&prack, peer_addr), "first PRACK is delivered");
        txns.on_outbound(&resp(200, "z9hG4bK-p1", "2 PRACK", ""), peer_addr);

        assert!(!txns.on_inbound(&prack, peer_addr), "duplicate PRACK absorbed");
        let got = recv_one(&peer, 1_000).await.expect("reactive re-answer to the dup PRACK");
        assert!(got.starts_with("SIP/2.0 200"), "re-answer is our 200 (PRACK): {got}");
        txns.shutdown();
    }

    /// RFC 3262 §3: a RELIABLE provisional we send (Require:100rel + RSeq) is
    /// retransmitted until the matching PRACK (RAck response-num = its RSeq)
    /// arrives — the gap that made a dropped reliable 183 unrecoverable (the
    /// peer's INVITE resender already stopped on the 100 Trying). A plain 18x
    /// stays best-effort (never proactively retransmitted).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calltxns_retransmits_reliable_1xx_until_prack() {
        let (txns, peer, peer_addr) = txn_rig().await;

        // A plain 180 arms NO resender (best-effort by design).
        txns.on_outbound(&resp(180, "z9hG4bK-i1", "1 INVITE", ""), peer_addr);
        assert!(
            recv_one(&peer, 1_200).await.is_none(),
            "a non-reliable 18x must not be proactively retransmitted"
        );

        // A reliable 183 IS retransmitted until its PRACK.
        let r183 = resp(183, "z9hG4bK-i1", "1 INVITE", "Require: 100rel\r\nRSeq: 1\r\n");
        txns.on_outbound(&r183, peer_addr);
        let got = recv_one(&peer, 2_000).await.expect("reliable 183 retransmit");
        assert!(got.starts_with("SIP/2.0 183"), "retransmit is the reliable 183: {got}");

        // The matching PRACK stops it (and is delivered to the app).
        assert!(txns.on_inbound(&prack_req("z9hG4bK-p2", "1 1 INVITE"), peer_addr));
        assert!(
            recv_one(&peer, 1_500).await.is_none(),
            "reliable-183 resender must stop on the matching PRACK"
        );
        txns.shutdown();
    }

    /// A response with an explicit To-tag (a forking UAS emits several 18x with
    /// DISTINCT To-tags on one INVITE branch, RFC 3261 §12.1.2).
    fn resp_tag(status: u16, branch: &str, cseq: &str, to_tag: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag={to_tag}\r\nCSeq: {cseq}\r\n\r\n"
        )
        .into_bytes()
    }

    /// TRUE FORKING — two 18x with DISTINCT To-tags on ONE INVITE branch must
    /// BOTH be delivered to the body (each is a separate early dialog, §12.1.2),
    /// never collapsed as a retransmit. A same-tag repeat still dedups. The
    /// discriminator is `(branch, status, To-tag)`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calltxns_distinct_fork_tags_are_not_deduped() {
        let (txns, _peer, peer_addr) = txn_rig().await;

        // Fork 1's 180 (tag=f1) — delivered.
        assert!(
            txns.on_inbound(&resp_tag(180, "z9hG4bK-i1", "1 INVITE", "f1"), peer_addr),
            "the first fork's 180 is delivered"
        );
        // Fork 2's 180 (tag=f2) on the SAME branch — a DISTINCT early dialog, so
        // it must be delivered too (not absorbed as a retransmit).
        assert!(
            txns.on_inbound(&resp_tag(180, "z9hG4bK-i1", "1 INVITE", "f2"), peer_addr),
            "a distinct-tag fork's 180 must NOT be deduped as a retransmit"
        );
        // Fork 1's 180 again (tag=f1) — a genuine retransmit, absorbed.
        assert!(
            !txns.on_inbound(&resp_tag(180, "z9hG4bK-i1", "1 INVITE", "f1"), peer_addr),
            "a same-tag repeat is a true retransmit and IS deduped"
        );
        // A losing fork's LATE 200 (tag=f2) is also delivered (the winner is f1,
        // but the body must see f2's 200 to ACK+BYE it, §13.2.2.4).
        assert!(
            txns.on_inbound(&resp_tag(200, "z9hG4bK-i1", "1 INVITE", "f2"), peer_addr),
            "a losing fork's late 200 (distinct tag) reaches the body for ACK+BYE"
        );
        txns.shutdown();
    }

    fn ack_req(branch: &str, cseq: u32) -> Vec<u8> {
        format!(
            "ACK sip:b@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: {cseq} ACK\r\n\r\n"
        )
        .into_bytes()
    }

    /// Always-display re-emission (the invisible-recovery gap): the send tap sees
    /// EVERY recovery frame the engine puts on the wire, tagged by kind — the
    /// proactive Timer A/E/G retransmit, the reactive re-ACK on a duplicate 2xx,
    /// and the reactive re-answer to a duplicate request. Without it these hit the
    /// socket below the recording layer and never reach the ladder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calltxns_sendtap_records_every_reemission() {
        let (txns, peer, peer_addr) = txn_rig().await;
        let seen = Arc::new(Mutex::new(Vec::<ReEmitKind>::new()));
        let sink = seen.clone();
        txns.set_sendtap(Arc::new(move |_raw, _dst, kind| {
            sink.lock().unwrap().push(kind);
        }));

        // (1) Proactive Timer-E retransmit of an outbound UPDATE, then stop it.
        txns.on_outbound(&update_req("z9hG4bK-u1"), peer_addr);
        recv_one(&peer, 2_000).await.expect("Timer E retransmit of the UPDATE");
        assert!(txns.on_inbound(&resp(200, "z9hG4bK-u1", "2 UPDATE", ""), peer_addr));

        // (2) Reactive re-answer to a duplicate request (our 200 was lost).
        let prack = prack_req("z9hG4bK-p1", "1 1 INVITE");
        assert!(txns.on_inbound(&prack, peer_addr));
        txns.on_outbound(&resp(200, "z9hG4bK-p1", "2 PRACK", ""), peer_addr);
        assert!(!txns.on_inbound(&prack, peer_addr), "dup PRACK absorbed → re-answer");

        // (3) Reactive re-ACK on a duplicate INVITE 2xx (our ACK was lost).
        let ok_inv = resp(200, "z9hG4bK-i9", "1 INVITE", "");
        assert!(txns.on_inbound(&ok_inv, peer_addr), "first 200 (INVITE) delivered");
        txns.on_outbound(&ack_req("z9hG4bK-a9", 1), peer_addr);
        assert!(!txns.on_inbound(&ok_inv, peer_addr), "dup 200 (INVITE) absorbed → re-ACK");

        let _ = recv_one(&peer, 200).await; // drain stragglers
        let kinds = seen.lock().unwrap().clone();
        assert!(kinds.contains(&ReEmitKind::Retransmit), "proactive retransmit tapped: {kinds:?}");
        assert!(kinds.contains(&ReEmitKind::ReAnswer), "reactive re-answer tapped: {kinds:?}");
        assert!(kinds.contains(&ReEmitKind::ReAck), "reactive re-ACK tapped: {kinds:?}");
        txns.shutdown();
    }
}
