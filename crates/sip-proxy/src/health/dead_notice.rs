//! The dead notice (ADR-0050 §3): each dead verdict of the proxy's health
//! view is told to every other `Alive` worker, on an out-of-dialog OPTIONS
//! carrying `X-Worker-Dead` signed with the cluster key. A worker that backs
//! the dead one's calls takes over those with work in flight at once, as the
//! proxy now routes the dead worker's traffic to it (ADR-0047 D1).
//!
//! The verdict is the registry's ([`WorkerRegistry::read_dead`]): a turn to
//! `Dead` of the health view, whatever wrote it, the probe's misses in a runner
//! or a harness's health write in a test, and the flip of a worker read `Alive`
//! to not ready, not terminating, with another member still ready. A membership
//! departure and a terminating flip are no verdict. The notice's instant is when
//! it is told, so another proxy's notice of the same flip is the same verdict only
//! at the same instant, a later one otherwise. Each notice is one non-INVITE client
//! transaction, retransmitted on Timer E until it is answered or Timer F ends it; the
//! answer is the worker's ordinary OPTIONS report, read by the probe only.

use std::sync::Arc;

use call::TxnKind;
use sip_clock::Clock;
use sip_ids::SigningKey;
use sip_message::dead_notice::{DeadNotice, X_WORKER_DEAD};
use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipHeader, SipRequest, SipStr};
use sip_net::UdpEndpoint;
use sip_txn::{IdGen, TransactionConfig, TransactionLayer, TransactionLayerClosed};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use super::stated_host::stated_host;
use crate::registry::{WorkerEntry, WorkerId, WorkerRegistry};

/// The sender of dead notices. Owns a UAC endpoint of its own.
pub struct DeadNotifier {
    txn: TransactionLayer,
    events: sip_txn::TransactionEvents,
    host: String,
    port: u16,
    registry: Arc<dyn WorkerRegistry>,
    verdicts: broadcast::Receiver<WorkerId>,
    key: SigningKey,
    clock: Clock,
    id_gen: Arc<IdGen>,
}

impl DeadNotifier {
    /// Must be called within a tokio runtime (it spawns the transaction-layer
    /// owner task over `endpoint`). Every verdict from now on is told.
    /// `parser` is the process's ingress parser.
    pub fn new(
        endpoint: Box<dyn UdpEndpoint>,
        parser: CustomParser,
        registry: Arc<dyn WorkerRegistry>,
        key: SigningKey,
        clock: Clock,
        id_gen: Arc<IdGen>,
    ) -> Self {
        let local = endpoint.local_addr();
        let (txn, events) = TransactionLayer::spawn(
            endpoint,
            Arc::new(parser),
            TransactionConfig::new(id_gen.clone()),
        );
        let verdicts = registry.read_dead();
        Self {
            txn,
            events,
            host: stated_host(local),
            port: local.port(),
            registry,
            verdicts,
            key,
            clock,
            id_gen,
        }
    }

    /// Tell every verdict until the transaction layer or the registry closes
    /// (spawn it; abort to stop). A reader that lags tells every ordinal a
    /// verdict may stand on now ([`WorkerRegistry::read_dead_now`]).
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                verdict = self.verdicts.recv() => {
                    let told = match verdict {
                        Ok(dead) => self.tell(&dead).await,
                        Err(RecvError::Lagged(_)) => self.tell_every_dead().await,
                        Err(RecvError::Closed) => return,
                    };
                    if told.is_err() {
                        return;
                    }
                }
                ev = self.events.recv() => if ev.is_none() {
                    return;
                },
            }
        }
    }

    async fn tell_every_dead(&self) -> Result<(), TransactionLayerClosed> {
        for id in self.registry.read_dead_now() {
            self.tell(&id).await?;
        }
        Ok(())
    }

    /// Tell every other `Alive` worker, routed or not, that `dead` is read dead
    /// now.
    async fn tell(&self, dead: &str) -> Result<(), TransactionLayerClosed> {
        let notice = DeadNotice { ordinal: dead.to_string(), at_ms: self.clock.now_ms() };
        let fields = notice.signed_fields();
        let signature = self.key.sign(&fields.each_ref().map(Vec::as_slice));
        let Some(value) = notice.header_value(&signature) else {
            tracing::warn!(worker = dead, "dead notice not sent: the ordinal makes no header");
            return Ok(());
        };
        tracing::info!(node = observe::node(), worker = dead, "dead verdict told to the workers");
        for w in self.registry.probed() {
            if w.id == dead || !w.is_alive() {
                continue;
            }
            let Some(dst) = w.address.to_socket_addr() else { continue };
            let Some(req) = self.build_options(&w, &value) else { continue };
            self.txn.send_request(req, dst, TxnKind::NonInvite).await?;
        }
        Ok(())
    }

    /// The OPTIONS carrying the notice `value` to `w`; `None` where its
    /// address does not make one.
    fn build_options(&self, w: &WorkerEntry, value: &str) -> Option<SipRequest> {
        let branch = self.id_gen.new_branch();
        let tag = self.id_gen.new_tag();
        generate_out_of_dialog_request(
            OutOfDialogMethod::Options,
            &GenerateOutOfDialogRequestOpts {
                request_uri: Some(
                    Uri::sip(SipStr::owned(&w.address.host)).with_port(w.address.port),
                ),
                call_id: format!("dead-notice-{branch}@{}", self.host),
                from: Some(
                    header::From::from_uri(Uri::sip_user(
                        SipStr::from_static("proxy"),
                        SipStr::owned(&self.host),
                    ))
                    .with_tag(SipStr::owned(&tag)),
                ),
                to: Some(header::To::from_uri(
                    Uri::sip_user(SipStr::from_static("worker"), SipStr::owned(&w.address.host))
                        .with_port(w.address.port),
                )),
                cseq: 1,
                via: Some(
                    Via::udp(SipStr::owned(&self.host), self.port)
                        .with_branch(SipStr::owned(&branch))
                        .requesting_rport(),
                ),
                contact: Some(header::Contact::from_uri(
                    Uri::sip_user(SipStr::from_static("proxy"), SipStr::owned(&self.host))
                        .with_port(self.port),
                )),
                max_forwards: Some(70),
                extra_headers: vec![SipHeader::new(X_WORKER_DEAD, value.to_string())],
                ..Default::default()
            },
        )
        .inspect_err(|e| {
            tracing::warn!(worker = %w.id, error = %e, "dead notice not sent: its address makes no OPTIONS");
        })
        .ok()
    }
}

#[cfg(test)]
mod tests;
