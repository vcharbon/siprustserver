//! [`B2buaScene`] — the standard alice ↔ b2bua ↔ bob fixture.

use scenario_harness::{callflow, Agent, Dialog, Harness, RunReport};

use crate::{settle_until, B2buaSut, B2buaSutBuilder, LimiterLeak};

/// The port of `alice` in a [`B2buaScene`]: the first address a fresh
/// harness allocates ([`Harness::next_addr`]).
pub const ALICE_PORT: u16 = 5060;
/// The port of `bob` in a [`B2buaScene`]: the second address allocated.
pub const BOB_PORT: u16 = 5070;
/// The port of the `b2bua` SUT in a [`B2buaScene`]: the third address
/// allocated.
pub const B2BUA_PORT: u16 = 5080;

/// The standard alice ↔ b2bua ↔ bob fixture with canonical default ports and a
/// b2bua that routes every call to bob — so a new test never picks ports or
/// wires routing by hand. Access [`alice`](Self::alice)/[`bob`](Self::bob)/
/// [`b2bua`](Self::b2bua) for the interesting part; [`establish`](Self::establish)
/// runs the canonical call (delegating to [`callflow::establish`]);
/// [`finish`](Self::finish) gates RFC + renders.
///
/// # Writing a new single-SUT b2bua test
///
/// The 90% case is one line:
///
/// ```ignore
/// let s = B2buaScene::new("my-test").await;
/// let mut dialog = s.establish().await;          // alice → b2bua → bob, confirmed
/// // ... the interesting part, driving `s.alice` / `s.bob` / `s.b2bua` ...
/// s.hangup(&mut dialog).await;                    // BYE / 200
/// s.finish().await;                              // reaped check + RFC gate + render
/// ```
///
/// [`finish`](Self::finish) settles the SUT and runs
/// [`B2buaSut::assert_fully_reaped`]: a call the scenario left up, or a
/// limiter hold never released, fails the test. A scenario that leaves
/// limiter holds behind on purpose declares them with
/// [`finish_leaving`](Self::finish_leaving).
///
/// The scene's addresses come from its harness's allocator
/// ([`Harness::next_addr`]); a further party takes the next one with
/// [`Harness::agent_auto`]. Each `Harness` has its own simulated network
/// namespace, so addresses never collide between tests. For a non-default b2bua decision (refer, limiter,
/// services, config tune) build it via [`with_b2bua`](Self::with_b2bua), which
/// hands you bob's port and the full [`B2buaSutBuilder`]:
///
/// ```ignore
/// let s = B2buaScene::with_b2bua("refer-test", |bob_port| {
///     B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
/// }).await;
/// ```
pub struct B2buaScene {
    pub h: Harness,
    pub alice: Agent,
    pub bob: Agent,
    pub b2bua: B2buaSut,
}

impl B2buaScene {
    /// alice/bob/b2bua bound on the harness's first three addresses
    /// ([`ALICE_PORT`]/[`BOB_PORT`]/[`B2BUA_PORT`]); the b2bua routes every call
    /// to bob. The 90%-case fixture — one line for a new test.
    pub async fn new(name: &str) -> Self {
        Self::with_b2bua(name, |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await
    }

    /// Like [`new`](Self::new) but customise the b2bua: `build` is handed bob's
    /// port and must return the [`B2buaSutBuilder`] to start (start is done for
    /// you on the scene's third address). This composes cleanly with every existing
    /// `B2buaSut::route_all*` entry point and the builder's chain methods
    /// (`outbound_proxy`, `limiter`, `services`, `tune`), so a test needing a
    /// non-default decision still gets the canonical alice/bob/ports wiring for
    /// free. Default decision = `route_all_to(bob)` (see [`new`](Self::new)).
    pub async fn with_b2bua(name: &str, build: impl FnOnce(u16) -> B2buaSutBuilder) -> Self {
        // Match the common b2bua-test convention: the harness's default
        // SIMULATED_TRANSIT_DELAY_MS (1 ms floor enforced by the fabric).
        let h = Harness::new(name);
        let alice = h.agent_auto("alice").await;
        let bob = h.agent_auto("bob").await;
        let at = h.next_addr().to_string();
        let b2bua = build(bob.addr().port()).start(&h, "b2bua", &at).await;
        Self { h, alice, bob, b2bua }
    }

    /// Run the canonical call (alice → b2bua → bob, confirmed) and return alice's
    /// [`Dialog`]. Equivalent to
    /// `callflow::establish(&self.alice, &self.bob, self.b2bua.addr)`.
    pub async fn establish(&self) -> Dialog {
        callflow::establish(&self.alice, &self.bob, self.b2bua.addr).await
    }

    /// Tear the call down (alice BYE / bob 200). Equivalent to
    /// `callflow::hangup(dialog, &self.bob)`.
    pub async fn hangup(&self, dialog: &mut Dialog) {
        callflow::hangup(dialog, &self.bob).await
    }

    /// Settle until every call is reaped, run [`B2buaSut::assert_fully_reaped`],
    /// then gate the RFC audit and render the report. Consumes the scene
    /// (`Harness::finish` consumes `self`).
    pub async fn finish(self) -> RunReport {
        self.finish_leaving(LimiterLeak::NONE).await
    }

    /// [`finish`](Self::finish) for a scenario that leaves limiter holds
    /// behind on purpose: the reaped check requires exactly `leak`.
    pub async fn finish_leaving(self, leak: LimiterLeak) -> RunReport {
        settle_until(|| self.b2bua.is_reaped_leaving(&leak)).await;
        self.b2bua.assert_fully_reaped_leaving(leak);
        self.h.finish().await
    }
}
