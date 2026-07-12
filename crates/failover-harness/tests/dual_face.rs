//! **Dual-face (multi-homed edge) proxy through the REAL `ProxyCore`** — the
//! two-plane network split (RFC 3261 §16 multi-homed proxy):
//!
//!   external plane 192.168.60.0/24          internal plane 10.244.0.0/16
//!   alice/bob/charlie (callers/callees)     b1 (b2bua worker)
//!            │                                        │
//!            ▼                                        ▼
//!   alice ─▶ proxy EXT face ═══ ONE ProxyCore ═══ proxy INT face ─▶ b1
//!         (192.168.60.250:5060)                (10.244.255.250:5080)
//!
//! The proxy is the ONLY bridge between the planes: every datagram a caller
//! ever sees must originate from the EXT face's bind, every worker-bound one
//! from the INT face's — asserted from the recorded wire trace on every test
//! ([`assert_plane_discipline`]), which is simultaneously the face-picker
//! check (requests AND responses) and the per-face source-address discipline
//! check. Double-RR order/cookie, in-dialog traversal in both directions,
//! CANCEL/ACK correlation across faces, and the dual-VIP takeover ride the
//! full callflow through a real worker; every call terminates and the worker
//! is asserted fully reaped. Per-message contracts (exact Via/RR bytes,
//! single-face byte-compat) live in `sip-proxy/src/core/dual_face_tests.rs`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::NoopLimiter;
use failover_harness::FailoverHarness;
use sip_message::types::SipHeader;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

// External (caller) plane.
const ALICE: &str = "192.168.60.10:5060";
const BOB: &str = "192.168.60.20:5060";
const CHARLIE: &str = "192.168.60.21:5060"; // reroute target
const PROXY_EXT: &str = "192.168.60.250:5060"; // external VIP
// Internal (worker) plane.
const PROXY_INT: &str = "10.244.255.250:5080"; // internal VIP
const B1: &str = "10.244.0.11:5091";
const INT_CIDRS: &str = "10.244.0.0/16";

/// Which plane an address lives on (the test topology is two disjoint IPv4
/// prefixes, so this is total).
fn plane(a: SocketAddr) -> &'static str {
    let ip = a.ip().to_string();
    if ip.starts_with("10.244.") {
        "int"
    } else if ip.starts_with("192.168.60.") {
        "ext"
    } else {
        panic!("address {a} belongs to neither test plane");
    }
}

/// THE dual-face invariant, read off the recorded wire trace: **no datagram
/// ever crosses the planes** — the proxy bridges by re-originating from the
/// face the destination lives on, so every recorded send has `from` and `to`
/// on the SAME plane. This is simultaneously (a) the egress face picker check
/// for requests and responses (a response relayed out the wrong socket would
/// pair an int `from` with an ext `to`) and (e) the per-face source-address
/// discipline (each face's egress sources from that face's bind).
fn assert_plane_discipline(fh: &FailoverHarness) {
    let entries = fh.sip_entries();
    assert!(!entries.is_empty(), "expected recorded traffic");
    for e in &entries {
        assert_eq!(
            plane(e.from),
            plane(e.to),
            "datagram crossed the planes on the wire: {} -> {} ({})",
            e.from,
            e.to,
            String::from_utf8_lossy(&e.raw).lines().next().unwrap_or(""),
        );
    }
    // And the caller-plane half really is the proxy's EXTERNAL bind (not some
    // other leak): everything a caller heard from the proxy came from EXT.
    let ext_face: SocketAddr = PROXY_EXT.parse().unwrap();
    let int_face: SocketAddr = PROXY_INT.parse().unwrap();
    assert!(
        entries.iter().any(|e| e.from == ext_face),
        "the external face must have originated traffic",
    );
    assert!(
        entries.iter().any(|e| e.from == int_face),
        "the internal face must have originated traffic",
    );
}

/// The Record-Route header values of a header list, in wire order.
fn rr_values(headers: &[SipHeader]) -> Vec<String> {
    headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("record-route"))
        .map(|h| h.value.clone())
        .collect()
}

/// No Route header naming either proxy face survives past the proxy (both
/// self-halves popped before the forward).
fn assert_no_proxy_route(headers: &[SipHeader], what: &str) {
    for h in headers.iter().filter(|h| h.name.eq_ignore_ascii_case("route")) {
        assert!(
            !h.value.contains("192.168.60.250") && !h.value.contains("10.244.255.250"),
            "{what}: a proxy self-Route leaked through: {}",
            h.value,
        );
    }
}

/// The `branch=` token of the topmost Via header.
fn top_via_branch(headers: &[SipHeader]) -> String {
    let via = headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("via"))
        .expect("a Via header");
    via.value
        .split(';')
        .find_map(|p| p.trim().strip_prefix("branch="))
        .expect("a branch param")
        .to_string()
}

