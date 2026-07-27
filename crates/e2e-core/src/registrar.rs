//! In-process **register-based front proxy** — a faithful Rust mimic of sipjs's
//! `src/sip-front-proxy/` registrar mode (`Registrar.ts`, `RegisterStrategy.ts`,
//! `CoreToExtRoutingStrategy.ts`), brought into the e2e harness as an autonomous
//! SUT task analogous to [`FakeLsbcB2bua`](crate::infra::FakeLsbcB2bua).
//!
//! The deployed Rust `sip-proxy` `ProxyCore` does **not** implement REGISTER —
//! it is the single-endpoint K8s-LB binary and explicitly defers the registrar
//! path (`sip-proxy/src/lib.rs`: "Deferred: the SIP registrar/REGISTER path";
//! `core/mod.rs`: "the dual-fabric registrar mode is out of scope"). So, exactly
//! as the task brief permits, the mimic runs **in-process** on the harness fabric
//! rather than being forced onto the cluster proxy.
//!
//! What it faithfully reproduces from sipjs:
//!   - **Binding key = To/From URI userpart, lowercased** (`Registrar.ts` v1
//!     "userpart-only AOR key … lower-cased … host part is intentionally
//!     ignored"). Single binding per AOR, last-write-wins.
//!   - **Lazy TTL on the Effect/test clock** — every `lookup`/`register` sweeps
//!     the entry against `Clock.now_ms`; no background sweeper, so `TestClock`
//!     (here the paused tokio clock via [`Clock::now_ms`]) deterministically
//!     expires a binding. (`Registrar.ts` "lazy TTL on Effect Clock".)
//!   - **Effective Expires precedence** — `Expires` header › Contact `;expires`
//!     param › default 3600 s; `0` de-registers (`RegisterStrategy.ts`
//!     `computeEffectiveExpires` / `DEFAULT_EXPIRES_SEC`).
//!   - **Contact stored verbatim** (RFC 3261 §10.3); 200 OK echoes the granted
//!     Contact + Expires (`RegisterStrategy.ts`).
//!   - **AOR → Contact routing for an inbound INVITE** — the Request-URI userpart
//!     is looked up; a live binding forwards the request to the registered
//!     Contact's host:port, a missing/expired one is `404 Not Found`
//!     (`CoreToExtRoutingStrategy.ts` `registrarLookupLayer`).
//!
//! Deliberate deviations (all out of sipjs v1 scope too, so this is parity):
//!   - No forking / multiple contacts, no Path, no auth, no 423 Min-Expires.
//!   - Single fabric (not the ext/core dual-endpoint of `RegistrarProxyConfig`):
//!     the harness fabric is one network, so REGISTER and the INVITE arrive on
//!     the same bind. The routing semantics (AOR lookup → Contact) are identical.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::{
    Contact, Expires, HeaderName, HeaderValue, MaxForwards, ParamValue, RecordRouteEntry,
    RouteEntry, Uri, Via,
};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipHeader, SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::UdpEndpoint;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Registrar default Expires when a REGISTER carries neither an `Expires`
/// header nor a Contact `;expires` param — the canonical RFC 3261 §10.2.1
/// value and the one sipjs locked in (`RegisterStrategy.DEFAULT_EXPIRES_SEC`).
pub const DEFAULT_EXPIRES_SEC: u32 = 3600;

/// The hop count a request that states none is forwarded with (RFC 3261
/// §8.1.1.6), before the §16.6 decrement.
const DEFAULT_MAX_FORWARDS: u32 = 70;

/// A live AOR binding (faithful to `Registrar.ts` `Binding`).
#[derive(Debug, Clone)]
struct Binding {
    /// Contact URI as supplied by the REGISTER, kept whole (RFC 3261 §10.3).
    contact_uri: Uri,
    /// Absolute virtual-clock millis when this binding expires.
    expires_at_ms: i64,
}

/// In-memory AOR → Contact binding store with lazy TTL on the harness clock —
/// the Rust port of `Registrar.inMemoryLayer`. AOR keys are the lowercased
/// userpart; expiry is swept lazily on every `lookup`/`register`.
#[derive(Clone)]
pub struct Registrar {
    clock: Clock,
    bindings: Arc<Mutex<HashMap<String, Binding>>>,
}

