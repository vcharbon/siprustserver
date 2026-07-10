//! Multi-callee routing — several logical [`Agent`]s on ONE bound socket.
//!
//! # Why
//!
//! A call-transfer (and any multi-callee flow) has more than one callee-side UA
//! on the fabric — Bob (transferor), Charlie (transferee), David (reroute
//! target). The B2BUA egresses **every** callee leg's INVITE to its single ROUTE
//! target address, so all of those legs land on **one** address. A plain
//! [`Harness::agent`](crate::Harness::agent) binds one socket per agent and its
//! single FIFO recv queue then cannot disambiguate — Charlie's re-INVITE
//! *responses* interleave with Bob's release *requests* (NOTIFY/BYE) on the same
//! queue, and `receive`/`expect` mis-classify the reorder.
//!
//! # What
//!
//! [`Harness::callee_group`](crate::Harness::callee_group) binds ONE socket and
//! vends several logical [`Agent`]s that share it, demultiplexed by a
//! [`LegPicker`] (the shared [`crate::legpick`] primitive):
//!
//! * an **out-of-dialog INVITE** is routed to the logical agent whose R-URI
//!   user-part prefix the picker matches (Charlie's ANNUAIRE digits, David's
//!   reroute number, Bob's original callee number — already prefix-distinct on
//!   the wire, so no routing-mock change is needed);
//! * every **in-dialog** message (a re-INVITE, ACK, BYE, NOTIFY, or a response)
//!   follows its dialog's owner — learned from the initial INVITE and keyed by
//!   Call-ID thereafter.
//!
//! Each logical agent keeps its OWN recv queue (packets for a sibling are stashed
//! under that sibling's key, not dropped or mis-delivered), so the crossing /
//! interleaving cases become drivable. Both the **direct** and the
//! **via-proxy** paths work: the demux keys on the R-URI user-part and Call-ID,
//! which are source-address-agnostic.
//!
//! This is pure test-harness plumbing — no B2BUA behaviour changes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use layer_harness::NetworkTag;
use sip_net::{
    BindUdpOpts, SendError, UaRole, UdpEndpoint, UdpEndpointCounters, UdpPacket,
};

use crate::agent::{decide_rr_fold, Agent, Harness};
use crate::legpick::{labelled_prefix_leg_picker, LegInfo};

/// A picker that resolves a leg to an owning logical-agent **name**, or `None`
/// for a no-route leg (dropped, never mis-delivered).
type OwnerPicker = Box<dyn Fn(&LegInfo) -> Option<String> + Send + Sync>;

/// Shared demux state behind every logical agent's sub-endpoint. One instance
/// per group, wrapped in a `Mutex` (the `UdpEndpoint` trait is `Send + Sync`;
/// the functional lane drives it single-threaded, so the lock is never
/// contended — it is held only for the O(1) classify/stash, never across a
/// `.await`).
struct Router {
    /// Out-of-dialog INVITE → owning agent name (by R-URI prefix).
    pick: OwnerPicker,
    /// Learned dialog ownership: Call-ID → owning agent name. Seeded on the
    /// initial INVITE, then every in-dialog message (request OR response) of
    /// that Call-ID follows it.
    call_owner: HashMap<String, String>,
    /// Per-owner backlog: a packet pulled off the shared socket that belongs to
    /// a sibling waits here until that sibling recvs — never re-ordered onto the
    /// wrong agent.
    stash: HashMap<String, VecDeque<UdpPacket>>,
}

/// Where a freshly-pulled packet goes.
enum Route {
    /// It is for the agent that pulled it — hand it back.
    Mine(UdpPacket),
    /// It was stashed for a sibling — the puller loops and pulls again.
    Stashed,
    /// No receiver owns it — dropped (observable via the eprintln below).
    Orphan,
}

