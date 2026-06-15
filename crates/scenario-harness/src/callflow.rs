//! Imperative call-choreography primitives — the canonical INVITE/180/200/ACK
//! dance, parameterised only on the address the caller routes *through*.
//!
//! # Use these; don't re-type the dance
//!
//! Every "set the call up, then test the interesting part" test runs the same
//! happy-path handshake: caller INVITEs (with an SDP offer) through some `via`
//! address; callee rings (180) then answers (200 with an SDP answer); the ACK
//! is relayed end-to-end. The ONLY thing that differs between the single-SUT
//! b2bua tests and the HA failover tests is the `via` address — a [`B2buaSut`]
//! addr for single-SUT, a front-proxy VIP for HA. So that dance lives here,
//! once, operating purely on [`Agent`] + a [`SocketAddr`]:
//!
//! - [`establish`] — the 90% case: full 180-then-200 handshake, default SDP,
//!   returns the caller's [`Dialog`].
//! - [`hangup`] — the BYE/200 teardown half nearly every test repeats.
//! - [`Call`] — a small builder for the variants (no 180, custom SDP) the HA
//!   tests need.
//!
//! This is the imperative **callflow choreography** primitive. It is distinct
//! from the ADR-0018 registered "Callflow shape" (a parameterised, named
//! template in the e2e test-management framework): this one is just a function
//! you call inline from a `#[tokio::test]`.
//!
//! Opt out only when the *subject* of the test is the handshake itself — a test
//! that asserts on the intermediate 18x, on the relayed SDP bodies, that injects
//! a non-2xx final response, or that crashes/partitions a node mid-handshake
//! must keep driving the dance by hand. When in doubt, leave it by hand.

use std::net::SocketAddr;

use crate::{Agent, Dialog};

/// Canonical minimal SDP offer used by [`establish`] / [`Call`]. A single audio
/// `m=` line — enough for the B2BUA to relay; tests that don't probe media
/// don't care about its contents.
pub const OFFER_SDP: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
/// Canonical minimal SDP answer used by [`establish`] / [`Call`]. See [`OFFER_SDP`].
pub const ANSWER_SDP: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Establish a confirmed dialog: `caller` INVITEs (default [`OFFER_SDP`]) through
/// `via`; `callee` rings (180) then answers (200, default [`ANSWER_SDP`]); the ACK
/// is relayed. Returns the caller's [`Dialog`]. `via` is the address the caller
/// routes through — a `B2buaSut` addr for single-SUT tests, a front-proxy VIP for
/// HA tests; that address is the ONLY thing that differs between the two stacks.
/// For variations (no 180, custom SDP) use [`Call`].
pub async fn establish(caller: &Agent, callee: &Agent, via: SocketAddr) -> Dialog {
    Call::new(caller, callee, via).establish().await
}

/// `dialog` owner sends BYE; `callee` answers 200. The teardown half of a call —
/// the BYE/200 pattern nearly every test repeats after the interesting part.
pub async fn hangup(dialog: &mut Dialog, callee: &Agent) {
    let mut bye = dialog.bye().await;
    callee.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
}

/// Builder for the happy-path handshake variants. The defaults match
/// [`establish`] (ring=true, [`OFFER_SDP`]/[`ANSWER_SDP`]); chain
/// [`no_ring`](Self::no_ring)/[`offer`](Self::offer)/[`answer`](Self::answer)
/// for the few tests that need a 200-only answer or custom bodies, then
/// [`establish`](Self::establish).
pub struct Call<'a> {
    caller: &'a Agent,
    callee: &'a Agent,
    via: SocketAddr,
    offer: &'a str,
    answer: &'a str,
    ring: bool,
}

impl<'a> Call<'a> {
    /// New call from `caller` to `callee` routed through `via`, with the
    /// canonical defaults (rings 180, [`OFFER_SDP`]/[`ANSWER_SDP`]).
    pub fn new(caller: &'a Agent, callee: &'a Agent, via: SocketAddr) -> Self {
        Self { caller, callee, via, offer: OFFER_SDP, answer: ANSWER_SDP, ring: true }
    }

    /// Skip the 180 — callee answers 200 directly (the HA "200-only" variant).
    pub fn no_ring(mut self) -> Self {
        self.ring = false;
        self
    }

    /// Override the INVITE's SDP offer body.
    pub fn offer(mut self, sdp: &'a str) -> Self {
        self.offer = sdp;
        self
    }

    /// Override the 200's SDP answer body.
    pub fn answer(mut self, sdp: &'a str) -> Self {
        self.answer = sdp;
        self
    }

    /// Run the handshake and return the caller's confirmed [`Dialog`].
    pub async fn establish(self) -> Dialog {
        let mut call = self.caller.invite(self.callee).with_sdp(self.offer).through(self.via).send().await;
        let mut uas = self.callee.receive("INVITE").await;
        if self.ring {
            uas.respond(180, "Ringing").await;
            call.expect(180).await;
        }
        uas.respond(200, "OK").with_sdp(self.answer).await;
        call.expect(200).await;
        let dialog = call.ack().await;
        self.callee.receive("ACK").await;
        dialog
    }
}
