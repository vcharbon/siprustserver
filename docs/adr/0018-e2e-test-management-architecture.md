# E2E test-management architecture

## Status

accepted — amended 2026-06-14 (layout-owned **egress rewrite**; see Decision);
amended 2026-07-03 (the shape registry is now the unified **open**
`ShapeRegistry` in `e2e-model`, shared with the load surface — ADR-0021).

## Context

We want a website (and a CI/CD CLI) to author, launch, and display end-to-end SIP
tests. Today a test is a single fused `#[tokio::test]` in which the topology
(agents), the message sequence (steps), the addresses, the message data
(headers/timers), and the assertions are all interleaved in one imperative
function. That fusion blocks the goals: it can't be authored from a web form, the
same test can't be run over two topologies, and there is no data/result the site
can read.

## Decision

Pull a test apart into **four orthogonal axes**:

- **Callflow shape** — compiled Rust, registered: the message-sequence template
  (basic call, re-routing, re-routing + PRACK), built on the fluent
  `Harness`/`Agent` DSL, parameterised over a declared input schema (a shared
  **core** of From/To/R-URI/headers/timers + optional per-shape **extras**).
- **Infra shape** — compiled Rust: the topology + clock the shape runs under.
  **fake** = Alice/Bob1/Bob2/LB/b2bua all in-process on `SimulatedSignalingNetwork`
  under a paused clock; **real** = Alice/Bob* are real-socket **Test Agents**, the
  **SUT** (LSBC LB + b2bua) is an external kind cluster under a wall clock.
- **Endpoint config** — JSON: binds an Infra shape's logical roles to concrete
  addresses + clock + `RECV_TIMEOUT`.
- **Test case** — JSON: input data + **checks** + the list of **compatible
  Callflow shapes** it can drive. The unit a user authors from the website.

The **same Callflow shape runs unchanged over any Infra shape** — the portability
invariant. It holds because both signaling and media ride the one
`Arc<dyn SignalingNetwork>` seam, and under a paused runtime a `recv` park
auto-advances virtual time, so a send/expect shape is already clock-agnostic
(paused-sim auto-advances; real-clock blocks on the real timeout). The single
clock knob, `RECV_TIMEOUT`, lives in Endpoint config.

**Egress rewrite (amendment 2026-06-14).** Topologies reach the callee by
*different* conventions — the real cluster needs a proprietary `X-Api-Call`
b-leg pin, the register front proxy resolves a registered AOR from the
Request-URI, the fake LB's scripted engine just routes. A shape must not bake in
any of these, or the portability invariant is a fiction. So the *layout* owns
the resolution of **every** logical callee role and the transform from a shape's
**logical** INVITE to the **wire** INVITE:

- `InfraRuntime::callee(role)` resolves ANY callee — the a-leg target, a reroute
  candidate, a REFER transfer target — to a `CalleeTarget { uri, addr }` (the
  registered AOR or `sip:<role>@<addr>`, plus the pin address). A shape never
  hard-codes `cfg.addr("bob2")` or an AOR.
- Each Infra shape declares an `EgressPolicy` (`Transparent` / `ApiCallPin` /
  `RegistrarAor`); `InfraRuntime::outgoing_invite(callees, …)` takes an **ordered
  candidate list** (primary + failover targets), folds in the Test case's core
  From/To/R-URI, then applies the policy's `EgressRewrite` (R-URI override + extra
  headers). One pinned callee → an `X-Api-Call` `destination`; several → an
  `X-Api-Call` `routes` failover plan (ADR-0017), so rerouting is expressed
  generically rather than hard-wired into one infra's engine.

A shape no longer branches on the infra; the layout may also do call setup the
convention requires (the register layout pre-REGISTERs the callees in `build`).
This is what lets `basic-call` / `basic-call-media` run over the register front
proxy — retiring the bespoke `register-call*` shapes — and is the seam any future
"rewrite the outgoing INVITE per topology" need (other proprietary headers,
forced routes) extends.

Execution is an **in-process registry** (shape id → parameterised fn) — since
ADR-0021 the unified open `ShapeRegistry` in **`e2e-model`**, whose descriptors
**`e2e-core`** attaches its functional bodies to. Two thin front-ends consume
it: an **`e2e-web`** Axum +
Maud + htmx server (authoring, launching, display) and an **`e2e-cli`** binary for
CI/CD that runs a campaign headless and **exits non-zero on any failed check or
non-advisory RFC violation**. A launch is an **async Run Job** with a per-infra
**concurrency cap** (fake fans out wide; real is capped low so the live SUT is not
overloaded); the site polls live progress via htmx, the CLI blocks to completion.

A **Campaign** (JSON) crosses {Test cases} × {their compatible Callflow shapes} ×
{Infra shapes}; each cell is one **Run**. Results are **JSON-first**: persist the
neutral `SeqDoc` + check verdicts + RFC findings + media refs as `result.json`;
Maud renders the call diagram (reusing seq-report's `SeqDoc → SVG`); the JSON API
serves the same `result.json` via **content negotiation on one route set** (HTML
for `Accept: text/html`, JSON for `application/json`) so HTML and API cannot drift.
Heavy artifacts (RTP `.wav`) are sibling files referenced by `result.json`, never
inlined. Committed inputs live under `e2e/{cases,checksets,infra,campaigns,schemas}`;
generated runs under a gitignored `e2e/runs/…` (kept out of `target/` so they
survive `cargo clean`). Every JSON type derives schemars `JsonSchema`; an `xtask`
emits committed `.schema.json` and files carry `$schema` for editor completion.

## Considered options

- **Shell out to `cargo test` / a subprocess per run.** Rejected: config injection
  is awkward, scenarios can't be cleanly enumerated or parameterised, and IPC is
  file-scraping. (Subprocess isolation was the one merit; not worth the friction
  for a filesystem-based v1.)
- **Fully data-driven steps in JSON** (author new sequences without Rust).
  Rejected as the primary model: it loses the fluent auto-generation
  (Via/tags/CSeq), and forking/PRACK/re-INVITE are awkward as flat data. New
  *sequences* stay Rust + redeploy; new *tests* are pure JSON.

## Consequences

- New message sequences require Rust + redeploy; the website authors **Test
  cases**, not **Callflow shapes**.
- Real-shape runs make the **LB the sole boundary in both directions**: Test
  Agents reach the SUT only via the LB VIP (a-leg in), and the SUT reaches them
  only back through the LB (b-leg out — `B2BUA_OUTBOUND_PROXY` = LB VIP), **never
  pod-direct**. Alice/Bob must therefore be reachable *from the LB alone*; a b2bua
  contacting Bob directly is the known NAT-failure mode this forbids. This is a
  hard invariant of the `real-kind` Infra shape, not merely "reuse the existing
  harness" — the Endpoint config and the SUT's outbound-proxy setting must enforce
  it.
- Glossary in `CONTEXT.md` → "E2E test-management vocabulary".