/// Wait for the single worker to fully reap the call (creations == removals,
/// per-call memory clean) — the CLAUDE.md release assertion.
async fn assert_reaped(fh: &FailoverHarness, b1: &failover_harness::ReplicatedB2buaSut) {
    let reaped = fh
        .settle_terminal(|| async {
            b1.metrics().removals_total() == b1.metrics().creations_total() && b1.memory_clean()
        })
        .await;
    assert!(
        reaped,
        "call must be fully reaped (creations {} != removals {}, or memory not clean: {} live / {} locks)",
        b1.metrics().creations_total(),
        b1.metrics().removals_total(),
        b1.active_calls(),
        b1.lock_count(),
    );
}

fn route_all_to_bob() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| NewCallResponse::Route(route_to("192.168.60.20", 5060)))
            .build(),
    )
}

/// Common bring-up: agents, dual-face proxy, one worker routed to bob whose
/// b-leg egresses through the proxy's INTERNAL face.
async fn stand_up(
    fh: &mut FailoverHarness,
    decision: Arc<dyn CallDecisionEngine>,
) -> (
    scenario_harness::Agent,
    scenario_harness::Agent,
    failover_harness::ProxySut,
    failover_harness::ReplicatedB2buaSut,
) {
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy = fh
        .spawn_proxy_dual("proxy", PROXY_INT, PROXY_EXT, INT_CIDRS, &[("b1", B1.parse().unwrap())], 0xC0FFEE)
        .await;
    let b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &[],
            ("192.168.60.20", 5060),
            ("10.244.255.250", 5080),
            decision,
            Arc::new(NoopLimiter),
        )
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready(), "the lone worker is ready at steady state");
    (alice, bob, proxy, b1)
}

/// **(a)(b)(e) + caller-side BYE (c).** Full establish/teardown across the two
/// planes. Pins the §16.6/§16.7 multi-homed double-RR as each ENDPOINT sees
/// it — order, per-face hosts, and the stickiness cookie on BOTH entries — and
/// the plane discipline over the whole recorded trace.
#[tokio::test(start_paused = true)]
async fn dual_face_establish_double_rr_and_caller_bye() {
    let mut fh = FailoverHarness::new("dual-face-establish", &["b1"]);
    let (alice, bob, proxy, b1) = stand_up(&mut fh, route_all_to_bob()).await;

    // ── alice → EXT face → b1 → INT face … EXT face → bob ────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.ext_addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;

    // (b) The CALLEE's view of the b-leg INVITE: topmost RR faces bob (EXT
    // cookie entry), the worker-facing INT `;outbound` entry below — cookie
    // (`w_pri=b1`) on BOTH entries. No proxy Route survives.
    {
        let req = bob_uas.request();
        let rrs = rr_values(&req.headers);
        assert_eq!(rrs.len(), 2, "double record-route at the callee, got {rrs:?}");
        assert!(rrs[0].contains("192.168.60.250:5060"), "top RR faces bob (EXT): {}", rrs[0]);
        assert!(!rrs[0].contains("outbound"), "callee-facing RR is the cookie half: {}", rrs[0]);
        assert!(rrs[0].contains("w_pri=b1"), "cookie on the callee-facing RR: {}", rrs[0]);
        assert!(rrs[1].contains("10.244.255.250:5080"), "lower RR faces the worker (INT): {}", rrs[1]);
        assert!(rrs[1].contains("outbound"), "worker-facing RR keeps the direction marker: {}", rrs[1]);
        assert!(rrs[1].contains("w_pri=b1"), "cookie on the worker-facing RR too: {}", rrs[1]);
        assert_no_proxy_route(&req.headers, "b-leg INVITE at bob");
    }

    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    let confirmed = call.expect(200).await;
    assert_eq!(confirmed.status, 200);

    // (b) The CALLER's view of the a-leg 200 (the RR set alice reverses per
    // §12.1.2): as-received order INT `;outbound` on top, EXT cookie below —
    // so alice's REVERSED route set starts with the face facing HER (EXT).
    {
        let rrs = rr_values(&confirmed.headers);
        assert_eq!(rrs.len(), 2, "double record-route echoed to the caller, got {rrs:?}");
        assert!(rrs[0].contains("10.244.255.250:5080"), "top RR (worker-facing, INT): {}", rrs[0]);
        assert!(rrs[0].contains("outbound"), "worker-facing marker: {}", rrs[0]);
        assert!(rrs[0].contains("w_pri=b1"), "cookie on the worker-facing RR: {}", rrs[0]);
        assert!(rrs[1].contains("192.168.60.250:5060"), "lower RR (caller-facing, EXT): {}", rrs[1]);
        assert!(rrs[1].contains("w_pri=b1"), "cookie on the caller-facing RR: {}", rrs[1]);
        assert!(!rrs[1].contains("outbound"), "caller-facing RR is the cookie half: {}", rrs[1]);
    }

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── (c) caller → callee in-dialog: alice hangs up ────────────────────────
    let mut a_bye = dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;
    assert_no_proxy_route(&bob_bye.request().headers, "relayed BYE at bob");
    bob_bye.respond(200, "OK").await;
    a_bye.expect(200).await;

    assert_reaped(&fh, &b1).await;
    assert_plane_discipline(&fh);
    fh.assert_full_rfc_clean("dual-face-establish");
    drop(proxy);
}

