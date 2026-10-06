//! A third-party transaction-stateful proxy (RFC 3261 §16) on the harness
//! fabric, running on its own as datagrams arrive. It is nobody's own proxy:
//! it knows nothing of the B2BUA's cookies or parameters and routes by Route
//! and Request-URI only, with the §16 rewrite of
//! [`scenario_harness::forwarded_request`].
//!
//! - It sends a request to the hop its Route or Request-URI names, or to one
//!   fixed next hop when started with one ([`spawn_stateful_proxy_toward`]: a
//!   local policy, §16.6 step 7).
//! - It Record-Routes a dialog-creating INVITE ([`RecordRoute::Yes`]), or
//!   stays out of the dialog it forwards ([`RecordRoute::No`], §16.6 step 4
//!   makes the Record-Route optional): the endpoints' in-dialog requests then
//!   go from Contact to Contact.
//! - A server transaction is keyed by the received top-Via branch and method.
//!   A retransmitted request is absorbed, and the last response the
//!   transaction sent is replayed when there is one (§17.2).
//! - The downstream branch is derived from the upstream one (§16.11), so a
//!   request is never forwarded twice under two branches.
//! - A request with Max-Forwards 0 is answered 483 and not forwarded; an ACK,
//!   which admits no response, is dropped (§16.3 step 3).
//! - An INVITE is answered 100 Trying at once and the downstream 100 is
//!   absorbed (§16.2, §16.7 step 5).
//! - A CANCEL is answered 200 here and a CANCEL of the forwarded INVITE, on
//!   its branch, goes downstream; that CANCEL's 200 is absorbed (§16.10).
//! - A non-2xx final to a forwarded INVITE is ACKed downstream by this hop,
//!   its repeats re-ACKed and absorbed, and the caller's ACK to it absorbed
//!   (§17.1.1.3); a 2xx and its repeats are forwarded (§16.7).
//! - Started with a second target ([`spawn_forking_proxy`]), it forks every
//!   dialog-creating INVITE in parallel (§16.6): one branch where Route or
//!   Request-URI names, one to the second target. Every provisional and every
//!   2xx goes upstream; the first 2xx CANCELs the branches still pending
//!   (§16.7 step 10); a non-2xx final goes upstream only once every branch is
//!   final and none answered, the best one chosen (§16.7 step 6, §16.7.1).
//! - On a forward to one target, a first Route without `;lr` left once its
//!   own is popped is a strict next hop: its URI becomes the Request-URI, the
//!   Request-URI rides the Route tail, and the request goes there (§16.6
//!   steps 6-7).
//! - It routes a dialog loosely (§16.12), as a strict router (§16.4), or
//!   rewrites the Request-URI of the in-dialog requests it forwards
//!   ([`DialogRouting`], [`spawn_stateful_proxy_routing`]).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use scenario_harness::{
    forwarded_request, forwarded_response, request_next_hop, response_next_hop, Agent, Harness,
    Inbound, StepError,
};
use sip_message::draft::HeaderList;
use sip_message::generators::{
    generate_ack_for_non_2xx, generate_cancel, generate_response, GenerateResponseOpts,
    InviteClientTransactionHandle,
};
use sip_message::header::{HeaderName, RecordRouteEntry, RouteEntry, Uri};
use sip_message::hops::hops_exhausted;
use sip_message::{SipRequest, SipResponse};
use tokio::task::JoinHandle;

/// A running third-party proxy. Aborts its loop on drop.
pub struct StatefulProxy {
    pub addr: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for StatefulProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Whether the proxy Record-Routes the dialogs it forwards (RFC 3261 §16.6
/// step 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordRoute {
    #[default]
    Yes,
    No,
}

/// How the proxy carries the in-dialog requests of a dialog it Record-Routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DialogRouting {
    /// A loose router (RFC 3261 §16.12): a `;lr` Record-Route, and the
    /// Request-URI forwarded as received.
    #[default]
    Loose,
    /// A strict router (RFC 2543): its Record-Route URI carries no `;lr`, so a
    /// peer addresses it in the Request-URI and carries the remote target in
    /// the last Route (§12.2.1.1). It moves that last Route back into the
    /// Request-URI before forwarding (§16.4).
    Strict,
    /// A URI-rewriting element: every in-dialog request it forwards leaves
    /// with the bare address of its next hop as Request-URI, no user part and
    /// none of the parameters the target stated. A request lifted to a strict
    /// next hop keeps that hop's URI (§16.6 step 6).
    RewriteRequestUri,
}