impl Router {
    /// Decide the owning agent name for a raw datagram. `None` = no route.
    fn owner_of(&mut self, raw: &[u8]) -> Option<String> {
        let leg = LegInfo::new(raw);
        let cid = leg.header("call-id").or_else(|| leg.header("i"));
        // A known dialog wins outright — in-dialog requests, responses, ACKs and
        // even an INVITE retransmit all follow the leg that first owned the call.
        if let Some(cid) = &cid {
            if let Some(owner) = self.call_owner.get(cid) {
                return Some(owner.clone());
            }
        }
        // Unknown dialog: only an out-of-dialog INVITE (no To-tag) mints a new
        // callee leg — pick by R-URI prefix and remember the ownership.
        if is_out_of_dialog_invite(&leg) {
            let owner = (self.pick)(&leg)?;
            if let Some(cid) = cid {
                self.call_owner.insert(cid, owner.clone());
            }
            return Some(owner);
        }
        // A stray in-dialog request / non-INVITE for an unseen dialog: best-effort
        // pick by R-URI so a mid-flow request still reaches a plausible owner
        // rather than orphaning. A response (no R-URI) yields `None`.
        (self.pick)(&leg)
    }
}

/// A logical [`Agent`]'s endpoint onto a shared callee socket. `recv` pulls from
/// the shared socket and returns only the packets this agent owns, stashing any
/// sibling's packet for that sibling; `send_to` goes straight out the shared
/// (recording-wrapped) socket, so the trace still sees every send on the one
/// lane.
struct SubEndpoint {
    owner: String,
    addr: SocketAddr,
    shared: Arc<dyn UdpEndpoint>,
    router: Arc<Mutex<Router>>,
}

impl SubEndpoint {
    /// Pop a previously-stashed packet for this owner, if any.
    fn pop_own(&self) -> Option<UdpPacket> {
        self.router
            .lock()
            .unwrap()
            .stash
            .get_mut(&self.owner)
            .and_then(|q| q.pop_front())
    }

    /// Classify a freshly-pulled packet under a single lock: hand it back if it
    /// is ours, stash it for the owning sibling, or drop a no-route orphan.
    fn route(&self, pkt: UdpPacket) -> Route {
        let mut r = self.router.lock().unwrap();
        match r.owner_of(&pkt.raw) {
            Some(owner) if owner == self.owner => Route::Mine(pkt),
            Some(owner) => {
                r.stash.entry(owner).or_default().push_back(pkt);
                Route::Stashed
            }
            None => {
                eprintln!(
                    "[callee-group] no-route leg dropped on {} (no receiver owns its R-URI/dialog)",
                    self.addr
                );
                Route::Orphan
            }
        }
    }
}

#[async_trait]
impl UdpEndpoint for SubEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        self.shared.send_to(buf, dst).await
    }

    async fn recv(&self) -> Option<UdpPacket> {
        loop {
            if let Some(pkt) = self.pop_own() {
                return Some(pkt);
            }
            // No lock held across this await — siblings can classify concurrently.
            let pkt = self.shared.recv().await?;
            match self.route(pkt) {
                Route::Mine(pkt) => return Some(pkt),
                Route::Stashed | Route::Orphan => continue,
            }
        }
    }

    fn try_recv(&self) -> Option<UdpPacket> {
        loop {
            if let Some(pkt) = self.pop_own() {
                return Some(pkt);
            }
            let pkt = self.shared.try_recv()?;
            match self.route(pkt) {
                Route::Mine(pkt) => return Some(pkt),
                Route::Stashed | Route::Orphan => continue,
            }
        }
    }

    fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    fn queue_depth(&self) -> usize {
        let own = self
            .router
            .lock()
            .unwrap()
            .stash
            .get(&self.owner)
            .map_or(0, VecDeque::len);
        own + self.shared.queue_depth()
    }

    fn queue_max(&self) -> usize {
        self.shared.queue_max()
    }

    fn counters(&self) -> UdpEndpointCounters {
        self.shared.counters()
    }
}

/// A set of logical [`Agent`]s that share one bound socket, demultiplexed by
/// R-URI prefix (out-of-dialog) and Call-ID (in-dialog). Built via
/// [`Harness::callee_group`](crate::Harness::callee_group).
pub struct CalleeGroup {
    agents: HashMap<String, Agent>,
    addr: SocketAddr,
}

impl CalleeGroup {
    /// The shared bound address (the B2BUA's single ROUTE target).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The logical agent bound under `name`. Panics if `name` was not declared —
    /// a scenario wiring bug, caught loudly at setup.
    pub fn agent(&self, name: &str) -> Agent {
        self.agents
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("callee-group has no agent named {name:?}"))
    }
}

