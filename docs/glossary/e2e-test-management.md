# E2E test-management vocabulary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "E2E test-management vocabulary".

The language of the authored end-to-end documents (Test cases, Check sets,
Endpoint configs) and the cell result record a campaign cell writes. See
[ADR-0018](../../docs/adr/0018-e2e-test-management-architecture.md)
(architecture) and [ADR-0019](../../docs/adr/0019-e2e-check-model-and-anchors.md)
(checks/anchors). It keeps apart *what the call does* (Callflow shape), *where
the endpoints live* (Endpoint config) and *what data fills the call* (Test
case), and labels each run with the topology it ran under (Infra shape).

**Callflow shape**:
A compiled-Rust, registered message-sequence template (basic call, re-routing,
re-routing + PRACK) parameterised over a declared **input-data schema** and the
**checks** it supports. Built on the fluent `Harness`/`Agent` DSL. Selected, not
authored; a new shape is Rust + redeploy. Per ADR-0021 a shape is declared
exactly once, as a **Shape descriptor** in the unified open `ShapeRegistry`
(`e2e-model`), and may carry a **load body** (see
[Loadgen fusion vocabulary](loadgen-fusion.md)).
_Avoid_: "scenario" (quadruple-overloaded: the Rust `Scenario` struct, the TS
instance, the JSON **Test case**, and loadgen's `--scenario` selector all
collide; a registered shape is a **shape**); "framework".

**Infra shape**:
The label naming the topology and clock a run was under: **fake** (Alice/Bob*,
LB and b2bua all in-process on `SimulatedSignalingNetwork`, paused clock) or
**real** (Alice/Bob* are real-socket **Test Agents**, the **SUT** is an external
kind cluster, wall clock). An **Endpoint config** carries it as `infraShape`, a
cell result as `cell.infra`.
_Avoid_: "topology"/"network" unqualified (the latter is the `SignalingNetwork`
trait).

**Test Agent**:
A scenario-driven UA the framework controls (Alice, Bob1, Bob2), a simulated
endpoint under a fake **Infra shape**, a real UDP socket under a real one.
_Avoid_: "client"/"UA"/"endpoint" unqualified.

**SUT** (system under test):
What the **Test Agents** exercise (the LSBC LB + b2bua), spawned in-process on
the sim fabric under a fake **Infra shape**, an external kind ip:port under a real
one. Never scenario-driven. The **LB is the sole boundary in BOTH directions**:
Test Agents reach the SUT only via the LB VIP (a-leg in), and the SUT reaches Test
Agents only *back through the LB* (b-leg out, `B2BUA_OUTBOUND_PROXY` = LB VIP),
**never pod-direct**. So in the real shape Alice/Bob must be reachable *from the LB
alone*; a b2bua contacting Bob directly is the known NAT-failure mode this
invariant exists to forbid (see the `force-b-leg-through-lb-proxy` finding).
"Fully fake" / "fully real" name the two ends of this.

**Test case** (a.k.a. **test data**):
A committed JSON file = input data (From/To/R-URI, timers, specific header
content) + **checks** + the **list of compatible Callflow shapes** it can drive
(validated against each shape's declared input schema at load). Per ADR-0021 the same document also feeds the load
fleet (attached to a mix entry via `--case` / `case=`), optionally carrying a
**binding pool** and `allowViolations`.
_Avoid_: "scenario" (the overloaded word; loadgen's `--scenario`
flag selects a registered **shape** by id, never a Test case).

**Endpoint config**:
A JSON file binding logical roles (alice, bob, charlie, lb…) to concrete
addresses, plus the agent recv-timeout and an optional egress policy, labelled
with the **Infra shape** it addresses. The load generator reads it
(`--endpoint-config`).

**Check**:
A declared assertion in a **Test case** / **Check set**, evaluated **post-call
over the recorded trace by the same engine as the 77 RFC rules**. A check is a
*parameterised audit rule contributed by the JSON*, surfaced as a verdict/anomaly
in the report exactly like an RFC finding, and identical across fake & real Infra
shapes. Checks are declarative and cannot influence the flow (no live Bob-side
branching). A check keys off a `<agent>.<anchor>` (**Message anchor**) and asserts
over a field: **URI-bearing headers** (From/To/PAI/PPI/R-URI/Diversion[]/Contact[])
expose typed helpers `.userInfo/.host/.port/.displayName/.tag/.param(x)`; **any
other header** gets `.present/.absent/.regex`; the **payload** gets `.body.regex`;
and the **transport source/dest** (`source.ip/.port`,
`dest.ip/.port`, read from the recorded `from`/`to` `SocketAddr`) is assertable too,
so a Test case *may optionally* verify which IP a message came from (e.g. b-leg
source == LB VIP), a capability, never a forced/default check. Values may bind a
Test-case input (`${input.from}`) or an Infra value (`${infra.lbVip}`).
_Avoid_: a separate live-assertion path in the Callflow shape (rejected; one path keeps
fake/real uniform and reuses the audit engine).

**Message anchor**:
A name from a **canonical, project-wide vocabulary** (`initialInvite`, `reInvite`,
`firstProvisional`, `answer`, `ack`, `bye`, `refer`, …) that a **Callflow shape**
*publishes* for each message it produces. Checks bind to `<agent>.<anchor>` rather
than a step index, so a **Check set** is portable across every shape that publishes
the anchors it references (validated at load; a referenced-but-unpublished anchor
is a compatibility error).
_Avoid_: per-shape free-string anchors (drift), raw `{method, nth}` selectors
(fragile).

**Check set**:
A committed, reusable JSON bundle of **checks** keyed by **Message anchor** (e.g.
`loadgen-invite-identity` = From-tag and Max-Forwards assertions on `initialInvite`).
Referenced by name from any **Test case** whose shape publishes the needed anchors,
so "verify the identity headers" is authored once and shared everywhere.
_Avoid_: re-authoring the same identity checks inline per Test case.