/// **(c) callee → caller in-dialog.** Bob (the b-leg UAS) originates the BYE
/// on his §12.1.1 route set — EXT cookie entry on top. It must ingress the
/// EXT face, pop BOTH self-Routes, decode the cookie to b1 over the INT face,
/// and the relayed a-leg BYE must reach alice back out the EXT face.
#[tokio::test(start_paused = true)]
async fn dual_face_callee_initiated_bye_traverses_both_faces() {
    let mut fh = FailoverHarness::new("dual-face-callee-bye", &["b1"]);
    let (alice, bob, proxy, b1) = stand_up(&mut fh, route_all_to_bob()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.ext_addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    // Bob hangs up on his own route set (cookie/EXT top, outbound/INT below).
    let mut bob_dialog = bob_uas.dialog();
    let mut b_bye = bob_dialog.bye().await;
    let mut alice_bye = alice.receive("BYE").await;
    assert_no_proxy_route(&alice_bye.request().headers, "relayed BYE at alice");
    alice_bye.respond(200, "OK").await;
    b_bye.expect(200).await;

    assert_reaped(&fh, &b1).await;
    assert_plane_discipline(&fh);
    fh.assert_full_rfc_clean("dual-face-callee-bye");
    drop(proxy);
}

/// **(c) caller → callee re-INVITE.** A mid-dialog renegotiation traverses
/// both faces on the route set fixed at dialog creation; the proxy must NOT
/// re-insert Record-Route mid-dialog (§12.2) and must pop both self-halves.
#[tokio::test(start_paused = true)]
async fn dual_face_reinvite_mid_dialog() {
    let mut fh = FailoverHarness::new("dual-face-reinvite", &["b1"]);
    let (alice, bob, proxy, b1) = stand_up(&mut fh, route_all_to_bob()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.ext_addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Renegotiate (same offer — a session refresh) caller → callee.
    let mut ri = dialog.reinvite(Some(OFFER)).await;
    let mut bob_ri = bob.receive("INVITE").await;
    assert!(
        rr_values(&bob_ri.request().headers).is_empty(),
        "no Record-Route on a mid-dialog re-INVITE (§12.2)",
    );
    assert_no_proxy_route(&bob_ri.request().headers, "re-INVITE at bob");
    bob_ri.respond(200, "OK").with_sdp(ANSWER).await;
    ri.expect(200).await;
    dialog.ack(None).await;
    bob.receive("ACK").await;

    // Terminate properly.
    let mut a_bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    a_bye.expect(200).await;

    assert_reaped(&fh, &b1).await;
    assert_plane_discipline(&fh);
    fh.assert_full_rfc_clean("dual-face-reinvite");
    drop(proxy);
}

/// **(d) CANCEL correlation across faces.** Alice's CANCEL lands on the EXT
/// face; the worker CANCELs the ringing b-leg through the INT face; the
/// proxy's `cancel_lru` (ONE instance across faces) must reuse the b-leg
/// INVITE's branch so bob sees the CANCEL on the SAME transaction (§9.1).
#[tokio::test(start_paused = true)]
async fn dual_face_cancel_correlates_across_faces() {
    let mut fh = FailoverHarness::new("dual-face-cancel", &["b1"]);
    let (alice, bob, proxy, b1) = stand_up(&mut fh, route_all_to_bob()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.ext_addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    let invite_branch = top_via_branch(&bob_uas.request().headers);
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // ── alice abandons: CANCEL through the EXT face ──────────────────────────
    let mut cxl = call.cancel().await;
    cxl.expect(200).await; // 200 to the CANCEL (a-leg, txn layer)
    call.expect(487).await; // 487 on the INVITE (auto-ACKed §17.1.1.3)

    // The worker CANCELs the still-ringing b-leg via the INT face; §9.1: the
    // CANCEL must carry the b-leg INVITE's top-Via branch — the shared
    // cancel_lru correlated it across the two sockets.
    let mut bob_cancel = bob.receive_absorbing("CANCEL", &["INVITE"]).await;
    assert_eq!(
        top_via_branch(&bob_cancel.request().headers),
        invite_branch,
        "the CANCEL must ride the b-leg INVITE's transaction (branch reuse across faces)",
    );
    bob_cancel.respond(200, "OK").await;
    bob_uas.respond(487, "Request Terminated").await;
    // The proxy's hop-by-hop §17.1.1.3 ACK for the 487 terminates at bob.
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    // Flush the worker's OWN §17.1.1.3 ACK (for the relayed 487) its one hop
    // to the proxy before the RFC gate snapshots the trace.
    fh.advance(Duration::from_millis(20)).await;

    assert_reaped(&fh, &b1).await;
    assert_plane_discipline(&fh);
    fh.assert_full_rfc_clean("dual-face-cancel");
    drop(proxy);
}

/// **(d) non-2xx ACK correlation across faces (486-then-reroute).** Bob 486s
/// the b-leg: the proxy synthesizes the hop ACK toward bob on the EXT face
/// (same branch as the INVITE it relayed), absorbs the worker's own ACK
/// arriving on the INT face, and the worker's reroute INVITE reaches charlie
/// — alice is bridged. The via-LB reroute twin, split across two planes.
#[tokio::test(start_paused = true)]
async fn dual_face_486_reroute_ack_correlation() {
    let mut fh = FailoverHarness::new("dual-face-486-reroute", &["b1"]);
    let decision: Arc<dyn CallDecisionEngine> = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("192.168.60.20", 5060);
                r.callback_context = Some("dual-face-486-reroute-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                let mut r = route_to("192.168.60.21", 5060);
                r.new_ruri = Some("sip:192.168.60.21:5060".into());
                CallTreatment::Route(r)
            })
            .build(),
    );
    let (alice, bob, proxy, b1) = stand_up(&mut fh, decision).await;
    let charlie = fh.agent("charlie", CHARLIE).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.ext_addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    let invite_branch = top_via_branch(&bob_uas.request().headers);
    bob_uas.respond(486, "Busy Here").await;

    // The hop ACK bob receives must belong to the INVITE transaction it 486'd
    // (§17.1.1.3 — same branch), even though the 486 relayed INWARD crossed to
    // the INT face and the worker's own ACK is absorbed there.
    let ack = bob.receive_absorbing("ACK", &["INVITE"]).await;
    assert_eq!(
        top_via_branch(&ack.request().headers),
        invite_branch,
        "the non-2xx hop ACK must reuse the INVITE's branch across the face split",
    );

    // ── the reroute reaches charlie (EXT face); alice is bridged ─────────────
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    let confirmed = call.expect(200).await;
    assert_eq!(confirmed.status, 200, "caller bridged to the reroute target");
    let mut dialog = call.ack().await;
    charlie.receive("ACK").await;

    let mut d_bye = dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    d_bye.expect(200).await;

    assert_reaped(&fh, &b1).await;
    assert_plane_discipline(&fh);
    fh.assert_full_rfc_clean("dual-face-486-reroute");
    drop(proxy);
}

/// **(f) dual-VIP takeover.** Both faces' identities (the two VIPs) move to a
/// peer proxy TOGETHER — the VRRP failover model. An established call
/// survives: the takeover proxy (same HMAC key, fresh branch space) decodes
/// the caller's cookie on the EXT face and bridges the in-dialog teardown
/// across both planes. The proxy is stateless by design, so the only state
/// that must survive is what rides the message (Route cookie) — pinned here.
#[tokio::test(start_paused = true)]
async fn dual_face_takeover_moves_both_faces_together() {
    let mut fh = FailoverHarness::new("dual-face-takeover", &["b1"]);
    let (alice, bob, proxy_a, b1) = stand_up(&mut fh, route_all_to_bob()).await;

    // ── establish through proxy A ────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy_a.ext_addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── proxy A dies; BOTH VIPs move to the peer proxy ───────────────────────
    fh.mark("proxy", None, "crash", "dual-face proxy killed; VIPs move to peer");
    drop(proxy_a);
    // Let the aborted recv task actually drop (releasing both face binds)
    // before the peer claims the same two VIP addresses.
    fh.advance(Duration::from_millis(100)).await;
    let proxy_b = fh
        .spawn_proxy_dual("proxy2", PROXY_INT, PROXY_EXT, INT_CIDRS, &[("b1", B1.parse().unwrap())], 0xFACE2)
        .await;
    fh.mark("proxy", None, "reboot", "peer proxy holds both VIPs");

    // ── the established call survives: teardown rides the takeover proxy ─────
    let mut a_bye = dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;
    bob_bye.respond(200, "OK").await;
    a_bye.expect(200).await;

    assert_reaped(&fh, &b1).await;
    assert_plane_discipline(&fh);
    fh.assert_full_rfc_clean("dual-face-takeover");
    drop(proxy_b);
}