impl Registrar {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            bindings: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Store / refresh `aor → contact_uri` for `ttl_sec` seconds. Existing
    /// binding for the same AOR is replaced (v1 last-write-wins).
    async fn register(&self, aor: &str, contact_uri: Uri, ttl_sec: u32) {
        let now = self.clock.now_ms();
        self.bindings.lock().await.insert(
            aor.to_lowercase(),
            Binding { contact_uri, expires_at_ms: now + (ttl_sec as i64) * 1000 },
        );
    }

    /// Remove the binding for `aor` immediately (idempotent) — `Expires: 0`.
    async fn remove(&self, aor: &str) {
        self.bindings.lock().await.remove(&aor.to_lowercase());
    }

    /// Look up the live Contact URI for `aor`, sweeping it if expired. `None`
    /// when there is no binding or it has lapsed (lazy expiry — `Registrar.ts`
    /// `sweep`).
    async fn lookup(&self, aor: &str) -> Option<Uri> {
        let now = self.clock.now_ms();
        let key = aor.to_lowercase();
        let mut map = self.bindings.lock().await;
        match map.get(&key) {
            Some(b) if b.expires_at_ms <= now => {
                map.remove(&key);
                None
            }
            Some(b) => Some(b.contact_uri.clone()),
            None => None,
        }
    }
}

/// A running in-process register front proxy bound as a SUT. Aborts its recv
/// loop on drop (same guard shape as the LB `ProxyGuard` in `infra.rs`).
pub struct RegisterProxyGuard {
    task: JoinHandle<()>,
}

