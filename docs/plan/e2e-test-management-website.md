# E2E Test-Management Website & CLI — Implementation Plan

Implements [ADR-0018](../adr/0018-e2e-test-management-architecture.md) (architecture)
and [ADR-0019](../adr/0019-e2e-check-model-and-anchors.md) (checks/anchors).
Vocabulary: `CONTEXT.md` → "E2E test-management vocabulary".

## Goal

Author (web), launch (web + CI CLI), and display end-to-end SIP tests built from
four orthogonal axes — **Callflow shape** (Rust) × **Infra shape** (Rust:
fake-sim-paused / real-kind-wallclock) × **Endpoint config** (JSON) × **Test case**
(JSON: input + checks + compatible shapes). The load-bearing invariant: **one
Callflow shape runs unchanged over fake and real** — only transport + clock differ,
never topology.

## Milestones (each independently demoable, dependency-ordered)

| # | Milestone | Proves |
|---|-----------|--------|
| **M1** | `basic-call` runs over **both** infra shapes via `e2e-core`, driven by a Rust integ test (no web). | The portability invariant — the riskiest thing — first. |
| **M2** | JSON model + schemas + check engine + `result.json`. | Data-driven tests + post-call checks. |
| **M3** | `e2e-cli` runs a campaign headless, exit-code gated. | CI/CD path. |
| **M4** | `e2e-web` (Maud/htmx) + content-negotiated API: list, launch, progress, cell detail w/ SVG. | The website. |
| **M5** | Media opt-in: per-endpoint `.wav` + classifier check. | "Hear the RTP." |
| **M6** | `rerouting` + `rerouting-prack` shapes; `invite-identity` Check set; `source.ip` checks. | Shape/check breadth + sharing. |

Do **not** build the web before M1 proves portability. If the real path is blocked
by cluster reachability in the dev environment, M1's fake half still lands and the
real half is a separate, env-gated task (it is the same code).

## Crate layout

```
crates/
  e2e-core/        # lib: registry, JSON model+schemas, check engine, run executor,
                   #      result model + persistence, artifact writers
  e2e-cli/         # bin: headless campaign runner (CI exit codes)
  e2e-web/         # bin: axum + maud + htmx over e2e-core
```

Callflow shapes + Infra shapes start as **modules inside `e2e-core`**
(`e2e-core::shapes`, `e2e-core::infra`); split into `e2e-shapes` only if compile
times demand it. `e2e-core` depends on: `scenario-harness`, `b2bua-harness`,
`failover-harness` (for the proxy spawn — see Phase B note), `sip-net`,
`seq-report`, `media`, `media-harness` (clips + classify), `sip-clock`, `serde`,
`serde_json`, `schemars`.

New workspace deps: `schemars` (JSON Schema), `axum` + `maud` + `tower-http`
(web), a WAV encoder (`hound`, or a 44-byte header hand-roll — PCM is already
`i16`). Add `e2e/runs/` to `.gitignore`.

---

## Phase A — Harness network + clock injection (the portability seam)

**Why first:** everything rests on running the *same* fluent scenario over sim and
real. Today `Harness` is welded to the sim network + paused clock.

