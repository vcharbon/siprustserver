# callshapes: composable call shapes

A **call shape** is a SIP call flow composed from an algebra, not hand-written:

```
ShapePlan = Establishment × Stage[] × Teardown          (routed through a RouteBinder)
```

Each shape compiles to a `scenario_harness::actor::ActorCall` and implements
`ActorScenario`, so one definition drives the loadgen fleet, the in-process
fake-net tests and the functional leak gate. Nothing platform-specific lives
here: a platform that does not route by `X-Api-Call` writes one `RouteBinder`.

---

## The pipeline algebra

Compose a shape by filling three slots (`callshapes::plan`):

| Slot | Type | Values |
|---|---|---|
| **Establishment** | `Establishment` | `Transparent` · `Reliable` (100rel) · `RerouteOnReject{reject, winner_reliable}` (bob rejects with no 18x, the SUT fails over to `bob2`) · `RerouteOnNoAnswer{no_answer_ms, winner_reliable}` (bob rings and never answers; the SUT's no-answer timer CANCELs it and fails over to `bob2`) · `Forked{tags, winner, reliable, loser_late_200}` · `RejectTerminal{code}` · `AbandonAfterRinging` · `CancelAnswerCrossing` |
| **Stage[]** | `Vec<Stage>` | `Stage::Script(...)`: `Reinvite{n}` · `UpdatePostConnect` · `UpdateEarly` (early dialog) · `KeepaliveOnce` · `KeepaliveLoop`. `Stage::Transfer(...)`: `Blind{refer_key}` · `BlindDeclined{refer_key, code}` |
| **Teardown** | `Teardown` | `CallerBye{after, feed}` · `CrossingBye{after}` (both ends hang up at once) · `None` (terminals only) |

Each stage yields the current dialog the next stage runs on, so one `Reinvite`
script runs the same on the initial, the post-reroute or the post-REFER dialog.

The shipped catalog (`callshapes::shapes`) is a set of constructors, each taking
the binder:

```rust
use callshapes::shapes;
let binder = shapes::default_binder();          // Arc<dyn RouteBinder>
let plan   = shapes::reinvite_n(binder, "reinvite10", 10);
```

A new cell is a `ShapePlan` literal (its fields are public):

```rust
use callshapes::plan::{ByeFeed, DwellKnob, Establishment, Script, ShapePlan, Stage, Teardown};

let plan = ShapePlan {
    id: "reliable+reinvite",                    // the report and metrics label
    binder: shapes::default_binder(),
    establish: Establishment::Reliable,
    stages: vec![Stage::Script(Script::Reinvite { n: 1 })],
    teardown: Teardown::CallerBye { after: DwellKnob::ReinviteGap, feed: ByeFeed::CheckpointAndPhase },
    ringing_gate: true,
    stamp_connected: true,
};
plan.validate().unwrap();
```

`validate()` rejects miscomposed chains: a stage after a terminal
establishment, a non-terminal with no teardown, `Reinvite{n: 0}`, `UpdateEarly`
on a non-`Reliable` establishment, `Forked` with fewer than 2 tags or a bad
winner.

`e2e_model::matrix` declares the establishment × script axes as data and
generates the compatible cross-product into the registry with stable ids
(`"<establishment>+<script>"`, e.g. `forked+reinvite`).

---

## The routing seam (`RouteBinder`)

A shape never spells wire routing syntax. A stage declares a `RouteIntent`
(`Direct { target }`: one callee role; `FailoverOnReject { targets }`: try in
order, advance on a reject), and a per-platform `RouteBinder` turns it into a
concrete INVITE:

```rust
pub trait RouteBinder: Send + Sync {
    fn invite_plan(&self, env: &CallEnv<'_>, intent: RouteIntent<'_>) -> InvitePlan;
    fn refer_authorization(&self, env: &CallEnv<'_>, refer_key: &str) -> Option<String> { /* default */ }
}
```

The shipped `EgressBinder` delegates to `env.invite_plan(intent.targets())`
(the `EgressPolicy` / `X-Api-Call` seam). A platform that routes by dialed
number returns an `InvitePlan` (public fields: `via`, `from`, `to`, `ruri`,
`headers`, `rewrite`) whose R-URI is the number its plan maps the intent's
first target role to, still `via` the SUT ingress. The shapes then run there
unchanged. Per-run values (numbers, header names, `refer_key`) come from the
case JSON or `ScenarioInputs`.

---

## Registering shapes

The load application (`loadgen::app`) runs behind the `Product` seam: a product
names its bin, adds its flags, and builds the `ShapeRegistry` its flags select.
The shipped bin is `loadgen::app::main(loadgen::app::Shipped)`. A product's
registry starts from `ShapeRegistry::empty()` or `with_defaults()` and
registers its shapes as `ShapeDescriptor`s; the descriptor knobs are listed in
[the loadgen README](../loadgen/README.md#how-to-add-a-scenario).

---

## Adding a new Establishment / Script / Transfer

1. **Add the variant** to the enum in `callshapes::plan` (`Establishment`,
   `Script`, or `Transfer`).
2. **Compile it**: extend the matching arm in `ShapePlan::compile_establishment`
   / `compile_script` / `compile_transfer`: push the caller goals and callee
   `ActorSpec`s, set the barrier `gate`, and for terminals the `Expect`.
   Reactive answering is not scripted: the callee `Disposition` and the reactor
   in `scenario_harness::actor` own it; add a `Disposition` variant there if the
   callee needs a new answer behaviour.
3. **Validate it**: add any structural precondition to `ShapePlan::validate`.
4. **Barrier and phase names are a fixed vocabulary** (`established`,
   `reinvited`, `updated`, `merged`, …): they key `StepError::who` and the
   downstream contract; never free-form.
5. **Test it** SUT-less first (a `scenario_harness::actor` machinery test), then
   through the SUT on the fake net.

---

## The fake-net paused-clock tests

New shape coverage grows in `crates/loadgen/tests/it/fake_net.rs`: the real
driver, mux and `DropModel` loss engine over one `SimulatedSignalingNetwork`
shared with an in-process `B2buaCore`, under `start_paused = true`
(`setup_fake`, `loss_soak`). Read `docs/testing/test-clock.md` before writing
a timed test. An ×N shape carries about N× the datagrams, so the delivered-call
count varies under concurrent scheduling: keep `total >= …` floors
conservative.

---

## SUT-reachability: do NOT create dangling-fork load cells

Some behaviours are **peer-to-peer only** and cannot be a through-SUT load cell,
because a dialog-terminating B2BUA absorbs the relevant message:

| Behaviour | Why not through-SUT | Coverage |
|---|---|---|
| `forked_loser_late_200` | the SUT forwards only the first b-leg 2xx and absorbs the loser's late 200, so the caller's ACK and BYE of the loser are unreachable | SUT-less machinery tests |
| re-INVITE glare | needs both ends to re-INVITE at once; the SUT owns one leg | SUT-less machinery test |
| UPDATE-vs-re-INVITE collision | same | SUT-less machinery test |

These have **no** `ShapeRegistry` load cell and no `TargetedDrop`. Everything
else (reliable, forked, reroute, CANCEL×200 crossing, early UPDATE, crossing
BYE, the generated cross-product) is reachable through a dialog-terminating
B2BUA and is a load cell. When you add a behaviour, decide this first: if the
SUT would swallow the distinguishing message, keep it SUT-less. A dangling fork
or leg on the callee times the call out, a false NOK.

---

## Pointers

- Algebra and compile fold: `src/plan.rs` · catalog: `src/shapes.rs` · routing
  seam: `src/binder.rs`.
- Matrix generation and catalog weights: `e2e_model::matrix`, `e2e_model::registry`.
- Reactor, dispositions, barriers: `scenario_harness::actor`.
- The load binary and how it relates to the test suites: `crates/loadgen/README.md`.