impl DialogRouting {
    /// §16.4: a request whose Request-URI names `hop` was strict-routed to
    /// it; the last Route is the target it is forwarded to.
    fn received(self, req: &SipRequest, hop: SocketAddr) -> SipRequest {
        if self != DialogRouting::Strict || !names(req.request_uri(), hop) {
            return req.clone();
        }
        let Some(target) = req.route_set().ok().and_then(|r| r.last().map(|e| e.uri().clone()))
        else {
            return req.clone();
        };
        req.thaw()
            .with_uri(target)
            .list::<RouteEntry>(|routes| {
                let mut routes = routes.into_vec();
                routes.pop();
                HeaderList::new(routes)
            })
            .expect("the Route set just read")
            .freeze()
            .expect("a request stays complete without its last Route")
    }

    /// A strict router's Record-Route names it without `;lr`.
    fn record_routed(self, forwarded: SipRequest, hop: SocketAddr) -> SipRequest {
        if self != DialogRouting::Strict || forwarded.to().tag().is_some() {
            return forwarded;
        }
        if forwarded.method().as_str() != "INVITE" {
            return forwarded;
        }
        let strict =
            RecordRouteEntry::from_uri(Uri::sip(hop.ip().to_string()).with_port(hop.port()));
        let draft = forwarded
            .thaw()
            .pop_top::<RecordRouteEntry>()
            .expect("the Record-Route this hop just added pops");
        let draft = if draft.has(&HeaderName::RecordRoute) {
            draft.prepend(strict)
        } else {
            draft.push_front(strict)
        };
        draft.freeze().expect("a request stays complete with its Record-Route replaced")
    }

    /// The URI-rewriting element aims an in-dialog request at `next_hop`'s
    /// bare address.
    fn retargeted(self, forwarded: SipRequest, next_hop: SocketAddr) -> SipRequest {
        if self != DialogRouting::RewriteRequestUri || forwarded.to().tag().is_none() {
            return forwarded;
        }
        let bare = Uri::sip(next_hop.ip().to_string()).with_port(next_hop.port());
        forwarded.thaw().with_uri(bare).freeze().expect("a request stays complete retargeted")
    }
}

/// §16.6 steps 6-7: a first Route without `;lr` is a strict next hop. Its URI
/// becomes the Request-URI, the Request-URI rides the Route tail, and the
/// request is sent to that hop, whose address is returned.
fn to_strict_next_hop(forwarded: SipRequest) -> (SipRequest, Option<SocketAddr>) {
    let first = forwarded.route_set().ok().and_then(|r| r.first().map(|e| e.uri().clone()));
    let Some(strict) = first.filter(|uri| !uri.is_loose_route()) else { return (forwarded, None) };
    let (host, port) = strict.host_port();
    let hop: SocketAddr =
        format!("{host}:{port}").parse().expect("a strict route names an address");
    let target = RouteEntry::from_uri(forwarded.request_uri().clone());
    let shuffled = forwarded
        .thaw()
        .with_uri(strict)
        .list::<RouteEntry>(|routes| {
            let mut routes = routes.into_vec();
            routes.remove(0);
            routes.push(target);
            HeaderList::new(routes)
        })
        .expect("the Route set just read")
        .freeze()
        .expect("a request stays complete with its route shuffled");
    (shuffled, Some(hop))
}

/// Whether `uri` names the address `hop`.
fn names(uri: &Uri, hop: SocketAddr) -> bool {
    let (host, port) = uri.host_port();
    host == hop.ip().to_string() && port == hop.port()
}

/// Bind a proxy-role endpoint named `name` at `addr` and run it,
/// Record-Routing every dialog.
pub async fn spawn_stateful_proxy(h: &Harness, name: &str, addr: &str) -> StatefulProxy {
    spawn_stateful_proxy_with(h, name, addr, RecordRoute::Yes).await
}

