//! Per-endpoint inbound demultiplexing — several actors on ONE bound UA.
//!
//! An [`Agent`] is one UA stack: one socket, one §17.2 receive view, one ACK
//! obligation table. A production peer plays SEVERAL roles on that one stack —
//! it originates a call and, under a different Call-ID, receives one the far end
//! re-originates back to the same socket pair (an application-server loopback).
//! Two actors both pulling [`Agent::recv_any`] would race for every datagram, so
//! an endpoint with more than one actor gets ONE receive pump ([`EndpointPump`])
//! that owns the pull and hands each inbound to its owner:
//!
//! * a message whose Call-ID a member already owns follows that member — the
//!   dialog identity, learned when the member ORIGINATED the dialog
//!   ([`EndpointHandle::own_dialog`]) or when its claim took the inbound leg;
//! * an inbound INITIAL INVITE is assigned by [`resolve_claim`] over the members'
//!   pending [`ClaimRule`]s — the shared precedence, never re-decided here — and
//!   the winning claim is consumed, binding the new dialog to that member;
//! * an initial INVITE no pending claim owns is COUNTED and recorded
//!   ([`Observation::UnclaimedInbound`]), never silently dropped;
//! * any other REQUEST on an unknown dialog goes to the endpoint's
//!   first-declared actor, whose reactor services it exactly as on an unshared
//!   endpoint — stray servicing (a 481, an OPTIONS 200) is member-agnostic, so
//!   the pick cannot cross-wire a dialog;
//! * a RESPONSE on an unknown dialog answers a request no member originated
//!   here — delivering it could satisfy the WRONG actor's pending expectation,
//!   so it is counted and recorded, never guessed at.
//!
//! An actor that owns its endpoint outright keeps pulling `recv_any` directly
//! ([`Inbox::Own`]) — the pump exists only where an endpoint is shared.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::endpoint::ActorSpec;
use super::state::{Observation, ObservedState};
use crate::agent::{Agent, Inbound};
use crate::claim::{resolve_claim, ClaimRule};
use crate::legpick::LegInfo;
use crate::StepError;

/// Where one actor's inbound messages come from.
pub enum Inbox {
    /// The actor owns its endpoint — it pulls the UA's receive view itself
    /// (the unshared default; behaviour identical to a direct `recv_any`).
    Own(Agent),
    /// The actor shares its endpoint — the endpoint's ONE pump delivers here.
    Shared {
        role: &'static str,
        rx: mpsc::UnboundedReceiver<Inbound>,
        /// The wait bound a silent inbox reports as a (non-fatal) timeout, so a
        /// sharing actor re-evaluates its exit condition exactly as often as an
        /// unshared one.
        idle: Duration,
    },
}

impl Inbox {
    /// The next inbound for this actor. A silent wait is
    /// [`StepError::Timeout`] and a gone endpoint [`StepError::QueueClosed`] —
    /// the same vocabulary [`Agent::recv_any`] returns, so the reactor's arm is
    /// source-agnostic. Cancel-safe: nothing is consumed unless it is returned.
    pub async fn recv(&mut self) -> Result<Inbound, StepError> {
        match self {
            Inbox::Own(agent) => agent.recv_any().await,
            Inbox::Shared { role, rx, idle } => {
                match tokio::time::timeout(*idle, rx.recv()).await {
                    Ok(Some(m)) => Ok(m),
                    Ok(None) => Err(StepError::QueueClosed { who: role.to_string() }),
                    Err(_) => Err(StepError::Timeout { who: role.to_string() }),
                }
            }
        }
    }
}

/// One actor's seat at a shared endpoint.
struct Member {
    role: &'static str,
    /// The rule by which an inbound initial INVITE is this actor's. `None` = the
    /// actor claims nothing: it receives only the dialogs it originates.
    claim: Option<ClaimRule>,
    /// Set once the claim has fired — a claim is consumed exactly once.
    fired: bool,
    tx: mpsc::UnboundedSender<Inbound>,
}

/// A shared endpoint's routing table: which member owns which dialog, and which
/// claims are still pending.
struct Demux {
    /// The UA's name — the endpoint's identity in the observed record.
    endpoint: String,
    members: Vec<Member>,
    /// Call-ID → owning member. Grow-only for the life of the call.
    owner: HashMap<String, usize>,
    /// How many [`ClaimRule::ArrivalOrder`] claims have fired (the ordinal
    /// `resolve_claim` compares against).
    ordinal: usize,
    obs: ObservedState,
}

impl Demux {
    /// Hand `msg` to its owner, recording it when no owner exists (or the owner
    /// has finished). Synchronous throughout — the decision never awaits.
    fn deliver(&mut self, msg: Inbound) {
        let detail = describe(&msg);
        match self.owner_of(&msg) {
            Ok(i) => {
                let role = self.members[i].role;
                if self.members[i].tx.send(msg).is_err() {
                    self.record(format!("{detail} (actor {role} finished)"));
                }
            }
            Err(reason) => self.record(format!("{detail} ({reason})")),
        }
    }

