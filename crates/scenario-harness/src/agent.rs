//! The fluent, dialog-aware harness — the auto-generating DSL (port of the
//! load-bearing half of `recorder.ts`'s `AgentProxy` / `DialogRef` + the dialog
//! state in `message-builder.ts`).
//!
//! This is the layer that means a scenario does **not** hand-author headers.
//! Agents are stateful UAs; each high-level call generates a correct-by-default
//! B2B message via `sip_message::generators` and tracks the dialog state needed
//! for the next one:
//!
//! ```ignore
//! let h = Harness::new("alice-calls-bob");
//! let alice = h.agent("alice", "127.0.0.1:5060").await;
//! let bob   = h.agent("bob",   "127.0.0.1:5070").await;
//!
//! let mut call = alice.invite(&bob).with_sdp(OFFER).send().await; // INVITE auto-built
//! let mut uas  = bob.receive("INVITE").await;
//! uas.respond(180, "Ringing").await;                              // To-tag minted here
//! call.expect(180).await;                                         // learns remote tag/target
//! uas.respond(200, "OK").with_sdp(ANSWER).await;
//! call.expect(200).await;
//! let mut dialog = call.ack().await;                              // ACK reuses INVITE CSeq
//! bob.receive("ACK").await;
//! let mut bye = dialog.bye().await;                               // BYE auto-increments CSeq
//! bob.receive("BYE").await.respond(200, "OK").await;
//! bye.expect(200).await;
//! let report = h.finish().await;                                  // render from the recording
//! ```
//!
//! What the harness fills in automatically, per RFC 3261: Via (fresh branch per
//! transaction, magic cookie), From/To with tags, Call-ID continuity, CSeq
//! numbering (1 INVITE → 1 ACK → n BYE; responses echo), Contact, Max-Forwards,
//! Content-Type/Length, remote-target routing (in-dialog requests go to the
//! peer's Contact). Everything still flows through the recording-wrapped
//! `SignalingNetwork`, so the reports are projected from the record exactly as
//! before — the auto-generation only changes *who writes the bytes*.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{NetworkTag, Recorder, RunContext, TransportKind};
use sip_clock::Clock;
use sip_message::generators::{
    generate_ack_for_2xx, generate_in_dialog_request, generate_out_of_dialog_request,
    generate_response, strip_route_uri_to_request_uri, ContactSpec, GenerateAckFor2xxOpts,
    GenerateInDialogRequestOpts, GenerateOutOfDialogRequestOpts, GenerateResponseOpts,
    InDialogMethod, InviteClientTransactionHandle, OutOfDialogMethod, SipTransport, StackDialog,
    ViaSpec,
};
use sip_message::message_helpers::{get_header, get_headers};
use sip_message::parser::custom::CustomParser;
use sip_message::{serialize, SipHeader, SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::{
    with_all_contracts, BindUdpOpts, ScopedAuditOptions, SignalingNetwork, SimulatedSignalingNetwork,
    UdpEndpoint,
};

use crate::run::RunReport;

const RECV_TIMEOUT: Duration = Duration::from_secs(2);

/// Monotonic id source for branches / tags / Call-IDs. Deterministic (no RNG),
/// so report bytes are stable across runs.
struct Ids(AtomicU64);
impl Ids {
    fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

/// A running fluent session: owns the recording-wrapped simulated network and
/// hands out [`Agent`]s. Drop or [`finish`](Harness::finish) when done.
pub struct Harness {
    network: Arc<dyn SignalingNetwork>,
    recording: sip_net::RecordingSignalingNetwork,
    recorder: Recorder,
    ids: Arc<Ids>,
    name: String,
    description: Option<String>,
}

impl Harness {
    /// Start a session named `scenario_name`. The simulated fabric uses the
    /// default [`crate::SIMULATED_TRANSIT_DELAY_MS`] one-hop transit delay, so a
    /// sent datagram arrives that much later (mirrors a real network).
    /// Timestamps ride a test clock so a paused runtime gives deterministic
    /// report times (see `run::run`).
    pub fn new(scenario_name: impl Into<String>) -> Self {
        Self::with_transit_delay(scenario_name, crate::SIMULATED_TRANSIT_DELAY_MS)
    }

    /// Like [`new`](Harness::new) but with an explicit one-hop transit delay
    /// (ms). Use `0` for instant delivery (e.g. when a test asserts exact,
    /// transit-free timestamps).
    pub fn with_transit_delay(scenario_name: impl Into<String>, transit_delay_ms: u64) -> Self {
        let recorder = Recorder::with_clock(TransportKind::Fake, Clock::test_at(0));
        let sim = Arc::new(SimulatedSignalingNetwork::new(transit_delay_ms));
        let wrapped = with_all_contracts(
            sim,
            recorder.clone(),
            RunContext::TestWithRecorder,
            ScopedAuditOptions::default(),
            true,
        );
        Self {
            network: wrapped.network,
            recording: wrapped.recording,
            recorder,
            ids: Arc::new(Ids(AtomicU64::new(1))),
            name: scenario_name.into(),
            description: None,
        }
    }

    /// Set the report description (port of `.describe(...)`).
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Declare and bind a named UA at `addr` (e.g. `"127.0.0.1:5060"`).
    pub async fn agent(&self, name: impl Into<String>, addr: &str) -> Agent {
        let name = name.into();
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        self.recorder.register_lane(addr, name.clone(), NetworkTag::Ext);
        let ep = self
            .network
            .bind_udp(BindUdpOpts::new(addr, 64))
            .await
            .unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        Agent {
            name: name.clone(),
            addr,
            uri: format!("sip:{name}@{}", addr.ip()),
            ep: Arc::from(ep),
            ids: self.ids.clone(),
        }
    }

    /// Declare and bind a record-routing proxy / load balancer at `addr`.
    pub async fn proxy(&self, name: impl Into<String>, addr: &str) -> Proxy {
        Proxy {
            agent: self.agent(name, addr).await,
        }
    }

    /// Bind a **System-Under-Test** endpoint on the shared, recording-wrapped
    /// fabric and register a `Core` lane for it. Returns the raw
    /// [`UdpEndpoint`] + its bound address so a real SUT (e.g. `sip-proxy`'s
    /// `ProxyCore`) can run its own recv loop against the same network the
    /// agents use — every `send_to`/`recv` still flows through the recorder, so
    /// the recording remains the trace. The caller owns the spawned loop (abort
    /// it on drop). This is the seam that lets the harness drive a real proxy,
    /// not just peer-to-peer agents (ADR-0006 → ADR-0009).
    pub async fn bind_sut(&self, name: impl Into<String>, addr: &str) -> (Box<dyn UdpEndpoint>, SocketAddr) {
        let name = name.into();
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        self.recorder.register_lane(addr, name, NetworkTag::Core);
        let ep = self
            .network
            .bind_udp(BindUdpOpts::new(addr, 256))
            .await
            .unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        (ep, addr)
    }

    /// Advance virtual time by `d` (requires a paused runtime —
    /// `#[tokio::test(start_paused = true)]`). Advances in 100 ms chunks,
    /// mirroring the source's `TestClock.adjust` loop so in-flight delivery
    /// tasks observe intermediate values. Because the report's `at_ms` rides
    /// the same tokio clock (via `sip-clock`), the elapsed time shows up in the
    /// rendered timestamps. Call it *between* protocol events (after the message
    /// just sent has been `expect`ed) so each message keeps a clean send/receive
    /// timestamp.
    pub async fn advance(&self, d: Duration) {
        sip_clock::testkit::advance_in_100ms_chunks(d).await;
    }

    /// Close the recording layer and return the [`RunReport`] (trace projected
    /// from the recording). Failures in the fluent flow panic in-line, so a
    /// returned report is by construction a passing run.
    pub async fn finish(self) -> RunReport {
        let events = self.recording.channel().snapshot();
        let audit = self.recording.close().await;
        RunReport::from_recording(self.name, self.description, self.recorder, events, audit)
    }
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// A stateful fake UA. Cheap to clone (shares the endpoint + id source); the
/// dialog state lives on the per-transaction handles it returns, not here.
#[derive(Clone)]
pub struct Agent {
    name: String,
    addr: SocketAddr,
    /// Dialog URI (`sip:name@ip`, no port) — used for From/To.
    uri: String,
    ep: Arc<dyn UdpEndpoint>,
    ids: Arc<Ids>,
}

impl Agent {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn branch(&self) -> String {
        format!("z9hG4bK-{}-{}", self.name, self.ids.next())
    }
    fn tag(&self) -> String {
        format!("{}-tag-{}", self.name, self.ids.next())
    }
    fn via(&self) -> ViaSpec {
        ViaSpec {
            local_ip: self.addr.ip().to_string(),
            local_port: self.addr.port(),
            transport: SipTransport::Udp,
            branch: self.branch(),
            custom_params: vec![],
        }
    }
    fn contact(&self) -> ContactSpec {
        ContactSpec {
            user: self.name.clone(),
            host: self.addr.ip().to_string(),
            port: self.addr.port(),
            uri_params: vec![],
        }
    }

    async fn send(&self, msg: &SipMessage, dst: SocketAddr) {
        self.ep
            .send_to(&serialize(msg), dst)
            .await
            .unwrap_or_else(|e| panic!("{} send failed: {e}", self.name));
    }

    async fn recv(&self) -> SipMessage {
        let pkt = tokio::time::timeout(RECV_TIMEOUT, self.ep.recv())
            .await
            .unwrap_or_else(|_| panic!("{} timed out waiting for a datagram", self.name))
            .unwrap_or_else(|| panic!("{} endpoint queue closed", self.name));
        CustomParser::new()
            .parse(&pkt.raw)
            .unwrap_or_else(|e| panic!("{} received an unparseable datagram: {e}", self.name))
    }

    /// Begin an out-of-dialog INVITE to `peer`. Returns a builder; call
    /// [`Invite::send`] (optionally after [`Invite::with_sdp`] / [`Invite::through`]).
    pub fn invite<'a>(&'a self, peer: &'a Agent) -> Invite<'a> {
        Invite {
            caller: self,
            peer,
            sdp: None,
            wire_dst: None,
        }
    }

    /// Receive the next request and assert its method. Returns a UAS-side
    /// transaction handle for sending responses.
    pub async fn receive(&self, method: &str) -> ServerTxn {
        match self.recv().await {
            SipMessage::Request(r) => {
                assert!(
                    r.method.eq_ignore_ascii_case(method),
                    "{} expected a {method} request, got {}",
                    self.name,
                    r.method
                );
                // UAS route set (§12.1.1): the request's Record-Route in
                // received order. Used if this UAS later originates in-dialog
                // requests (e.g. bob sends the BYE).
                let route_set = get_headers(&r.headers, "record-route")
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                ServerTxn {
                    agent: self.clone(),
                    request: r,
                    to_tag: None,
                    route_set,
                }
            }
            SipMessage::Response(r) => panic!(
                "{} expected a {method} request, got a {} {} response",
                self.name, r.status, r.reason
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Outgoing INVITE builder + client transaction
// ---------------------------------------------------------------------------

/// Builder for an outgoing INVITE (lets the SDP offer be attached fluently).
pub struct Invite<'a> {
    caller: &'a Agent,
    peer: &'a Agent,
    sdp: Option<String>,
    /// Wire destination override — the INVITE is *addressed* to `peer` (its
    /// Contact is the Request-URI) but *sent* here. Set by [`Invite::through`]
    /// to route an initial INVITE via a proxy/LB.
    wire_dst: Option<SocketAddr>,
}

impl<'a> Invite<'a> {
    /// Attach an SDP offer body.
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.sdp = Some(sdp.to_string());
        self
    }

    /// Send the initial INVITE to `proxy` instead of directly to the peer (the
    /// Request-URI still targets the peer). Used to drive an LB/record-routing
    /// proxy; subsequent in-dialog requests then follow the route set learned
    /// from the proxy's Record-Route automatically.
    pub fn through(mut self, proxy: SocketAddr) -> Self {
        self.wire_dst = Some(proxy);
        self
    }

    /// Generate the INVITE (all headers filled in), send it, and return the
    /// client transaction handle.
    pub async fn send(self) -> ClientInvite {
        let caller = self.caller;
        let peer = self.peer;
        let wire_dst = self.wire_dst.unwrap_or(peer.addr);
        let call_id = format!("{}-{}@{}", caller.name, caller.ids.next(), caller.addr.ip());
        let from_tag = caller.tag();
        let request_uri = format!("sip:{}@{}:{}", peer.name, peer.addr.ip(), peer.addr.port());

        let opts = GenerateOutOfDialogRequestOpts {
            request_uri: request_uri.clone(),
            call_id: call_id.clone(),
            from_uri: caller.uri.clone(),
            from_tag: from_tag.clone(),
            to_uri: peer.uri.clone(),
            to_tag: None,
            cseq: 1,
            via: Some(caller.via()),
            contact: Some(caller.contact()),
            max_forwards: Some(70),
            body: self.sdp.as_deref().map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            content_type: None,
            extra_headers: vec![],
        };
        let invite = generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts);
        caller.send(&SipMessage::Request(invite.clone()), wire_dst).await;

        let dialog = StackDialog {
            call_id,
            local_tag: from_tag,
            remote_tag: String::new(),
            local_uri: caller.uri.clone(),
            remote_uri: peer.uri.clone(),
            remote_target: request_uri,
            local_cseq: 1,
            route_set: vec![],
        };
        ClientInvite {
            agent: caller.clone(),
            fallback_addr: peer.addr,
            original_invite: invite,
            dialog,
        }
    }
}

/// UAC-side INVITE client transaction + the dialog it is establishing.
pub struct ClientInvite {
    agent: Agent,
    /// Where to send the ACK if no Contact was learned (shouldn't happen for a
    /// well-behaved 2xx, but keeps the harness robust).
    fallback_addr: SocketAddr,
    original_invite: SipRequest,
    dialog: StackDialog,
}

impl ClientInvite {
    /// Wait for and assert a response status. Learns the remote tag (from the
    /// first tagged response) and the remote target (from Contact), so the
    /// later ACK/BYE route and address correctly. Returns the response.
    pub async fn expect(&mut self, status: u16) -> SipResponse {
        let resp = expect_response(&self.agent, status).await;
        if self.dialog.remote_tag.is_empty() {
            if let Some(tag) = &resp.to.tag {
                self.dialog.remote_tag = tag.clone();
            }
        }
        if let Some(target) = first_contact_uri(&resp) {
            self.dialog.remote_target = target;
        }
        // Build the dialog route set from the response's Record-Route, REVERSED
        // (UAC, RFC 3261 §12.1.2), once — so a later 200 doesn't re-seed it.
        if self.dialog.route_set.is_empty() {
            let rr = get_headers(&resp.headers, "record-route");
            if !rr.is_empty() {
                self.dialog.route_set = rr.iter().rev().map(|s| s.to_string()).collect();
            }
        }
        resp
    }

    /// Generate and send the ACK for the 2xx (CSeq reused from the INVITE per
    /// RFC 3261 §13.2.2.4), then return the confirmed [`Dialog`]. With a route
    /// set the ACK carries Route headers and goes to the first hop (the proxy).
    pub async fn ack(&mut self) -> Dialog {
        let handle = InviteClientTransactionHandle {
            original_invite: self.original_invite.clone(),
        };
        let opts = GenerateAckFor2xxOpts {
            via: Some(self.agent.via()),
            ..Default::default()
        };
        let ack = generate_ack_for_2xx(Some(&handle), &self.dialog, &opts);
        let dst = next_hop(&self.dialog, self.fallback_addr);
        self.agent.send(&SipMessage::Request(ack), dst).await;
        Dialog {
            agent: self.agent.clone(),
            fallback_addr: dst,
            dialog: self.dialog.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Confirmed dialog (in-dialog requests)
// ---------------------------------------------------------------------------

/// A confirmed dialog. In-dialog requests auto-increment CSeq and route to the
/// remote target.
pub struct Dialog {
    agent: Agent,
    fallback_addr: SocketAddr,
    dialog: StackDialog,
}

impl Dialog {
    /// Send a BYE (CSeq auto-incremented). Returns its client transaction.
    pub async fn bye(&mut self) -> InDialogTxn {
        self.request(InDialogMethod::Bye, None).await
    }

    /// Send any in-dialog request (re-INVITE, INFO, …); attach an SDP body
    /// with `sdp`.
    pub async fn request(&mut self, method: InDialogMethod, sdp: Option<&str>) -> InDialogTxn {
        let opts = GenerateInDialogRequestOpts {
            via: Some(self.agent.via()),
            contact: Some(self.agent.contact()),
            body: sdp.map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            ..Default::default()
        };
        let res = generate_in_dialog_request(method, &self.dialog, &opts);
        self.dialog = res.dialog; // local_cseq bumped
        let dst = next_hop(&self.dialog, self.fallback_addr);
        self.agent.send(&SipMessage::Request(res.request), dst).await;
        InDialogTxn {
            agent: self.agent.clone(),
        }
    }
}

/// Client transaction for an in-dialog request.
pub struct InDialogTxn {
    agent: Agent,
}

impl InDialogTxn {
    /// Wait for and assert a response status.
    pub async fn expect(&mut self, status: u16) -> SipResponse {
        expect_response(&self.agent, status).await
    }
}

// ---------------------------------------------------------------------------
// UAS-side server transaction
// ---------------------------------------------------------------------------

/// UAS-side transaction for a received request. `respond` echoes Via/From/To/
/// Call-ID/CSeq and mints a stable To-tag on the first non-100 response.
pub struct ServerTxn {
    agent: Agent,
    request: SipRequest,
    to_tag: Option<String>,
    route_set: Vec<String>,
}

impl ServerTxn {
    /// The received request (for inspecting headers / SDP).
    pub fn request(&self) -> &SipRequest {
        &self.request
    }

    /// Send a response. Returns a builder for attaching an SDP answer.
    pub fn respond(&mut self, status: u16, reason: &str) -> Respond<'_> {
        Respond {
            txn: self,
            status,
            reason: reason.to_string(),
            sdp: None,
        }
    }

    /// Form the UAS-side confirmed [`Dialog`] for this transaction, so this UA
    /// can originate in-dialog requests (e.g. the callee sends the BYE). Call
    /// after responding 2xx (so the To-tag is minted). The remote target is the
    /// caller's Contact; the route set is the request's Record-Route in order
    /// (§12.1.1), so in-dialog requests route back through any proxy.
    pub fn dialog(&self) -> Dialog {
        let req = &self.request;
        let local_tag = self.to_tag.clone().unwrap_or_default();
        let remote_target = get_header(&req.headers, "contact")
            .map(unwrap_angle)
            .unwrap_or_else(|| req.from.uri.clone());
        let dialog = StackDialog {
            call_id: req.call_id.clone(),
            local_tag,
            remote_tag: req.from.tag.clone().unwrap_or_default(),
            // From the UAS's view, "local" is itself and "remote" is the caller.
            local_uri: self.agent.uri.clone(),
            remote_uri: req.from.uri.clone(),
            remote_target,
            local_cseq: 0, // UAS originates its own CSeq space; first request → 1
            route_set: self.route_set.clone(),
        };
        let fallback = next_hop(&dialog, top_via_addr(req).unwrap_or(self.agent.addr));
        Dialog {
            agent: self.agent.clone(),
            fallback_addr: fallback,
            dialog,
        }
    }
}

/// Builder for a UAS response (lets an SDP answer be attached fluently).
pub struct Respond<'a> {
    txn: &'a mut ServerTxn,
    status: u16,
    reason: String,
    sdp: Option<String>,
}

impl<'a> Respond<'a> {
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.sdp = Some(sdp.to_string());
        self
    }

    /// Generate and send the response.
    pub async fn send(self) {
        let txn = self.txn;
        if self.status > 100 && txn.to_tag.is_none() {
            txn.to_tag = Some(txn.agent.tag());
        }
        // Contact is required on 2xx and useful on 18x to establish the early
        // dialog's remote target; omit on plain 100.
        let contact = if self.status >= 180 {
            Some(txn.agent.contact())
        } else {
            None
        };
        let opts = GenerateResponseOpts {
            to_tag: txn.to_tag.clone(),
            contact,
            body: self.sdp.as_deref().map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            content_type: None,
            extra_headers: vec![],
            incoming_source: None,
        };
        let resp = generate_response(&txn.request, self.status, &self.reason, &opts);
        // Responses are routed by Via, not Route (RFC 3261 §18.2.2): send to the
        // request's topmost Via sent-by. With a proxy in the path that Via is
        // the proxy's, so the response correctly traverses it back.
        let dst = top_via_addr(&txn.request).unwrap_or(txn.agent.addr);
        txn.agent.send(&SipMessage::Response(resp), dst).await;
    }
}

/// Allow `respond(...).await` directly (no explicit `.send()`), by making the
/// builder awaitable.
impl<'a> std::future::IntoFuture for Respond<'a> {
    type Output = ();
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}

// ---------------------------------------------------------------------------
// Proxy / load balancer (loose-routing, Record-Route)
// ---------------------------------------------------------------------------

/// A minimal loose-routing proxy — the test stand-in for the LB front proxy
/// (port target: `sip-front-proxy`). It does the load-bearing routing surgery
/// per RFC 3261 §16:
///   - adds its own **Via** (top) to forwarded requests so responses route back
///     through it (§16.6), and strips that Via from responses (§16.7);
///   - inserts a `;lr` **Record-Route** (top) on dialog-creating INVITEs so both
///     peers route in-dialog requests through it (§16.6.4);
///   - strips its own top **Route** from in-dialog requests it is the loose
///     router for (§16.4) before forwarding.
///
/// It is *stateless* and *scripted*: the test says which way to forward each
/// message (the real proxy resolves the next hop from the top Route / RURI).
#[derive(Clone)]
pub struct Proxy {
    agent: Agent,
}

impl Proxy {
    pub fn addr(&self) -> SocketAddr {
        self.agent.addr
    }
    pub fn name(&self) -> &str {
        &self.agent.name
    }

    fn record_route_value(&self) -> String {
        format!("<sip:{}:{};lr>", self.agent.addr.ip(), self.agent.addr.port())
    }

    /// Receive one request, apply the §16 surgery, and forward it to `next`.
    /// Returns the (rewritten) request for assertions.
    pub async fn forward_request(&self, next: SocketAddr) -> SipRequest {
        let SipMessage::Request(mut req) = self.agent.recv().await else {
            panic!("{} expected a request to forward", self.agent.name);
        };
        // Loose router popping itself off the route set (§16.4) — in-dialog
        // requests (ACK/BYE/…) arrive with our Record-Route as the top Route.
        strip_top_route_if_self(&mut req, self.agent.addr);
        // Record-Route dialog-creating requests so in-dialog traffic returns
        // through us (§16.6.4). A dialog-creating INVITE has no To-tag yet.
        if req.method.eq_ignore_ascii_case("INVITE") && req.to.tag.is_none() {
            prepend_header(&mut req.headers, "Record-Route", &self.record_route_value());
        }
        // Add our Via on top so the response comes back to us (§16.6).
        prepend_header(&mut req.headers, "Via", &self.via_value());
        self.agent.send(&SipMessage::Request(req.clone()), next).await;
        req
    }

    /// Receive one response, strip our Via, and forward it to `next`.
    pub async fn forward_response(&self, next: SocketAddr) -> SipResponse {
        let SipMessage::Response(mut resp) = self.agent.recv().await else {
            panic!("{} expected a response to forward", self.agent.name);
        };
        strip_top_via_if_self(&mut resp.headers, self.agent.addr);
        self.agent.send(&SipMessage::Response(resp.clone()), next).await;
        resp
    }

    fn via_value(&self) -> String {
        format!(
            "SIP/2.0/UDP {}:{};branch={}",
            self.agent.addr.ip(),
            self.agent.addr.port(),
            self.agent.branch()
        )
    }
}

/// Insert a header at the top of the list (RFC 3261 §16.6 prepend semantics for
/// Via / Record-Route).
fn prepend_header(headers: &mut Vec<SipHeader>, name: &str, value: &str) {
    headers.insert(
        0,
        SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        },
    );
}

/// Strip the first Route header if it routes to `me` (the loose router removing
/// itself, §16.4).
fn strip_top_route_if_self(req: &mut SipRequest, me: SocketAddr) {
    if let Some(pos) = req
        .headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("route"))
    {
        let uri = strip_route_uri_to_request_uri(&req.headers[pos].value);
        if uri_to_addr(&uri) == Some(me) {
            req.headers.remove(pos);
        }
    }
}

/// Strip the topmost Via if it is `me`'s (the proxy removing its own Via from a
/// response before forwarding upstream, §16.7).
fn strip_top_via_if_self(headers: &mut Vec<SipHeader>, me: SocketAddr) {
    if let Some(pos) = headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("via"))
    {
        let sent_by = headers[pos]
            .value
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.split(';').next())
            .map(str::trim);
        if let Some(addr) = sent_by.and_then(hostport_to_addr) {
            if addr == me {
                headers.remove(pos);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn expect_response(agent: &Agent, status: u16) -> SipResponse {
    match agent.recv().await {
        SipMessage::Response(r) => {
            assert_eq!(
                r.status, status,
                "{} expected a {status} response, got {} {}",
                agent.name, r.status, r.reason
            );
            r
        }
        SipMessage::Request(r) => panic!(
            "{} expected a {status} response, got a {} request",
            agent.name, r.method
        ),
    }
}

/// Unwrap a `<uri>` name-addr / Route value to its bare URI (params after `>`
/// dropped); a bare URI passes through trimmed.
fn unwrap_angle(value: &str) -> String {
    let t = value.trim();
    match (t.find('<'), t.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => t[a + 1..b].to_string(),
        _ => t.to_string(),
    }
}

/// The first Contact URI on a response, unwrapped from `<...>`. Used to learn
/// the dialog remote target.
fn first_contact_uri(resp: &SipResponse) -> Option<String> {
    get_header(&resp.headers, "contact").map(unwrap_angle)
}

/// Resolve a SIP URI to a socket address (default port 5060, IPv4 fixtures
/// only). Handles `sip:user@host:port`, the userless `sip:host:port;lr` form
/// of a Route/Record-Route URI, and a bare `host:port`.
fn uri_to_addr(uri: &str) -> Option<SocketAddr> {
    let no_scheme = uri
        .strip_prefix("sips:")
        .or_else(|| uri.strip_prefix("sip:"))
        .unwrap_or(uri);
    // Host part is whatever follows the last '@' (none → the whole thing).
    let host_part = no_scheme.rsplit('@').next()?;
    let host_port = host_part.split([';', '?']).next()?.trim();
    hostport_to_addr(host_port)
}

/// Parse a bare `host:port` (or `host`, default port 5060) to a socket address.
fn hostport_to_addr(host_port: &str) -> Option<SocketAddr> {
    if let Ok(sa) = host_port.parse::<SocketAddr>() {
        return Some(sa);
    }
    format!("{host_port}:5060").parse().ok()
}

/// The wire destination for an in-dialog request: the first hop in the route
/// set (the proxy) when present, else the dialog's remote target. For both
/// loose and strict routing the next hop is the address of `route_set[0]`'s
/// URI; with no route set it is the remote target.
fn next_hop(dialog: &StackDialog, fallback: SocketAddr) -> SocketAddr {
    if let Some(top) = dialog.route_set.first() {
        if let Some(addr) = uri_to_addr(&strip_route_uri_to_request_uri(top)) {
            return addr;
        }
    }
    uri_to_addr(&dialog.remote_target).unwrap_or(fallback)
}

/// The address a response to `req` must be sent to: the topmost Via's sent-by
/// (RFC 3261 §18.2.2). (`received=`/`rport=` are not stamped by this harness's
/// `generate_response`, so the sent-by host:port is authoritative here.)
fn top_via_addr(req: &SipRequest) -> Option<SocketAddr> {
    let via = get_header(&req.headers, "via")?;
    // "SIP/2.0/UDP host:port;branch=…" → take the token after the transport,
    // before the first ';'.
    let after_transport = via.split_whitespace().nth(1)?;
    let sent_by = after_transport.split(';').next()?.trim();
    hostport_to_addr(sent_by)
}