/// [`spawn_stateful_proxy`] with the Record-Route choice stated.
pub async fn spawn_stateful_proxy_with(
    h: &Harness,
    name: &str,
    addr: &str,
    record_route: RecordRoute,
) -> StatefulProxy {
    spawn(h, name, addr, record_route, None, DialogRouting::Loose).await
}

/// [`spawn_stateful_proxy`] sending every request it forwards to `next_hop`,
/// whatever its Route and Request-URI name (§16.6 step 7).
pub async fn spawn_stateful_proxy_toward(
    h: &Harness,
    name: &str,
    addr: &str,
    next_hop: SocketAddr,
) -> StatefulProxy {
    spawn(h, name, addr, RecordRoute::Yes, Some(next_hop), DialogRouting::Loose).await
}

/// [`spawn_stateful_proxy_with`] forking every dialog-creating INVITE: one
/// branch where its Route or Request-URI names, and one to `second`, a user at
/// an address that becomes that branch's Request-URI (§16.6 step 2).
pub async fn spawn_forking_proxy(
    h: &Harness,
    name: &str,
    addr: &str,
    record_route: RecordRoute,
    second: (&str, SocketAddr),
) -> StatefulProxy {
    let agent = h.agent_with_roles(name, addr, HashSet::from([sip_net::UaRole::Proxy])).await;
    agent.drop_to_raw_wire();
    let addr = agent.addr();
    let (user, at) = second;
    let fork_to = Uri::sip(at.ip().to_string()).with_port(at.port()).with_user(user.to_string());
    let proxy = Proxy {
        agent: Some(agent),
        record_route,
        fork_to: Some(fork_to),
        routing: DialogRouting::Loose,
        ..Proxy::default()
    };
    StatefulProxy { addr, task: tokio::spawn(proxy.run()) }
}

/// Run the proxy on `agent`, an endpoint already bound with the proxy role on
/// a fabric this module does not own, sending what it forwards to `next_hop`
/// when set.
pub fn stateful_proxy_on(
    agent: Agent,
    record_route: RecordRoute,
    next_hop: Option<SocketAddr>,
) -> StatefulProxy {
    agent.drop_to_raw_wire();
    let addr = agent.addr();
    let proxy = Proxy {
        agent: Some(agent),
        record_route,
        next_hop,
        routing: DialogRouting::Loose,
        ..Proxy::default()
    };
    StatefulProxy { addr, task: tokio::spawn(proxy.run()) }
}

async fn spawn(
    h: &Harness,
    name: &str,
    addr: &str,
    record_route: RecordRoute,
    next_hop: Option<SocketAddr>,
    routing: DialogRouting,
) -> StatefulProxy {
    let agent = h.agent_with_roles(name, addr, HashSet::from([sip_net::UaRole::Proxy])).await;
    // Repeats are this proxy's transactions' to absorb or replay, not the
    // harness's.
    agent.drop_to_raw_wire();
    let addr = agent.addr();
    let proxy = Proxy { agent: Some(agent), record_route, next_hop, routing, ..Proxy::default() };
    StatefulProxy { addr, task: tokio::spawn(proxy.run()) }
}

/// [`spawn_stateful_proxy`] carrying the dialogs it Record-Routes by
/// `routing`.
pub async fn spawn_stateful_proxy_routing(
    h: &Harness,
    name: &str,
    addr: &str,
    routing: DialogRouting,
) -> StatefulProxy {
    spawn(h, name, addr, RecordRoute::Yes, None, routing).await
}

/// One server transaction: what it last sent upstream, and where.
#[derive(Default)]
struct ServerTxn {
    last_response: Option<(Vec<u8>, SocketAddr)>,
    final_status: Option<u16>,
}

/// The client transaction an INVITE server transaction opened downstream.
struct InviteClient {
    forwarded: SipRequest,
    next_hop: SocketAddr,
    final_status: Option<u16>,
}

/// A dialog-creating INVITE forwarded on several branches (§16.7).
struct Fork {
    branches: Vec<InviteClient>,
    /// A 2xx went upstream: every later non-2xx final is absorbed.
    answered: bool,
    /// The pending branches were CANCELed; none is CANCELed twice.
    cancelled: bool,
    /// The best non-2xx final received so far.
    best: Option<SipResponse>,
}