**Tasks**
1. `crates/scenario-harness/src/agent.rs`: extract the recorder + `with_all_contracts`
   wiring from `with_transit_delay` ([:192](../../crates/scenario-harness/src/agent.rs#L192))
   into a private `fn init(name, net: Arc<dyn SignalingNetwork>, clock: Clock,
   transport_kind: TransportKind, recv_timeout: Duration) -> Self`. Keep
   `with_transit_delay` as the sim caller. Add:
   ```rust
   pub fn with_network_and_clock(
       name: impl Into<String>,
       net: Arc<dyn SignalingNetwork>,
       clock: Clock,
       transport_kind: TransportKind,
       recv_timeout: Duration,
   ) -> Self
   ```
2. Make `RECV_TIMEOUT` an instance field (sourced from Endpoint config), not a
   `const` — real clock needs a larger value; sim keeps the small one. Thread it to
   the `tokio::time::timeout` at [:675](../../crates/scenario-harness/src/agent.rs#L675).
3. `advance()` policy: portable shapes are **advance-free** (recv auto-advances
   under pause; real blocks on the real timeout). A shape that calls `advance()` is
   **sim-only** and must declare itself fake-only; the real infra builder rejects
   running it. `basic-call`/`rerouting`/`rerouting-prack` are advance-free.

**Acceptance:** an existing fluent test (e.g. `fluent_dialog`) still passes via
`with_transit_delay`, and a new throwaway test constructs a `Harness` via
`with_network_and_clock` over a `RealSignalingNetwork` on loopback and completes a
trivial send/recv.

---

## Phase B — Infra shape + SUT builder

**Concept:** an `InfraShape` builds the `Harness` (net + clock), resolves logical
roles → addresses from the **Endpoint config**, and — *for fake only* — spawns the
SUT (LB + b2bua) in-process wired b-leg→LB. For real, it spawns nothing (the SUT is
the external kind VIP).

**Tasks**
1. `e2e-core::infra`:
   ```rust
   pub trait InfraShape {
       fn id(&self) -> &str;
       fn kind(&self) -> InfraKind;            // Fake | Real
       async fn build(&self, cfg: &EndpointConfig) -> InfraRuntime;
   }
   pub struct InfraRuntime {            // handed to the Callflow shape
       pub harness: Harness,
       pub agents: BTreeMap<String, Agent>,    // alice, bob1, bob2
       pub sut_ingress: SocketAddr,            // the LB VIP — the ONLY addr agents send to
       pub lb_vip: SocketAddr,                 // for ${infra.lbVip}
       _sut_guard: Option<SutGuard>,           // keeps fake LB+b2bua tasks alive; None for real
   }
   ```
2. `fake-lsbc-b2bua`: `SimulatedSignalingNetwork` + `Clock::test_at(0)`; bind
   alice/bob1/bob2 as agents; spawn the LB via the `ProxySut` path
   ([failover-harness:728](../../crates/failover-harness/src/harness.rs#L728)) and
   the b2bua via `B2buaSut::start_with_outbound_proxy(h, "b2bua", addr, decision,
   Some(lb_vip))` so **b-leg egresses through the LB** — topology identical to real.
   Register the b2bua as a worker in the LB's `SimulatedWorkerRegistry`.
3. `real-kind`: `RealSignalingNetwork` + wall `Clock`; bind agents to
   cluster-reachable host addresses from the Endpoint config; `sut_ingress = lb_vip`
   from config; spawn nothing.
4. **Reachability invariant (real):** the LB is the sole boundary in both
   directions; agents reachable *from the LB alone*; never pod-direct. The Endpoint
   config carries `lb_vip` and the host-side agent addresses the cluster can reach.
   (Reuse the proven non-register E2E reachability — `force-b-leg-through-lb-proxy`.)
5. **Proxy-spawn reuse:** the LB spawn currently lives in `failover-harness`. Either
   (a) `e2e-core` depends on `failover-harness` and calls it, or (b) lift the proxy
   spawn into a small `sip-proxy` test-support helper. Start with (a); refactor to
   (b) if the dependency feels wrong.

**Acceptance:** `fake-lsbc-b2bua` builds an `InfraRuntime` where alice→LB→b2bua→LB→bob1
routes a bare INVITE/200/ACK; `real-kind` builds one against a loopback stub (full
cluster run is Phase A's env-gated follow-up).

---

## Phase C — Callflow shape registry + `basic-call`

**Tasks**
1. `e2e-core::shape`:
   ```rust
   pub trait CallflowShape {
       fn id(&self) -> &str;
       fn anchors(&self) -> &[Anchor];           // canonical names it publishes
       fn required_input(&self) -> &[&str];       // core/extra fields it needs
       fn media(&self) -> MediaMode;              // Off | Exchange
       async fn run(&self, rt: &mut InfraRuntime, input: &Input) -> anyhow::Result<()>;
   }
   ```
2. **Anchor publishing:** add a labeling hook to the fluent DSL so a shape tags the
   message it just sent/received: `rt.anchor("bob1", Anchor::InitialInvite, &recv)`.
   Store `(agent, anchor) → recording seq` in a side-table on the `Harness`, surfaced
   on the `RunReport`. The check engine resolves `<agent>.<anchor>` through it.
3. Canonical anchor enum: `InitialInvite, ReInvite, FirstProvisional, Answer, Ack,
   Bye, Refer` (extensible). One Rust enum = the project-wide vocabulary (ADR-0019).
4. `basic-call` shape: `alice.invite(sut_ingress).from(input.from).to(input.to)
   .ruri(input.ruri).send()`; `bob1.receive("INVITE")` → tag `bob1.initialInvite`;
   `200`/ACK → tag `answer`/`ack`; `bye` → tag `bye`. Publishes
   `[initialInvite, answer, ack, bye]`.
5. Registry: `pub fn registry() -> ShapeRegistry` returning a
   `BTreeMap<&str, Box<dyn CallflowShape>>` (manual; revisit `inventory` later).

**Acceptance (M1):** an `e2e-core` integ test runs `basic-call` over **both**
`fake-lsbc-b2bua` and `real-kind` (real against a loopback b2bua stub) from one
shape body, producing a `RunReport` each. Portability invariant proven.

---

## Phase D — JSON model + schemas

**Tasks**
1. serde + `schemars::JsonSchema` structs in `e2e-core::model`:
   - `EndpointConfig { infra_shape, roles: Map<String, AddrSpec>, lb_vip, clock,
     recv_timeout_ms }`
   - `Input { core: CoreInput, extras: Map<String, Value> }` where `CoreInput {
     from, to, ruri, headers: Map, timers: Map }`
   - `Check`, `CheckBlock` (`<agent>.<anchor>` → field→assertion), `CheckSet`
   - `TestCase { compatible_shapes: Vec<String>, input: Input, check_sets:
     Vec<String>, checks: Vec<CheckBlock> }`
   - `Campaign { cases: Vec<String>, infra_shapes: Vec<String>, concurrency:
     Map<InfraKind, usize> }`
2. Loader + **validation**: a `TestCase` is compatible with a shape iff (i) `input`
   satisfies the shape's `required_input`, and (ii) every anchor referenced by its
   checks/check-sets is published by the shape. Fail loudly at load.
3. `xtask e2e-schema`: emit `e2e/schemas/*.schema.json`; authored files carry
   `$schema`. Per-shape extras expressed via `if/then` on the `callflowShape`
   discriminator in the Test-case schema.

**Acceptance:** load + validate the M1 test as JSON; `cargo run -p xtask --
e2e-schema` writes schemas; an intentionally-incompatible test fails validation
with a precise message.

---

## Phase E — Check engine

**Tasks**
1. `e2e-core::checks`: resolve `<agent>.<anchor>` → `RecordedSipEntry` (via the
   anchor side-table) → parsed `SipMessage`.
2. Field extraction:
   - **URI headers** (From/To/PAI/PPI/R-URI/Diversion[]/Contact[]) →
     `.userInfo/.host/.port/.displayName/.tag/.param(x)` via the existing typed parse
     / Refined views.
   - **Other headers** → `.present/.absent/.regex` over raw value.
   - **Payload** → `.body.regex`.
   - **Transport** → `.source.ip/.port`, `.dest.ip/.port` from `RecordedSipEntry.from`
     /`.to` ([report.rs:29](../../crates/sip-net/src/report.rs#L29)). Optional, never
     forced.
3. Ops: `regex | eq | exists | absent`. Value sources: literal, `${input.x}`,
   `${infra.lbVip}`. A selector matching no recorded message → **fail** unless
   `optional`.
4. Output `Vec<CheckVerdict>`; merge with the RFC cross-message findings the report
   already computes; the cell fails on any non-advisory failure.

**Acceptance:** the M1 test gains `invite-identity` checks (From/PAI/R-URI on
`bob1.initialInvite`); they pass on both infra shapes; a deliberately wrong regex
fails identically on both.

---

## Phase F — Result model + persistence

**Tasks**
1. `RunResult { ids, verdict, seq_doc: SeqDoc, checks: Vec<CheckVerdict>, rfc:
   Vec<Anomaly>, media: Vec<MediaRef>, timings }` — all serde. `SeqDoc` is already a
   neutral struct; derive/confirm `Serialize`.
2. Persist `e2e/runs/<campaign>/<ts>/<case>__<shape>__<infra>/result.json` + sibling
   `*.wav`; write a `campaign.json` aggregate index (per-cell verdict).
3. Make `seq_report::render_svg(doc: &SeqDoc) -> String` **public** (currently
   private in `html.rs`); reuse `scenario_harness::report::project::sip_doc()` for
   `RunReport → SeqDoc`.

**Acceptance:** an M1 run writes a `result.json` that round-trips and a
`campaign.json` index; `render_svg` produces the same diagram the HTML report shows.

---

## Phase G — Run executor (Run Job)

**Tasks**
1. `e2e-core::run`: expand a Campaign to cells {case × compatible shape × infra};
   run with a **per-infra concurrency cap** (fake wide, real low/serial); persist
   each cell as it finishes; build the aggregate.
2. Two drivers over one core: `run_blocking(campaign) -> CampaignResult` (CLI) and
   `spawn_job(campaign) -> JobHandle { status(), subscribe() }` (web), where status
   reports per-cell progress as results land.

**Acceptance:** a 2-case × 1-shape × {fake} campaign runs to a populated run dir +
aggregate; concurrency cap honored.

---

## Phase H — `e2e-cli`

`e2e run <campaign.json> [--infra <id>...] [--case <id>...]` → `run_blocking`,
prints a summary table, **exits non-zero on any failed check / non-advisory RFC
violation**. `e2e schema` shells the xtask. `e2e validate <file>` lints JSON.

**Acceptance:** green campaign → exit 0; a campaign with a failing check → exit 1
with the failing cells listed. Usable as a CI gate.

---

## Phase I — `e2e-web` (axum + maud + htmx)

**Content-negotiated single route set** (ADR-0018) — one handler per resource,
Maud for `text/html`, `result.json` for `application/json`:

| Route | HTML | JSON |
|-------|------|------|
| `GET /campaigns` | list + launch buttons | campaign index |
| `GET /campaigns/{id}` | detail + matrix preview | campaign def |
| `POST /campaigns/{id}/runs` | 202 + redirect to run page | `{runId}` |
| `GET /runs/{id}` | live progress (htmx poll `hx-trigger="every 1s"`) | job status + verdicts |
| `GET /runs/{id}/cells/{cell}` | detail: inline SVG + checks + RFC + `<audio>` | cell `result.json` |
| `GET /cases/{id}` (+ `POST` authoring) | schema-driven form | test case JSON |

htmx swaps HTML fragments (`hx-target`); the same handlers serve JSON to the CLI/API.
Authoring form is generated from the emitted JSON Schema.

**Acceptance:** launch the M1 campaign from the browser, watch rows flip live, open a
cell, see the SVG call diagram + check verdicts; `curl -H 'Accept: application/json'`
on the same URLs returns the mirrored JSON.

---

## Phase J — Media (per-shape opt-in)

**Tasks**
1. For `MediaMode::Exchange` shapes: each agent opens a `MediaEndpoint` on the *same*
   `SignalingNetwork` ([transport.rs:139](../../crates/media/src/transport.rs#L139)),
   negotiates from the SDP, sends its deterministic reference clip (Alice 200Hz /
   Bob 110Hz, `media-harness` clips), records inbound → `recorded().pcm`.
2. Write `<agent>.received.wav` (PCM `i16` @ 8kHz); reference from `result.json`
   (`MediaRef { agent, wav, classify, rms }`). Classifier verdict (`media-harness`
   `classify`) becomes a Check (`alice hears bob`).
3. Web cell page: `<audio controls src=".../bob1.received.wav">` + classification
   badge.

**Acceptance:** a media-on `basic-call` variant produces playable `.wav`s on both
infra shapes; the "alice hears bob" check passes.

---

## Phase K — More shapes + Check sets

1. `rerouting`: alice→LB→b2bua, bob1 fails/redirects, b2bua re-targets bob2.
   Publishes per-agent anchors (`bob1.initialInvite`, `bob2.initialInvite`, `answer`,
   …). Input extra: reroute target.
2. `rerouting-prack`: + reliable provisional + PRACK (the "fake PRACK feature").
   Publishes `firstProvisional`, `prack`. Advance-free (PRACK is message-driven).
3. `e2e/checksets/invite-identity.json`: From/PAI/R-URI/Diversion on `initialInvite`
   — shared across all three shapes, proving Check-set portability.

**Acceptance:** all three shapes × both infra shapes × the shared check set run green
in one campaign; the same `invite-identity` set drives `bob1` and `bob2`.

---

## Cross-cutting risks & decisions already made

- **Portability is the keystone** — Phase A/C de-risk it first; if anything forces
  topology to differ between fake and real, stop and reconcile (the invariant is the
  whole point).
- **Real-shape reachability** is environmental (host↔cluster↔host via the LB only).
  The real half of any milestone is env-gated on a reachable kind cluster; the fake
  half always runs in CI.
- **`advance()`-using shapes are sim-only** and must declare it; the portable set
  avoids `advance()`.
- **No live Bob-side checks** (ADR-0019) — all checks are post-call over the
  recording, so fake and real verdicts are byte-identical.
- **`source.ip` is an opt-in capability**, not a forced check (the LB-VIP b-leg
  assertion is authored per test, never baked in).
- **JSON-first results**, Maud-rendered, heavy artifacts as sibling files (never
  base64-inlined).

## Suggested first PR boundaries

1. **PR-1 (Phase A):** Harness net+clock injection + `RECV_TIMEOUT` field. Pure
   `scenario-harness` refactor, existing tests stay green.
2. **PR-2 (Phases B+C):** `e2e-core` skeleton + infra builders + `basic-call` +
   the M1 portability integ test.
3. **PR-3 (Phases D+E+F):** JSON model + schemas + check engine + result
   persistence.
4. **PR-4 (Phases G+H):** run executor + CLI (CI-usable).
5. **PR-5 (Phase I):** web + content-negotiated API.
6. **PR-6 (Phases J+K):** media + the two further shapes + shared check set.
