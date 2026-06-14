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
use sip_message::message_helpers::{extract_contact_uri, get_header, get_headers, parse_sip_uri};
use sip_message::parser::custom::CustomParser;
use sip_message::{serialize, SipHeader, SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::UdpEndpoint;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Registrar default Expires when a REGISTER carries neither an `Expires`
/// header nor a Contact `;expires` param — the canonical RFC 3261 §10.2.1
/// value and the one sipjs locked in (`RegisterStrategy.DEFAULT_EXPIRES_SEC`).
pub const DEFAULT_EXPIRES_SEC: u32 = 3600;

/// A live AOR binding (faithful to `Registrar.ts` `Binding`).
#[derive(Debug, Clone)]
struct Binding {
    /// Contact URI as supplied by the REGISTER, stored verbatim (RFC 3261 §10.3).
    contact_uri: String,
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
    async fn register(&self, aor: &str, contact_uri: &str, ttl_sec: u32) {
        let now = self.clock.now_ms();
        self.bindings.lock().await.insert(
            aor.to_lowercase(),
            Binding {
                contact_uri: contact_uri.to_string(),
                expires_at_ms: now + (ttl_sec as i64) * 1000,
            },
        );
    }

    /// Remove the binding for `aor` immediately (idempotent) — `Expires: 0`.
    async fn remove(&self, aor: &str) {
        self.bindings.lock().await.remove(&aor.to_lowercase());
    }

    /// Look up the live Contact URI for `aor`, sweeping it if expired. `None`
    /// when there is no binding or it has lapsed (lazy expiry — `Registrar.ts`
    /// `sweep`).
    async fn lookup(&self, aor: &str) -> Option<String> {
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

    async fn send(&self, msg: &SipMessage, dst: SocketAddr) {
        let _ = self.ep.send_to(&serialize(msg), dst).await;
    }

    async fn on_request(&self, mut req: SipRequest, src: SocketAddr) {
        let method = req.method.to_string();
        // ── REGISTER: handle locally (mimics RegisterStrategy.handle) ──────────
        if method.eq_ignore_ascii_case("REGISTER") {
            self.handle_register(&req, src).await;
            return;
        }

        // ── Max-Forwards (§16.6 step 3): every forwarded request MUST have its
        //    Max-Forwards decremented; a request that arrives at 0 is rejected
        //    483 Too Many Hops and never forwarded. An ACK (no transaction, no
        //    response possible) is the one request a proxy forwards without a
        //    possible 483 — but it still decrements. A missing header defaults to
        //    70 before the decrement (RFC 3261 §16.6 step 3 / §8.1.1.6). ───────
        match decrement_max_forwards(&mut req.headers) {
            Ok(()) => {}
            Err(()) if method.eq_ignore_ascii_case("ACK") => {
                // ACK is hop-by-hop with no response: drop it rather than 483.
                return;
            }
            Err(()) => {
                self.reject(&req, 483, "Too Many Hops", src).await;
                return;
            }
        }

        // ── Loose-router self-pop (§16.4): in-dialog requests (ACK/BYE/…) carry
        //    our Record-Route as the top Route — strip it before forwarding. ──
        strip_top_route_if_self(&mut req, self.addr);

        // ── Dialog-creating INVITE: resolve the Request-URI AOR → Contact
        //    (mimics CoreToExtRoutingStrategy.registrarLookupLayer). ──────────
        let next_hop = if method.eq_ignore_ascii_case("INVITE") && req.to.tag.is_none() {
            match self.resolve_aor(&req).await {
                Ok(dest) => {
                    prepend_header(
                        &mut req.headers,
                        "Record-Route",
                        &format!("<sip:{}:{};lr>", self.addr.ip(), self.addr.port()),
                    );
                    dest
                }
                Err((status, reason)) => {
                    self.reject(&req, status, reason, src).await;
                    return;
                }
            }
        } else {
            // In-dialog / other: next hop is the top Route (post self-pop) or the
            // Request-URI — the standard loose-routing next hop (§16.5/§16.6).
            match self.in_dialog_next_hop(&req) {
                Some(d) => d,
                None => return,
            }
        };

        // Add our Via on top so the response routes back to us (§16.6); forward.
        prepend_header(
            &mut req.headers,
            "Via",
            &format!(
                "SIP/2.0/UDP {}:{};branch={}",
                self.addr.ip(),
                self.addr.port(),
                self.next_branch()
            ),
        );
        self.send(&SipMessage::Request(req), next_hop).await;
    }

    /// Resolve the inbound Request-URI's AOR userpart to the registered Contact's
    /// host:port. Returns `Err((status, reason))` to reject, mirroring
    /// `CoreToExtRoutingStrategy.resolve`'s `RouteOutcome::reject`.
    async fn resolve_aor(&self, req: &SipRequest) -> Result<SocketAddr, (u16, &'static str)> {
        let aor = parse_sip_uri(&req.uri)
            .and_then(|u| u.user)
            .filter(|u| !u.is_empty())
            .ok_or((400u16, "Bad Request"))?;
        let contact = self
            .registrar
            .lookup(&aor)
            .await
            .ok_or((404u16, "Not Found"))?;
        let bare = extract_contact_uri(&contact);
        let parsed = parse_sip_uri(&bare).ok_or((500u16, "Server Internal Error"))?;
        format!("{}:{}", parsed.host, parsed.port)
            .parse::<SocketAddr>()
            .map_err(|_| (500u16, "Server Internal Error"))
    }

    /// The next hop for an in-dialog / non-dialog-creating request: the address
    /// of the top Route (loose routing) or, absent a Route, the Request-URI.
    fn in_dialog_next_hop(&self, req: &SipRequest) -> Option<SocketAddr> {
        if let Some(route) = get_header(&req.headers, "route") {
            if let Some(addr) = uri_to_addr(route) {
                return Some(addr);
            }
        }
        uri_to_addr(&req.uri)
    }

    /// REGISTER handler — the Rust port of `RegisterStrategy.inMemoryRegistrar`.
    async fn handle_register(&self, req: &SipRequest, src: SocketAddr) {
        // AOR = To-URI userpart, lowercased (RFC 3261 §10.2).
        let aor = parse_sip_uri(&req.to.uri)
            .and_then(|u| u.user)
            .filter(|u| !u.is_empty());
        let contact_raw = get_header(&req.headers, "contact").map(str::to_string);
        let (Some(aor), Some(contact_raw)) = (aor, contact_raw) else {
            // To-URI userpart and a Contact are both required (RFC 3261 §10.3).
            self.reject(req, 400, "Bad Request", src).await;
            return;
        };
        let contact_uri = extract_contact_uri(&contact_raw);
        let expires_sec = effective_expires(req, &contact_raw);

        if expires_sec == 0 {
            self.registrar.remove(&aor).await; // single-Contact de-registration
        } else {
            self.registrar
                .register(&aor, &contact_uri, expires_sec)
                .await;
        }

        // 200 OK echoes the granted Contact + Expires (RFC 3261 §10.3 step 8).
        let resp = generate_response(
            req,
            200,
            "OK",
            &GenerateResponseOpts {
                to_tag: Some(self.reg_tag()),
                contact: None,
                body: vec![],
                content_type: None,
                extra_headers: vec![
                    SipHeader {
                        name: "Contact".into(),
                        value: format!("{contact_raw};expires={expires_sec}"),
                    },
                    SipHeader {
                        name: "Expires".into(),
                        value: expires_sec.to_string(),
                    },
                ],
                incoming_source: Some((src.ip().to_string(), src.port())),
            },
        );
        self.send(&SipMessage::Response(resp), src).await;
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
        self.send(&SipMessage::Response(resp), src).await;
    }

    fn reg_tag(&self) -> String {
        format!("regproxy-{}", self.next_branch())
    }

    /// Relay a response upstream: strip our own top Via (§16.7) and send to the
    /// address in the now-top Via (the next hop toward the UAC).
    async fn on_response(&self, mut resp: SipResponse) {
        strip_top_via_if_self(&mut resp.headers, self.addr);
        if let Some(dst) = top_via_addr(&resp) {
            self.send(&SipMessage::Response(resp), dst).await;
        }
    }
}

// ---------------------------------------------------------------------------
// REGISTER expiry precedence (port of RegisterStrategy.computeEffectiveExpires)
// ---------------------------------------------------------------------------

/// Effective Expires: `Expires` header › Contact `;expires` param › default.
/// Negative/unparseable collapse to the default; `0` is preserved (de-register).
fn effective_expires(req: &SipRequest, contact_value: &str) -> u32 {
    if let Some(h) = get_header(&req.headers, "expires") {
        if let Ok(n) = h.trim().parse::<i64>() {
            if n >= 0 {
                return n as u32;
            }
        }
    }
    // Contact `;expires=N` — both the URI param and the header-level param.
    let bare = extract_contact_uri(contact_value);
    if let Some(u) = parse_sip_uri(&bare) {
        if let Some(v) = u.params.get("expires") {
            if let Ok(n) = v.trim().parse::<i64>() {
                if n >= 0 {
                    return n as u32;
                }
            }
        }
    }
    // Header-level `;expires=` after the closing `>` of a name-addr Contact.
    if let (Some(gt), true) = (contact_value.find('>'), contact_value.contains('<')) {
        for seg in contact_value[gt + 1..].split(';') {
            let seg = seg.trim();
            if let Some(rest) = seg.strip_prefix("expires=").or_else(|| {
                seg.split_once('=')
                    .filter(|(k, _)| k.eq_ignore_ascii_case("expires"))
                    .map(|(_, v)| v)
            }) {
                if let Ok(n) = rest.trim().parse::<i64>() {
                    if n >= 0 {
                        return n as u32;
                    }
                }
            }
        }
    }
    DEFAULT_EXPIRES_SEC
}

// ---------------------------------------------------------------------------
// §16 routing surgery helpers (mirrors scenario-harness `Proxy`)
// ---------------------------------------------------------------------------

/// Decrement Max-Forwards in place (RFC 3261 §16.6 step 3). A missing header is
/// treated as 70 (the §8.1.1.6 default a well-formed request carries) and
/// inserted decremented. Returns `Err(())` when the inbound value is already 0
/// (the caller rejects 483 Too Many Hops / drops an ACK); a non-numeric value is
/// repaired to `70 - 1` so a malformed hop count can never wedge forwarding.
fn decrement_max_forwards(headers: &mut Vec<SipHeader>) -> Result<(), ()> {
    if let Some(h) = headers
        .iter_mut()
        .find(|h| h.name.eq_ignore_ascii_case("max-forwards"))
    {
        let cur: u32 = h.value.trim().parse().unwrap_or(70);
        if cur == 0 {
            return Err(());
        }
        h.value = (cur - 1).to_string();
        return Ok(());
    }
    headers.push(SipHeader {
        name: "Max-Forwards".to_string(),
        value: "69".to_string(),
    });
    Ok(())
}

fn prepend_header(headers: &mut Vec<SipHeader>, name: &str, value: &str) {
    headers.insert(
        0,
        SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        },
    );
}

/// Strip the first Route header if it loose-routes to `me` (§16.4 self-pop).
fn strip_top_route_if_self(req: &mut SipRequest, me: SocketAddr) {
    if let Some(pos) = req
        .headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("route"))
    {
        if uri_to_addr(&req.headers[pos].value) == Some(me) {
            req.headers.remove(pos);
        }
    }
}