impl Drop for RegisterProxyGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Spawn the autonomous register front proxy on `ep` (its bound address is
/// `proxy_addr`). It handles REGISTER locally against `registrar` and routes
/// inbound dialog-creating INVITEs by AOR lookup to the registered Contact,
/// then transparently relays the rest of the dialog (§16 Via/Record-Route
/// surgery) so responses and in-dialog requests loop back through it.
pub fn spawn_register_proxy(
    ep: Box<dyn UdpEndpoint>,
    proxy_addr: SocketAddr,
    registrar: Registrar,
) -> RegisterProxyGuard {
    let task = tokio::spawn(async move {
        let proxy = RegisterProxy {
            ep: ep.into(),
            addr: proxy_addr,
            registrar,
            branch: std::sync::atomic::AtomicU64::new(0),
        };
        proxy.run().await;
    });
    RegisterProxyGuard { task }
}

struct RegisterProxy {
    ep: Arc<dyn UdpEndpoint>,
    addr: SocketAddr,
    registrar: Registrar,
    branch: std::sync::atomic::AtomicU64,
}

impl RegisterProxy {
    async fn run(&self) {
        while let Some(pkt) = self.ep.recv().await {
            let Ok(msg) = CustomParser::new().parse(&pkt.raw) else {
                continue; // ignore garbage on the wire (same as the real proxy)
            };
            match msg {
                SipMessage::Request(req) => self.on_request(req, pkt.src).await,
                SipMessage::Response(resp) => self.on_response(resp).await,
            }
        }
    }

    fn next_branch(&self) -> String {
        let n = self
            .branch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("z9hG4bK-regproxy-{n}")
    }

    async fn send_wire(&self, bytes: &[u8], dst: SocketAddr) {
        let _ = self.ep.send_to(bytes, dst).await;
    }

    /// The loose-route entry this proxy records on a dialog (§16.6.4).
    fn record_route(&self) -> RecordRouteEntry {
        let uri = Uri::sip(self.addr.ip().to_string())
            .with_port(self.addr.port())
            .with_flag("lr");
        RecordRouteEntry::from_uri(uri)
    }

    /// This proxy's own hop, with a fresh branch (§16.6 step 8).
    fn via(&self) -> Via {
        Via::udp(self.addr.ip().to_string(), self.addr.port()).with_branch(self.next_branch())
    }

    async fn on_request(&self, req: SipRequest, src: SocketAddr) {
        // ── REGISTER: handle locally (mimics RegisterStrategy.handle) ──────────
        if req.method() == Method::Register {
            self.handle_register(&req, src).await;
            return;
        }

        // ── Max-Forwards (§16.6 step 3): every forwarded request MUST have its
        //    Max-Forwards decremented; a request that arrives at 0 is rejected
        //    483 Too Many Hops and never forwarded. An ACK (no transaction, no
        //    response possible) is the one request a proxy forwards without a
        //    possible 483 — but it still decrements. ────────────────────────────
        let Ok(hops) = forwarded_max_forwards(&req) else {
            if req.method() == Method::Ack {
                // ACK is hop-by-hop with no response: drop it rather than 483.
                return;
            }
            self.reject(&req, 483, "Too Many Hops", src).await;
            return;
        };
        let mut draft = req.thaw().set(hops);

        // ── Loose-router self-pop (§16.4): in-dialog requests (ACK/BYE/…) carry
        //    our Record-Route as the top Route — strip it before forwarding. A
        //    route set the strict reader rejects rides through untouched. ──────
        let routes = req.list::<RouteEntry>().unwrap_or_default();
        let pop_self = routes.first().and_then(|r| uri_addr(r.uri())) == Some(self.addr);
        if pop_self {
            let Ok(popped) = draft.pop_top::<RouteEntry>() else { return };
            draft = popped;
        }

        // ── Dialog-creating INVITE: resolve the Request-URI AOR → Contact
        //    (mimics CoreToExtRoutingStrategy.registrarLookupLayer). ──────────
        let next_hop = if req.method() == Method::Invite && req.to().tag().is_none() {
            match self.resolve_aor(&req).await {
                Ok(dest) => {
                    // §7.3 asks a proxy to write what it processes near the top,
                    // so a first Record-Route opens the header block.
                    let entry = self.record_route();
                    draft = if draft.has(&HeaderName::RecordRoute) {
                        draft.prepend(entry)
                    } else {
                        draft.push_front(entry)
                    };
                    dest
                }
                Err((status, reason)) => {
                    self.reject(&req, status, reason, src).await;
                    return;
                }
            }
        } else {
            // In-dialog / other: next hop is the top surviving Route (loose
            // routing) or the Request-URI (§16.5/§16.6).
            let route_hop = routes.get(usize::from(pop_self)).and_then(|r| uri_addr(r.uri()));
            match route_hop.or_else(|| uri_addr(req.request_uri())) {
                Some(d) => d,
                None => return,
            }
        };

        // Add our Via on top so the response routes back to us (§16.6); forward.
        let Ok(bytes) = draft.prepend(self.via()).freeze_bytes() else { return };
        self.send_wire(&bytes, next_hop).await;
    }

    /// Resolve the inbound Request-URI's AOR userpart to the registered Contact's
    /// host:port. Returns `Err((status, reason))` to reject, mirroring
    /// `CoreToExtRoutingStrategy.resolve`'s `RouteOutcome::reject`.
    async fn resolve_aor(&self, req: &SipRequest) -> Result<SocketAddr, (u16, &'static str)> {
        let aor = req
            .request_uri()
            .user()
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .ok_or((400u16, "Bad Request"))?;
        let contact = self.registrar.lookup(&aor).await.ok_or((404u16, "Not Found"))?;
        uri_addr(&contact).ok_or((500u16, "Server Internal Error"))
    }

    /// REGISTER handler — the Rust port of `RegisterStrategy.inMemoryRegistrar`.
    async fn handle_register(&self, req: &SipRequest, src: SocketAddr) {
        // AOR = To-URI userpart, lowercased (RFC 3261 §10.2).
        let aor = req.to().uri().user().filter(|u| !u.is_empty()).map(str::to_string);
        let contact_raw = req.raw_text(HeaderName::Contact).next();
        let (Some(aor), Some(contact_raw)) = (aor, contact_raw) else {
            // To-URI userpart and a Contact are both required (RFC 3261 §10.3).
            self.reject(req, 400, "Bad Request", src).await;
            return;
        };
        // A Contact no reader accepts — `*` above all — carries no binding: it
        // may de-register, never register (RFC 3261 §10.3 step 6).
        let contact = Contact::parse(&contact_raw).ok();
        let expires_sec = effective_expires(req, contact.as_ref());

        let granted = match (&contact, expires_sec) {
            (_, 0) => {
                self.registrar.remove(&aor).await; // single-Contact de-registration
                None
            }
            (Some(contact), _) => {
                self.registrar.register(&aor, contact.uri().clone(), expires_sec).await;
                Some(contact.clone())
            }
            (None, _) => {
                self.reject(req, 400, "Bad Request", src).await;
                return;
            }
        };

        // 200 OK echoes the granted Contact + Expires (RFC 3261 §10.3 step 8).
        // The lifetime rides as a HEADER parameter of the Contact, which is
        // where §10.2.4 puts it and what a name-addr echo keeps it as.
        let contact_echo = match granted {
            Some(contact) => {
                extra_header(contact.with_param("expires", ParamValue::text(expires_sec.to_string())))
            }
            None => SipHeader {
                name: HeaderName::Contact.as_wire_str().into(),
                value: contact_raw.clone(),
            },
        };
        let resp = generate_response(
            req,
            200,
            "OK",
            &GenerateResponseOpts {
                to_tag: Some(self.reg_tag()),
                contact: None,
                body: vec![],
                content_type: None,
                extra_headers: vec![contact_echo, extra_header(Expires::new(expires_sec))],
                incoming_source: Some((src.ip().to_string(), src.port())),
            },
        );
        self.send_wire(resp.image(), src).await;
    }

    async fn reject(&self, req: &SipRequest, status: u16, reason: &str, src: SocketAddr) {
        let resp = generate_response(
            req,
            status,
            reason,
            &GenerateResponseOpts {
                to_tag: Some(self.reg_tag()),
                contact: None,
                body: vec![],
                content_type: None,
                extra_headers: vec![],
                incoming_source: Some((src.ip().to_string(), src.port())),
            },
        );
        self.send_wire(resp.image(), src).await;
    }

    fn reg_tag(&self) -> String {
        format!("regproxy-{}", self.next_branch())
    }

    /// Relay a response upstream: strip our own top Via (§16.7) and send to the
    /// address in the now-top Via (the next hop toward the UAC).
    async fn on_response(&self, resp: SipResponse) {
        let vias = resp.list::<Via>().unwrap_or_default();
        let ours = vias.first().and_then(via_addr) == Some(self.addr);
        let Some(dst) = vias.get(usize::from(ours)).and_then(via_addr) else { return };
        let mut draft = resp.thaw();
        if ours {
            let Ok(popped) = draft.pop_top::<Via>() else { return };
            draft = popped;
        }
        let Ok(bytes) = draft.freeze_bytes() else { return };
        self.send_wire(&bytes, dst).await;
    }
}

// ---------------------------------------------------------------------------
// REGISTER expiry precedence (port of RegisterStrategy.computeEffectiveExpires)
// ---------------------------------------------------------------------------

/// Effective Expires: `Expires` header › Contact `;expires` param › default.
/// A value no reader accepts (negative, non-numeric) falls through to the next
/// source; `0` is preserved (de-register).
fn effective_expires(req: &SipRequest, contact: Option<&Contact>) -> u32 {
    if let Some(Ok(expires)) = req.header::<Expires>() {
        return expires.value();
    }
    // Contact `;expires=N` — both the URI param and the header-level param.
    let Some(contact) = contact else { return DEFAULT_EXPIRES_SEC };
    for value in [contact.uri().param("expires"), contact.param("expires")] {
        if let Some(Ok(n)) = value.and_then(ParamValue::as_str).map(str::parse::<u32>) {
            return n;
        }
    }
    DEFAULT_EXPIRES_SEC
}

// ---------------------------------------------------------------------------
// §16 routing helpers (mirrors scenario-harness `Proxy`)
// ---------------------------------------------------------------------------

/// The hop count a forwarded request carries (RFC 3261 §16.6 step 3): the
/// inbound value decremented, or the §8.1.1.6 default decremented when the
/// request states no readable count. `Err(())` when the inbound count is
/// already 0 — the caller answers 483 Too Many Hops or drops an ACK.
fn forwarded_max_forwards(req: &SipRequest) -> Result<MaxForwards, ()> {
    match req.header::<MaxForwards>() {
        Some(Ok(hops)) => hops.decremented().ok_or(()),
        _ => Ok(MaxForwards::new(DEFAULT_MAX_FORWARDS - 1)),
    }
}

/// A typed value lowered onto the generator's still-stringly `extra_headers`.
fn extra_header(value: impl HeaderValue) -> SipHeader {
    SipHeader { name: value.name().as_wire_str().into(), value: value.to_wire().into() }
}

/// The transport address a URI names, defaulting the port per RFC 3261 §19.1.2.
fn uri_addr(uri: &Uri) -> Option<SocketAddr> {
    let (host, port) = uri.host_port();
    hostport_to_addr(&format!("{host}:{port}"))
}

/// Where to relay a response next: the address in a Via's sent-by (§18.2.2).
fn via_addr(via: &Via) -> Option<SocketAddr> {
    let (host, port) = via.sent_by().pair();
    hostport_to_addr(&format!("{host}:{port}"))
}

fn hostport_to_addr(host_port: &str) -> Option<SocketAddr> {
    if let Ok(sa) = host_port.parse::<SocketAddr>() {
        return Some(sa);
    }
    format!("{host_port}:5060").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::SipStr;

    /// A REGISTER carrying `extra` on top of the mandatory headers, as the
    /// parser reads it.
    fn parse_register(extra: &[(&str, &str)]) -> Result<SipMessage, sip_message::SipParseError> {
        let mut raw = "REGISTER sip:127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-t\r\n\
             From: <sip:bob@register.example>;tag=t\r\nTo: <sip:bob@register.example>\r\n\
             Call-ID: c\r\nCSeq: 1 REGISTER\r\n"
            .to_string();
        for (name, value) in extra {
            raw.push_str(&format!("{name}: {value}\r\n"));
        }
        raw.push_str("Content-Length: 0\r\n\r\n");
        CustomParser::new().parse(raw.as_bytes())
    }

    fn register(extra: &[(&str, &str)]) -> SipRequest {
        match parse_register(extra).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("not a request"),
        }
    }

    fn hops(req: &SipRequest) -> Option<u32> {
        forwarded_max_forwards(req).ok().map(|h| h.value())
    }

    #[test]
    fn max_forwards_decrements_by_one() {
        assert_eq!(hops(&register(&[("Max-Forwards", "70")])), Some(69));
    }

    #[test]
    fn max_forwards_zero_is_rejected() {
        assert_eq!(hops(&register(&[("Max-Forwards", "0")])), None);
    }

    #[test]
    fn max_forwards_absent_is_the_decremented_default() {
        // A request missing the header is treated as the §8.1.1.6 default (70).
        assert_eq!(hops(&register(&[])), Some(69));
    }

    /// A hop count no reader accepts never reaches the forwarding path: the
    /// parser refuses the whole message, so the proxy's default covers the
    /// request that states no count at all.
    #[test]
    fn max_forwards_non_numeric_never_parses() {
        assert!(parse_register(&[("Max-Forwards", "garbage")]).is_err());
    }

    /// RegisterStrategy.computeEffectiveExpires precedence: Expires header wins,
    /// then Contact `;expires`, then the default; `0` is preserved (de-register).
    #[test]
    fn effective_expires_precedence() {
        let contact = |value: &str| Contact::parse(&SipStr::owned(value)).unwrap();
        let plain = contact("<sip:bob@127.0.0.1:5170>");

        // Expires header wins over the default.
        let r = register(&[("Contact", "<sip:bob@127.0.0.1:5170>"), ("Expires", "120")]);
        assert_eq!(effective_expires(&r, Some(&plain)), 120);

        // Header-level Contact `;expires=` when no Expires header.
        let tagged = contact("<sip:bob@127.0.0.1:5170>;expires=42");
        let r = register(&[("Contact", "<sip:bob@127.0.0.1:5170>;expires=42")]);
        assert_eq!(effective_expires(&r, Some(&tagged)), 42);

        // Default when neither is present.
        let r = register(&[("Contact", "<sip:bob@127.0.0.1:5170>")]);
        assert_eq!(effective_expires(&r, Some(&plain)), DEFAULT_EXPIRES_SEC);

        // 0 is preserved (de-registration).
        let r = register(&[("Contact", "<sip:bob@127.0.0.1:5170>"), ("Expires", "0")]);
        assert_eq!(effective_expires(&r, Some(&plain)), 0);
    }

    /// A wildcard Contact reads as no binding at all: it may de-register, and
    /// the 200 OK echoes the bytes the UA sent (RFC 3261 §10.3 step 6).
    #[test]
    fn wildcard_contact_carries_no_binding() {
        assert!(Contact::parse(&SipStr::owned("*")).is_err());
        let r = register(&[("Contact", "*"), ("Expires", "0")]);
        assert_eq!(effective_expires(&r, None), 0);
    }
}
