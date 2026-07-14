# callshapes — composable, cross-platform call shapes

A **call shape** is a SIP call flow *composed* from an algebra, not hand-written:

```
ShapePlan = Establishment × Stage[] × Teardown          (routed through a RouteBinder)
```

Each shape compiles to a `scenario_harness::actor::ActorCall` at build time and
implements `ActorScenario`, so the same definition drives the loadgen fleet, the
in-process fake-net tests, and the functional leak gate — unchanged.

This crate is consumed **upstream** (siprustserver's loadgen + e2e model) and by
**external platforms** that do not use siprustserver's `X-Api-Call` routing —
newkahsip, which routes by *dialed number*, is the worked example throughout.
Nothing platform-specific lives here: a downstream platform picks up the crate on
its next submodule pin and writes one small adapter (a `RouteBinder`) plus a
~30-LOC bin.

---

## 1. The pipeline algebra

Compose a shape by filling three slots (`callshapes::plan`):

| Slot | Type | Values (as landed) |
|---|---|---|
| **Establishment** | `Establishment` | `Transparent` · `Reliable` (100rel) · `RerouteOnReject{reject, winner_reliable}` (E4 — bob rejects with **no 18x**, SUT fails over to `bob2`) · `Forked{tags, winner, reliable, loser_late_200}` (E3 true forking) · `RejectTerminal{code}` · `AbandonAfterRinging` · `CancelAnswerCrossing` (E5 branch race) |
| **Stage[]** | `Vec<Stage>` | `Stage::Script(...)`: `Reinvite{n}` (S1, N serialized) · `UpdatePostConnect` (S2) · `UpdateEarly` (C5, early dialog) · `KeepaliveOnce` · `KeepaliveLoop`. `Stage::Transfer(...)`: `Blind{refer_key}` (T1 REFER) · `BlindDeclined{refer_key, code}` |
| **Teardown** | `Teardown` | `CallerBye{after, feed}` · `CrossingBye{after}` (S3 both ends hang up at once) · `None` (terminals only) |

Each stage yields the *current dialog* the next stage runs on, so one `Reinvite`
script runs identically on the initial dialog, the post-reroute dialog, or the
post-REFER dialog — depending only on where it sits in the chain.

The shipped catalog (`callshapes::shapes`) is a set of constructors, each taking
the platform binder:

```rust
use callshapes::shapes;
let binder = shapes::default_binder();          // Arc<dyn RouteBinder>
let plan   = shapes::reinvite_n(binder, "reinvite10", 10);   // Reliable? no — Transparent + Reinvite{10}
```

To compose a **new** cell directly (the `ShapePlan` fields are public):

```rust
use callshapes::plan::{ByeFeed, DwellKnob, Establishment, Script, ShapePlan, Stage, Teardown};

let plan = ShapePlan {
    id: "reliable+reinvite",                    // ScenarioId = &'static str (report/metrics label)
    binder: shapes::default_binder(),
    establish: Establishment::Reliable,
    stages: vec![Stage::Script(Script::Reinvite { n: 1 })],
    teardown: Teardown::CallerBye { after: DwellKnob::ReinviteGap, feed: ByeFeed::CheckpointAndPhase },
    ringing_gate: true,
    stamp_connected: true,
};
plan.validate().unwrap();                        // structural check (env-independent)
```

`validate()` rejects miscomposed chains (a stage after a terminal establishment,
a non-terminal with no teardown, `Reinvite{n: 0}`, `UpdateEarly` on a
non-`Reliable` establishment, `Forked` with < 2 tags / a bad winner).

### The declared compatibility matrix

`e2e_model::matrix` declares the establishment × script axes as data and
generates the compatibility-gated cross-product into the registry with stable
ids (`"<establishment>+<script>"`, e.g. `forked+reinvite`, `reroute+update`) —
so the catalog is *generated*, not hand-listed. Add an axis point (an `Est` or
`Scr`) or a `compatible()` rule there and every legal cell appears.

---

## 2. The abstract routing seam (`RouteBinder`)

A shape never spells wire routing syntax. A stage declares a `RouteIntent`:

```rust
pub enum RouteIntent<'a> {
    Direct { target: &'static str },              // deliver to one callee ROLE ("bob")
    FailoverOnReject { targets: &'a [&'static str] }, // try in order, advance on a reject
}
```

and a per-platform `RouteBinder` turns it into a concrete INVITE:

```rust
pub trait RouteBinder: Send + Sync {
    fn invite_plan(&self, env: &CallEnv<'_>, intent: RouteIntent<'_>) -> InvitePlan;
    fn refer_authorization(&self, env: &CallEnv<'_>, refer_key: &str) -> Option<String> { /* default */ }
}
```

Upstream ships `EgressBinder` (the historic `EgressPolicy` / `X-Api-Call` seam;
`invite_plan` just delegates to `env.invite_plan(intent.targets())`).

### Worked example — newkahsip's dial-plan binder

newkahsip routes by *dialed number* (an R-URI user mapped from
`routing-mock/config/numbers.json`), not `X-Api-Call`. It implements the trait
by building an `InvitePlan` with its own R-URI (the `InvitePlan` fields — `via`,
`from`, `to`, `ruri`, `headers`, `rewrite` — are public):

```rust
struct DialPlanBinder { plan: NumberPlan }        // loaded from numbers.json

impl RouteBinder for DialPlanBinder {
    fn invite_plan(&self, env: &CallEnv<'_>, intent: RouteIntent<'_>) -> InvitePlan {
        // Map the intent's FIRST target role → the dialed number whose BL
        // scenario realizes that behaviour (a plain number for Direct; a number
        // whose downstream reroutes for FailoverOnReject).
        let number = self.plan.number_for(intent.targets()[0]);
        InvitePlan {
            via: env.via,                          // still routes THROUGH the SUT ingress
            ruri: Some(format!("sip:{number}@{}", env.via)),
            from: None, to: None, headers: vec![],
            rewrite: Default::default(),           // EgressRewrite::default() = no-op
        }
    }
}
```

The same shapes now run on newkahsip with zero changes — only the binder differs.
Per-run values (numbers, header names, `refer_key`) come from the case JSON /
`ScenarioInputs`.

---

## 3. Registering shapes — the ~30-LOC downstream bin

The load application (`loadgen::app`) is parameterised by an injectable
`ShapeRegistry`, so a third-party load bin is a one-liner over its own registry.
The shipped bin (`crates/loadgen/src/bin/loadgen.rs`) is just:

```rust
loadgen::app::run(Args::parse(), loadgen::ShapeRegistry::with_defaults()).await
```

A downstream bin starts from `ShapeRegistry::empty()` (or `with_defaults()`) and
registers its composed shapes. `ShapeDescriptor` carries the load metadata:

```rust
use e2e_model::{ShapeRegistry, ShapeDescriptor};
use std::sync::Arc;

fn nk_registry() -> ShapeRegistry {
    let binder = || Arc::new(DialPlanBinder::load("config/numbers.json")) as Arc<dyn RouteBinder>;
    let mut reg = ShapeRegistry::empty();
    reg.register(
        ShapeDescriptor::new("nk_reroute+reinvite")
            .needs_bob2()                                        // topology the driver must bind
            .default_weight(1.0)                                 // share of the default mix
            .load_shared(Arc::new(nk_reroute_reinvite(binder()))), // Arc<dyn ActorScenario>
    );
    // …register the rest…
    reg
}
// fn main() { loadgen::app::run(Args::parse(), nk_registry()).await }
```

Descriptor knobs: `.anchors(&[Anchor])` (sampling), `.needs_charlie()` /
`.needs_bob2()` (extra callee legs the driver binds), `.default_weight(f64)` /
`.failure_weight(f64)` (mix membership), `.emergency()`, `.load_shared(body)` for
a stateless body or `.load_actor_with(|inputs| …)` when the body needs
`ScenarioInputs` (e.g. a `refer_key`). One id space — a duplicate id panics at
registration.

---

## 4. Adding a new Establishment / Script / Transfer

1. **Add the variant** to the enum in `callshapes::plan` (`Establishment`,
   `Script`, or `Transfer`).
2. **Compile it** — extend the matching arm in `ShapePlan::compile_establishment`
   / `compile_script` / `compile_transfer`: push the caller goals + callee
   `ActorSpec`(s), set the barrier `gate`, and (for terminals) the `Expect`.
   Reactive answering is NOT scripted — the callee `Disposition` and the reactor
   in `scenario_harness::actor` own it; add a `Disposition` variant there if the
   callee needs new answer behaviour (as C1 `ForkingRing`, C5
   `ReliableAnswerEarlyUpdate` did).
3. **Validate it** — add any structural precondition to `ShapePlan::validate`
   (e.g. `UpdateEarly` requires `Establishment::Reliable`).
4. **Barrier/phase names are a fixed bounded vocabulary** (`established`,
   `reinvited`, `updated`, `merged`, …) — they key `StepError::who` and the
   downstream contract; never free-form.
5. **Test it** SUT-less first (a `scenario_harness::actor` machinery test), then
   through the SUT on the fake net.

---

## 5. The fake-net paused-clock test pattern

New shape coverage grows in `crates/loadgen/tests/fake_net.rs`: the REAL driver +
mux + `DropModel` loss engine run over one `SimulatedSignalingNetwork` shared with
an in-process `B2buaCore`, under `#[tokio::test(start_paused = true)]`. Every
timer (governor, recv, retransmit ladders, the SUT's 32 s reap) rides virtual
time, so loss soaks that need 32 s+ of SIP traffic run deterministically in the
default lane.

```rust
#[tokio::test(start_paused = true)]
async fn my_shape() {
    let (h, b2bua, core, transport) = setup_fake(7200).await;   // shared sim net + SUT
    let reporter = Arc::new(Reporter::new(/* … */));
    let driver = Driver::new(cfg(b2bua.addr, 20.0, 2, 8, seed), vec![mix("my_shape", 1.0)], reporter.clone(), transport);
    driver.run().await;
    assert_eq!(reporter.count("my_shape", &ResultClass::Ok), reporter.total_calls());
    // Terminate + assert release (CLAUDE.md rule): drain, advance past the 32 s
    // dead-call timer, then the strict oracle.
    h.advance(Duration::from_secs(40)).await;
    b2bua.assert_fully_reaped();
}
```

Loss soaks use the `loss_soak()` helper (7 % drop + auto-retransmit → the
graceful-degradation gates: recovery happens, `RfcAuditFail == 0`, every NOK is a
bounded `Timeout`, fully reaped). Deterministic `TargetedDrop` tests drop the nth
distinct **request** of a method (`permanent: false` proves recovery,
`permanent: true` bounded give-up). **Gotcha:** an ×N shape carries ~N× the
datagrams, so the governor's *delivered-call count* varies under concurrent
scheduling — keep `total >= …` floors conservative and don't assert retransmit
*dominance* (only `ok > 0`).

Read `docs/testing/test-clock.md` before writing any timed test.

---

## 6. SUT-reachability — do NOT create dangling-fork load cells

Some behaviours are **peer-to-peer only** and CANNOT be a through-SUT load cell,
because a dialog-terminating B2BUA absorbs the relevant message:

| Behaviour | Why not through-SUT | Coverage |
|---|---|---|
| `forked_loser_late_200` | the SUT forwards only the FIRST b-leg 2xx and absorbs the loser's late 200 → the caller's ACK+BYE-the-loser path is unreachable | SUT-less machinery tests (C1a/C1b) |
| re-INVITE glare (S5) | needs BOTH ends to re-INVITE at once; the SUT owns one leg | SUT-less machinery test (C4/S5) |
| UPDATE-vs-re-INVITE collision (S6) | same | SUT-less machinery test (C4/S6) |

These have **no** `ShapeRegistry` load cell and no `TargetedDrop`. Everything else
(reliable, forked, reroute, CANCEL×200 crossing, early UPDATE, crossing BYE, the
generated cross-product) is dialog-terminating-B2BUA-reachable and IS a load cell.
When you add a new behaviour, decide this first: if the SUT would swallow the
distinguishing message, keep it SUT-less and do **not** register a load cell for
it — a dangling fork/leg on the callee times the call out (a false NOK), not a
real finding.

---

## Pointers

- Algebra + compile fold: `src/plan.rs` · catalog: `src/shapes.rs` · routing
  seam: `src/binder.rs`.
- Matrix generation + catalog weights: `e2e_model::matrix`, `e2e_model::registry`.
- Reactor / dispositions / barriers: `scenario_harness::actor`.
- Program spec + phase findings: `docs/todos/callshapes-program.md`.
- Load bin quick start + how it relates to the test suites: `crates/loadgen/README.md`.