/// Builder for a [`CalleeGroup`] — declare each logical callee and the R-URI
/// user-part prefix that routes to it, then [`build`](Self::build).
pub struct CalleeGroupBuilder<'h> {
    harness: &'h Harness,
    addr: SocketAddr,
    roles: HashSet<UaRole>,
    queue_max: usize,
    /// `(agent name, R-URI user-part prefix)` in declaration order.
    members: Vec<(String, String)>,
}

impl<'h> CalleeGroupBuilder<'h> {
    pub(crate) fn new(harness: &'h Harness, addr: SocketAddr) -> Self {
        Self {
            harness,
            addr,
            // A callee UA answers (Uas) and may originate in-dialog (Uac, e.g.
            // the transferee's own re-INVITE); never a proxy.
            roles: HashSet::from([UaRole::Uac, UaRole::Uas]),
            queue_max: 64,
            members: Vec::new(),
        }
    }

    /// Declare a logical callee `name` owning every leg whose R-URI user-part
    /// starts with `ruri_prefix`. Longest-match wins across siblings, so
    /// prefixes that are prefixes of each other (`bob` vs `bob2`) are fine.
    pub fn callee(mut self, name: impl Into<String>, ruri_prefix: impl Into<String>) -> Self {
        self.members.push((name.into(), ruri_prefix.into()));
        self
    }

    /// Override the bind roles for RFC-rule subject dispatch (default
    /// `{Uac, Uas}`).
    pub fn with_roles(mut self, roles: HashSet<UaRole>) -> Self {
        self.roles = roles;
        self
    }

    /// Bind the shared socket and materialize the logical agents.
    pub async fn build(self) -> CalleeGroup {
        assert!(!self.members.is_empty(), "a callee-group needs at least one callee");

        let lane_name = self
            .members
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join("+");
        // One recorded lane for the shared socket — every leg's send/recv tees
        // onto it, so the RFC hard gate judges the (prefix-distinct) callee legs
        // as one peer, exactly as they land on the wire.
        self.harness.register_lane(self.addr, lane_name, NetworkTag::Ext);

        let ep = self
            .harness
            .network()
            .bind_udp(BindUdpOpts::new(self.addr, self.queue_max).with_roles(self.roles))
            .await
            .unwrap_or_else(|e| panic!("callee-group bind {} failed: {e}", self.addr));
        let shared: Arc<dyn UdpEndpoint> = Arc::from(ep);

        // Longest-prefix picker over the members, each prefix labelled with its
        // agent name (the shared `legpick` primitive).
        let base = labelled_prefix_leg_picker(
            self.members.iter().map(|(n, p)| (p.clone(), n.clone())),
        );
        let pick: OwnerPicker = Box::new(move |leg: &LegInfo| {
            let matched = base(leg);
            (!matched.is_empty()).then_some(matched)
        });

        let router = Arc::new(Mutex::new(Router {
            pick,
            call_owner: HashMap::new(),
            stash: HashMap::new(),
        }));

        let ids = self.harness.ids();
        let recv_timeout = self.harness.recv_timeout();
        let mut agents = HashMap::new();
        for (name, _) in &self.members {
            let sub = SubEndpoint {
                owner: name.clone(),
                addr: self.addr,
                shared: shared.clone(),
                router: router.clone(),
            };
            let ep: Arc<dyn UdpEndpoint> = Arc::new(sub);
            let agent = Agent {
                name: name.clone(),
                addr: self.addr,
                uri: format!("sip:{name}@{}", self.addr.ip()),
                ep,
                ids: ids.clone(),
                rr_fold: decide_rr_fold(name),
                recv_timeout,
                // Each logical callee is its own UA: per-leg §17.2 receive
                // view over the shared socket (newkahneed-034).
                txn: std::sync::Arc::new(crate::agent::TxnView::functional()),
            };
            agents.insert(name.clone(), agent);
        }

        CalleeGroup { agents, addr: self.addr }
    }
}

/// Whether `leg` is a dialog-creating (out-of-dialog) INVITE — method INVITE
/// with a tag-less To (RFC 3261 §12.1: an initial INVITE's To carries no tag).
fn is_out_of_dialog_invite(leg: &LegInfo) -> bool {
    let is_invite = leg.method().is_some_and(|m| m.eq_ignore_ascii_case("INVITE"));
    let to = leg.header("to").or_else(|| leg.header("t")).unwrap_or_default();
    is_invite && !to.to_ascii_lowercase().contains(";tag=")
}