/// Strip the topmost Via if it is `me`'s (§16.7 response surgery).
fn strip_top_via_if_self(headers: &mut Vec<SipHeader>, me: SocketAddr) {
    if let Some(pos) = headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("via"))
    {
        if via_sent_by_addr(&headers[pos].value) == Some(me) {
            headers.remove(pos);
        }
    }
}

/// The address in the topmost Via's sent-by (where to relay a response next).
fn top_via_addr(resp: &SipResponse) -> Option<SocketAddr> {
    let vias = get_headers(&resp.headers, "via");
    via_sent_by_addr(vias.first()?)
}

fn via_sent_by_addr(via: &str) -> Option<SocketAddr> {
    let sent_by = via
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.split(';').next())
        .map(str::trim)?;
    hostport_to_addr(sent_by)
}

/// Resolve a SIP URI (or `<uri;lr>` Route value, or bare host:port) to an addr.
fn uri_to_addr(uri: &str) -> Option<SocketAddr> {
    let parsed = parse_sip_uri(uri)?;
    hostport_to_addr(&format!("{}:{}", parsed.host, parsed.port))
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

    fn hdr(name: &str, value: &str) -> SipHeader {
        SipHeader { name: name.into(), value: value.into() }
    }

    fn mf(headers: &[SipHeader]) -> Option<String> {
        headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("max-forwards"))
            .map(|h| h.value.clone())
    }

    #[test]
    fn max_forwards_decrements_by_one() {
        let mut h = vec![hdr("Max-Forwards", "70")];
        assert!(decrement_max_forwards(&mut h).is_ok());
        assert_eq!(mf(&h).as_deref(), Some("69"));
    }

    #[test]
    fn max_forwards_zero_is_rejected() {
        let mut h = vec![hdr("Max-Forwards", "0")];
        assert!(decrement_max_forwards(&mut h).is_err());
        // The value is left untouched so the 483 reflects what arrived.
        assert_eq!(mf(&h).as_deref(), Some("0"));
    }

    #[test]
    fn max_forwards_absent_is_inserted_decremented() {
        // A request missing the header is treated as the §8.1.1.6 default (70).
        let mut h = vec![hdr("Via", "SIP/2.0/UDP 127.0.0.1:5060")];
        assert!(decrement_max_forwards(&mut h).is_ok());
        assert_eq!(mf(&h).as_deref(), Some("69"));
    }

    #[test]
    fn max_forwards_non_numeric_is_repaired() {
        let mut h = vec![hdr("Max-Forwards", "garbage")];
        assert!(decrement_max_forwards(&mut h).is_ok());
        assert_eq!(mf(&h).as_deref(), Some("69"));
    }

    /// RegisterStrategy.computeEffectiveExpires precedence: Expires header wins,
    /// then Contact `;expires`, then the default; `0` is preserved (de-register).
    #[test]
    fn effective_expires_precedence() {
        let req = |hdrs: Vec<SipHeader>| {
            let mut raw = "REGISTER sip:127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-t\r\n\
                 From: <sip:bob@register.example>;tag=t\r\nTo: <sip:bob@register.example>\r\n\
                 Call-ID: c\r\nCSeq: 1 REGISTER\r\n"
                .to_string();
            for h in &hdrs {
                raw.push_str(&format!("{}: {}\r\n", h.name, h.value));
            }
            raw.push_str("Content-Length: 0\r\n\r\n");
            match CustomParser::new().parse(raw.as_bytes()).unwrap() {
                SipMessage::Request(r) => r,
                _ => panic!("not a request"),
            }
        };
        let contact = "<sip:bob@127.0.0.1:5170>";

        // Expires header wins over the default.
        let r = req(vec![
            SipHeader { name: "Contact".into(), value: contact.into() },
            SipHeader { name: "Expires".into(), value: "120".into() },
        ]);
        assert_eq!(effective_expires(&r, contact), 120);

        // Header-level Contact `;expires=` when no Expires header.
        let c2 = "<sip:bob@127.0.0.1:5170>;expires=42";
        let r = req(vec![SipHeader { name: "Contact".into(), value: c2.into() }]);
        assert_eq!(effective_expires(&r, c2), 42);

        // Default when neither is present.
        let r = req(vec![SipHeader { name: "Contact".into(), value: contact.into() }]);
        assert_eq!(effective_expires(&r, contact), DEFAULT_EXPIRES_SEC);

        // 0 is preserved (de-registration).
        let r = req(vec![
            SipHeader { name: "Contact".into(), value: contact.into() },
            SipHeader { name: "Expires".into(), value: "0".into() },
        ]);
        assert_eq!(effective_expires(&r, contact), 0);
    }
}