    /// The member owning `msg`: the dialog's, else — for a dialog-creating
    /// INVITE — whichever pending claim takes it (consumed here). A stray
    /// REQUEST on an unknown dialog goes to the endpoint's first-declared actor
    /// (stray servicing is member-agnostic); a RESPONSE on an unknown dialog is
    /// refused — no member originated its request, so delivering it could
    /// satisfy the wrong actor's expectation. `Err` names the refusal.
    fn owner_of(&mut self, msg: &Inbound) -> Result<usize, &'static str> {
        let call_id = call_id_of(msg);
        if let Some(&i) = self.owner.get(&call_id) {
            return Ok(i);
        }
        match msg {
            Inbound::Request(txn) => {
                // The datagram as it arrived — a received message carries its
                // own image, so the demux reads the wire, never a re-render.
                let leg = LegInfo::new(txn.request().image());
                if leg.is_initial_invite() {
                    let i = self.claim_leg(&leg).ok_or("no pending claim")?;
                    self.owner.insert(call_id, i);
                    return Ok(i);
                }
                Ok(0)
            }
            Inbound::Response(_) => Err("no owning dialog"),
        }
    }

    /// Resolve the pending claim owning this initial INVITE, consuming it.
    fn claim_leg(&mut self, leg: &LegInfo<'_>) -> Option<usize> {
        let pending: Vec<Option<&ClaimRule>> =
            self.members.iter().map(|m| if m.fired { None } else { m.claim.as_ref() }).collect();
        let i = resolve_claim(&pending, leg, self.ordinal)?;
        if matches!(self.members[i].claim, Some(ClaimRule::ArrivalOrder(_))) {
            self.ordinal += 1;
        }
        self.members[i].fired = true;
        Some(i)
    }

    /// Record an inbound this endpoint could hand to no actor.
    fn record(&self, detail: String) {
        self.obs.record(
            Observation::UnclaimedInbound { endpoint: self.endpoint.clone(), detail },
            Instant::now(),
        );
    }
}

/// One shared endpoint's receive pump: the ONE consumer of its UA's receive
/// view. Runs alongside the actors for the life of the call.
pub struct EndpointPump {
    agent: Agent,
    demux: Arc<Mutex<Demux>>,
}

impl EndpointPump {
    /// Pull this endpoint's inbounds and deliver each to its owner until the
    /// endpoint's queue closes. A silent wait is not fatal (the actors' own exit
    /// conditions end the call); an unparseable datagram is, exactly as it is
    /// for an unshared actor.
    pub async fn run(self) -> Result<(), StepError> {
        loop {
            match self.agent.recv_any().await {
                Ok(m) => self.demux.lock().unwrap().deliver(m),
                Err(StepError::Timeout { .. }) => {}
                Err(StepError::QueueClosed { .. }) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

/// An actor's registration handle on its shared endpoint.
#[derive(Clone)]
pub struct EndpointHandle {
    demux: Arc<Mutex<Demux>>,
    member: usize,
}

impl EndpointHandle {
    /// Bind a dialog this actor ORIGINATED to it, so the dialog's responses and
    /// in-dialog requests come back to this actor rather than to the endpoint's
    /// first-declared one. Called in the same poll as the origination, before
    /// the actor yields again — no inbound for the new dialog can precede it.
    pub fn own_dialog(&self, call_id: impl Into<String>) {
        self.demux.lock().unwrap().owner.insert(call_id.into(), self.member);
    }
}

/// Wire the actors that SHARE a UA stack ([`Agent::stack_id`]): one
/// [`EndpointPump`] per shared endpoint, plus each sharing actor's inbox and
/// registration handle keyed by role. An actor that owns its endpoint outright
/// appears in neither — it keeps pulling `recv_any`, byte-for-byte as before.
pub fn wire_shared_endpoints(
    specs: &[ActorSpec],
    obs: &ObservedState,
) -> (Vec<EndpointPump>, HashMap<&'static str, (Inbox, EndpointHandle)>) {
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for (i, spec) in specs.iter().enumerate() {
        let stack = spec.agent.stack_id();
        match groups.iter_mut().find(|(s, _)| *s == stack) {
            Some((_, members)) => members.push(i),
            None => groups.push((stack, vec![i])),
        }
    }

    let mut pumps = Vec::new();
    let mut wiring = HashMap::new();
    for (_, indices) in groups.into_iter().filter(|(_, m)| m.len() > 1) {
        let agent = specs[indices[0]].agent.clone();
        // The seat's silent-wait bound is the UA's own `recv_timeout`, so a
        // sharing actor wakes to re-check its exit condition exactly as often
        // as one pulling `recv_any` itself.
        let idle = agent.recv_timeout;
        let mut members = Vec::with_capacity(indices.len());
        let mut inboxes = Vec::with_capacity(indices.len());
        for &i in &indices {
            let (tx, rx) = mpsc::unbounded_channel();
            let spec = &specs[i];
            members.push(Member { role: spec.role, claim: spec.claim.clone(), fired: false, tx });
            inboxes.push((spec.role, Inbox::Shared { role: spec.role, rx, idle }));
        }
        let demux = Arc::new(Mutex::new(Demux {
            endpoint: agent.name().to_string(),
            members,
            owner: HashMap::new(),
            ordinal: 0,
            obs: obs.clone(),
        }));
        for (member, (role, inbox)) in inboxes.into_iter().enumerate() {
            wiring.insert(role, (inbox, EndpointHandle { demux: demux.clone(), member }));
        }
        pumps.push(EndpointPump { agent, demux });
    }
    (pumps, wiring)
}

/// This inbound's dialog identity.
fn call_id_of(msg: &Inbound) -> String {
    match msg {
        Inbound::Request(txn) => txn.request().call_id().to_string(),
        Inbound::Response(r) => r.call_id().to_string(),
    }
}

/// This inbound in one bounded line — what a record names it by.
fn describe(msg: &Inbound) -> String {
    match msg {
        Inbound::Request(txn) => {
            let r = txn.request();
            format!("{} request (Call-ID {})", r.method(), r.call_id())
        }
        Inbound::Response(r) => format!("{} response (Call-ID {})", r.status(), r.call_id()),
    }
}