#[derive(Default)]
struct Proxy {
    agent: Option<Agent>,
    record_route: RecordRoute,
    /// Where every forwarded request goes, when set.
    next_hop: Option<SocketAddr>,
    /// By (upstream branch, method).
    server: HashMap<(String, String), ServerTxn>,
    /// By upstream branch.
    invites: HashMap<String, InviteClient>,
    /// Downstream branch → upstream branch.
    downstream: HashMap<String, String>,
    tags: u64,
    /// The second target of every dialog-creating INVITE, when forking.
    fork_to: Option<Uri>,
    /// By upstream branch.
    forks: HashMap<String, Fork>,
    /// Downstream branch of a fork → (upstream branch, branch index).
    fork_branches: HashMap<String, (String, usize)>,
    routing: DialogRouting,
}

impl Proxy {
    async fn run(mut self) {
        loop {
            match self.agent().recv_any().await {
                Ok(Inbound::Request(txn)) => self.on_request(txn.request().clone()).await,
                Ok(Inbound::Response(resp)) => self.on_response(resp).await,
                Err(StepError::Timeout { .. }) => continue,
                Err(_) => return,
            }
        }
    }

    fn agent(&self) -> &Agent {
        self.agent.as_ref().expect("the proxy runs on its endpoint")
    }

    async fn on_request(&mut self, req: SipRequest) {
        let branch = req.top_via().branch().unwrap_or_default().to_string();
        let method = req.method().as_str().to_string();
        if method == "ACK" {
            let hop_ack = self
                .server
                .get(&(branch.clone(), "INVITE".to_string()))
                .is_some_and(|t| t.final_status.is_some_and(|s| s >= 300));
            if !hop_ack && !hops_exhausted(&req) {
                self.forward(&req, &branch).await;
            }
            return;
        }
        let key = (branch.clone(), method.clone());
        if let Some(txn) = self.server.get(&key) {
            if let Some((wire, to)) = txn.last_response.clone() {
                self.send(&wire, to).await;
            }
            return;
        }
        self.server.insert(key.clone(), ServerTxn::default());
        if hops_exhausted(&req) {
            self.respond(&key, &req, 483, "Too Many Hops").await;
            return;
        }
        match method.as_str() {
            "CANCEL" if self.forks.contains_key(&branch) => {
                self.respond(&key, &req, 200, "OK").await;
                self.cancel_pending_branches(&branch, None).await;
            }
            "INVITE" if self.fork_to.is_some() && req.to().tag().is_none() => {
                self.respond(&key, &req, 100, "Trying").await;
                self.fork(&req, &branch).await;
            }
            "CANCEL" => {
                let Some(invite) = self.invites.get(&branch) else {
                    self.respond(&key, &req, 481, "Call/Transaction Does Not Exist").await;
                    return;
                };
                let pending = invite.final_status.is_none();
                let (forwarded, next_hop) = (invite.forwarded.clone(), invite.next_hop);
                self.respond(&key, &req, 200, "OK").await;
                if pending {
                    let cancel = generate_cancel(
                        &InviteClientTransactionHandle { original_invite: forwarded },
                        &[],
                    );
                    self.send(cancel.image(), next_hop).await;
                }
            }
            "INVITE" => {
                self.respond(&key, &req, 100, "Trying").await;
                let (forwarded, next_hop) = self.forward(&req, &branch).await;
                self.invites
                    .insert(branch, InviteClient { forwarded, next_hop, final_status: None });
            }
            _ => {
                self.forward(&req, &branch).await;
            }
        }
    }

