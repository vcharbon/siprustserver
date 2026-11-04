//! [`WeakAgent`]: an [`Agent`] that does not keep its socket bound, and the
//! two conversions between them.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_clock::Clock;
use sip_message::SipMessage;
use sip_net::UdpEndpoint;

use super::ids::Ids;
use super::party::Party;
use super::rr_fold::RecordRouteFold;
use super::step::{StepCounter, StepError};
use super::Agent;
use crate::absorption::{Absorption, TwoXxAcks};
use crate::dialogs::DialogJudge;

/// An [`Agent`] that does not keep its socket bound: what a harness holds so
/// that, at its end, it sights what is still queued on each UA it bound.
pub(crate) struct WeakAgent {
    name: String,
    addr: SocketAddr,
    uri: String,
    ep: std::sync::Weak<dyn UdpEndpoint>,
    ids: Arc<Ids>,
    rr_fold: RecordRouteFold,
    recv_timeout: Duration,
    txn: Arc<Absorption>,
    two_xx_acks: Arc<TwoXxAcks>,
    holdback: Arc<Mutex<VecDeque<Result<SipMessage, StepError>>>>,
    dialogs: Arc<DialogJudge>,
    steps: StepCounter,
    answers_as: Party,
    advertised: Option<SocketAddr>,
    arrivals: Option<Clock>,
}

impl WeakAgent {
    /// The agent, while its socket is still bound by someone.
    pub(crate) fn upgrade(&self) -> Option<Agent> {
        Some(Agent {
            name: self.name.clone(),
            addr: self.addr,
            uri: self.uri.clone(),
            ep: self.ep.upgrade()?,
            ids: self.ids.clone(),
            rr_fold: self.rr_fold,
            recv_timeout: self.recv_timeout,
            txn: self.txn.clone(),
            two_xx_acks: self.two_xx_acks.clone(),
            holdback: self.holdback.clone(),
            dialogs: self.dialogs.clone(),
            steps: self.steps.clone(),
            answers_as: self.answers_as,
            advertised: self.advertised,
            arrivals: self.arrivals.clone(),
        })
    }
}

impl Agent {
    /// This agent without a hold on its socket.
    pub(crate) fn downgrade(&self) -> WeakAgent {
        WeakAgent {
            name: self.name.clone(),
            addr: self.addr,
            uri: self.uri.clone(),
            ep: Arc::downgrade(&self.ep),
            ids: self.ids.clone(),
            rr_fold: self.rr_fold,
            recv_timeout: self.recv_timeout,
            txn: self.txn.clone(),
            two_xx_acks: self.two_xx_acks.clone(),
            holdback: self.holdback.clone(),
            dialogs: self.dialogs.clone(),
            steps: self.steps.clone(),
            answers_as: self.answers_as,
            advertised: self.advertised,
            arrivals: self.arrivals.clone(),
        }
    }
}