    async fn on_response(&mut self, resp: SipResponse) {
        let method = resp.cseq().method().to_string();
        if method == "CANCEL" || resp.status() == 100 {
            return;
        }
        let downstream = resp.top_via().branch().unwrap_or_default().to_string();
        if let Some((up, index)) = self.fork_branches.get(&downstream).cloned() {
            self.on_fork_response(resp, up, index).await;
            return;
        }
        let upstream = self.downstream.get(&downstream).cloned();
        if let Some(up) = &upstream {
            if method == "INVITE" && resp.status() >= 300 {
                if let Some(invite) = self.invites.get_mut(up) {
                    let repeat = invite.final_status.is_some();
                    invite.final_status = Some(resp.status());
                    let (ack, next_hop) =
                        (generate_ack_for_non_2xx(&invite.forwarded, &resp, &[]), invite.next_hop);
                    self.send(ack.image(), next_hop).await;
                    if repeat {
                        return;
                    }
                }
            } else if method == "INVITE" && resp.status() >= 200 {
                if let Some(invite) = self.invites.get_mut(up) {
                    invite.final_status = Some(resp.status());
                }
            } else if resp.status() >= 200 {
                let key = (up.clone(), method.clone());
                if self.server.get(&key).is_some_and(|t| t.final_status.is_some()) {
                    return;
                }
            }
        }
        let hop = self.agent().addr();
        let forwarded = forwarded_response(&resp, hop);
        let to = response_next_hop(&forwarded).expect("the next Via names an address");
        self.send(forwarded.image(), to).await;
        if let Some(up) = upstream {
            let txn = self.server.entry((up, method)).or_default();
            txn.last_response = Some((forwarded.image().to_vec(), to));
            if resp.status() >= 200 {
                txn.final_status = Some(resp.status());
            }
        }
    }

    /// Forward a dialog-creating INVITE received on `branch` on two branches:
    /// where it names, and to the second target.
    async fn fork(&mut self, req: &SipRequest, branch: &str) {
        let fork_to = self.fork_to.clone().expect("a forking proxy has a second target");
        let mut branches = Vec::new();
        for (index, retarget) in [None, Some(fork_to)].into_iter().enumerate() {
            let agent = self.agent();
            let derived = format!(
                "z9hG4bK-{}-{}-{index}",
                agent.name(),
                branch.trim_start_matches("z9hG4bK")
            );
            let mut forwarded = forwarded_request(req, agent.addr(), &derived);
            if self.record_route == RecordRoute::No {
                forwarded = forwarded
                    .thaw()
                    .pop_top::<RecordRouteEntry>()
                    .expect("the Record-Route this hop just added pops")
                    .freeze()
                    .expect("a request stays complete without its Record-Route");
            }
            if let Some(uri) = retarget {
                forwarded = forwarded
                    .thaw()
                    .with_uri(uri)
                    .freeze()
                    .expect("a request stays complete under a new Request-URI");
            }
            let next_hop = match (index, self.next_hop) {
                (0, Some(fixed)) => fixed,
                _ => request_next_hop(&forwarded).expect("the next hop names an address"),
            };
            self.send(forwarded.image(), next_hop).await;
            self.fork_branches.insert(derived, (branch.to_string(), index));
            branches.push(InviteClient { forwarded, next_hop, final_status: None });
        }
        self.forks.insert(
            branch.to_string(),
            Fork { branches, answered: false, cancelled: false, best: None },
        );
    }

    /// CANCEL every branch of the fork opened by `upstream` that has no final
    /// yet, but `except` (§16.10, §16.7 step 10), once per fork.
    async fn cancel_pending_branches(&mut self, upstream: &str, except: Option<usize>) {
        let Some(fork) = self.forks.get_mut(upstream) else { return };
        if std::mem::replace(&mut fork.cancelled, true) {
            return;
        }
        let pending: Vec<(SipRequest, SocketAddr)> = fork
            .branches
            .iter()
            .enumerate()
            .filter(|(i, b)| Some(*i) != except && b.final_status.is_none())
            .map(|(_, b)| (b.forwarded.clone(), b.next_hop))
            .collect();
        for (forwarded, next_hop) in pending {
            let cancel =
                generate_cancel(&InviteClientTransactionHandle { original_invite: forwarded }, &[]);
            self.send(cancel.image(), next_hop).await;
        }
    }

    /// A response on branch `index` of the fork opened by `upstream` (§16.7).
    async fn on_fork_response(&mut self, resp: SipResponse, upstream: String, index: usize) {
        let status = resp.status();
        let mut forward = None;
        if status >= 300 {
            let fork = self.forks.get_mut(&upstream).expect("a fork branch has its fork");
            let branch = &mut fork.branches[index];
            let ack = generate_ack_for_non_2xx(&branch.forwarded, &resp, &[]);
            let (repeat, next_hop) = (branch.final_status.is_some(), branch.next_hop);
            branch.final_status = Some(status);
            self.send(ack.image(), next_hop).await;
            if repeat {
                return;
            }
            let fork = self.forks.get_mut(&upstream).expect("a fork branch has its fork");
            if fork.best.as_ref().is_none_or(|best| better_final(status, best.status())) {
                fork.best = Some(resp);
            }
            let all_final = fork.branches.iter().all(|b| b.final_status.is_some());
            if !fork.answered && all_final {
                forward = fork.best.clone();
            }
        } else if status >= 200 {
            let fork = self.forks.get_mut(&upstream).expect("a fork branch has its fork");
            fork.branches[index].final_status = Some(status);
            let first = !fork.answered;
            fork.answered = true;
            forward = Some(resp);
            if first {
                self.cancel_pending_branches(&upstream, Some(index)).await;
            }
        } else {
            forward = Some(resp);
        }
        let Some(resp) = forward else { return };
        let hop = self.agent().addr();
        let forwarded = forwarded_response(&resp, hop);
        let to = response_next_hop(&forwarded).expect("the next Via names an address");
        self.send(forwarded.image(), to).await;
        let txn = self.server.entry((upstream, "INVITE".to_string())).or_default();
        txn.last_response = Some((forwarded.image().to_vec(), to));
        if forwarded.status() >= 200 {
            txn.final_status = Some(forwarded.status());
        }
    }

    /// Forward `req` (received on `branch`) downstream on the branch derived
    /// from it; returns the forwarded request and its next hop.
    async fn forward(&mut self, req: &SipRequest, branch: &str) -> (SipRequest, SocketAddr) {
        let agent = self.agent();
        let derived = format!("z9hG4bK-{}-{}", agent.name(), branch.trim_start_matches("z9hG4bK"));
        let req = &self.routing.received(req, agent.addr());
        let mut forwarded = forwarded_request(req, agent.addr(), &derived);
        // `forwarded_request` Record-Routes a dialog-creating INVITE with this
        // hop as the topmost entry.
        if self.record_route == RecordRoute::No
            && req.method().as_str() == "INVITE"
            && req.to().tag().is_none()
        {
            forwarded = forwarded
                .thaw()
                .pop_top::<RecordRouteEntry>()
                .expect("the Record-Route this hop just added pops")
                .freeze()
                .expect("a request stays complete without its Record-Route");
        }
        if self.record_route == RecordRoute::Yes {
            forwarded = self.routing.record_routed(forwarded, agent.addr());
        }
        let (forwarded, strict_hop) = to_strict_next_hop(forwarded);
        let next_hop = self.next_hop.or(strict_hop).unwrap_or_else(|| {
            request_next_hop(&forwarded).expect("the next hop names an address")
        });
        let forwarded = match strict_hop {
            Some(_) => forwarded,
            None => self.routing.retargeted(forwarded, next_hop),
        };
        self.send(forwarded.image(), next_hop).await;
        if req.method().as_str() != "ACK" {
            self.downstream.insert(derived, branch.to_string());
        }
        (forwarded, next_hop)
    }

    /// Answer `req` from this hop and keep the answer for its repeats.
    async fn respond(
        &mut self,
        key: &(String, String),
        req: &SipRequest,
        status: u16,
        reason: &str,
    ) {
        let to_tag = (status > 100).then(|| {
            self.tags += 1;
            format!("{}-tag-{}", self.agent().name(), self.tags)
        });
        let resp = generate_response(
            req,
            status,
            reason,
            &GenerateResponseOpts { to_tag, ..Default::default() },
        );
        let to = response_next_hop(&resp).expect("the top Via names an address");
        self.send(resp.image(), to).await;
        let txn = self.server.entry(key.clone()).or_default();
        txn.last_response = Some((resp.image().to_vec(), to));
        if status >= 200 {
            txn.final_status = Some(status);
        }
    }

    async fn send(&self, wire: &[u8], to: SocketAddr) {
        self.agent().try_send_datagram(wire, to).await.expect("the fabric takes the datagram");
    }
}

/// Whether non-2xx final `status` beats `best` for the response a forking
/// proxy sends upstream (§16.7 step 6): a 6xx first, else the lowest class.
fn better_final(status: u16, best: u16) -> bool {
    match (status / 100 == 6, best / 100 == 6) {
        (true, false) => true,
        (false, true) => false,
        _ => status / 100 < best / 100,
    }
}
