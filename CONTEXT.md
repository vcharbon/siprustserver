# siprustserver

The Rust port of [sipjsserver](./portsource/sipjsserver). This glossary pins
the **migration vocabulary** — the words we use to talk about porting one
codebase to another. The **SIP domain glossary** (Worker, partition,
replication channel, leg model, overload, …) is not duplicated here; it lives
in [portsource/sipjsserver/CONTEXT.md](./portsource/sipjsserver/CONTEXT.md)
and carries over unchanged.

## Language

### Migration units

**Layer**:
One row of [MIGRATION_STATUS.md](./MIGRATION_STATUS.md) — a cohesive subsystem
ported as a unit (message layer, network layer, call model, rule engine,
limiter). In the Rust workspace, one layer maps to one **crate**.
_Avoid_: "Effect Layer" (a source-side DI construct — say **layer interface**
for that), "module" (a layer is a crate, not a Rust `mod`).

**Slice**:
One increment of work that advances one or more **layers**, recorded as a dated
entry in MIGRATION_STATUS. Slice 1 = the pure message layer. A layer may take
several slices.
_Avoid_: "phase" (overloaded — the source used "Phase 1/2" for the native
parser rollout).

**Layer interface** (a.k.a. **DI seam**):
The Rust `trait` a layer exposes so consumers depend on the abstraction, not a
concrete impl — the idiom that replaces the source's Effect `Layer`/`ServiceMap`
dependency injection. Multiple impls (and future decorator wrappers) implement
the same trait and are swapped at the boundary.
_Avoid_: "service" (Effect term), "interface" unqualified.

### Test porting

**Parity oracle**:
A second, independent implementation of a **layer interface** kept *only* for
tests, to cross-check the production impl. For the message layer the oracle is
`rvoip-sip-core`, checked against the ported `CustomParser`.
_Avoid_: "reference parser" (the production custom parser is the reference for
behaviour; the oracle is the looser cross-check).

**Compliance matrix**:
The test that runs every parser impl over a shared fixture corpus (RFC 4475
torture, CVE regressions, RFC 5118 IPv6, param-grammar gaps) and asserts each
impl's expected accept/reject. The Rust port of `parser-compliance.test.ts`;
it is how the **parity oracle** is exercised.
_Avoid_: "parity test" (the source's `parity` wrapper is a distinct
SignalingNetwork construct, deferred to a later layer).

**Frozen corpus**:
A committed set of grammar-valid inputs generated once by `abnfgen` from the
vendored ABNF grammars, replayed deterministically by the ABNF fuzz test.
Regenerated only on demand via `cargo run -p xtask -- abnf-regen`.
_Avoid_: "fuzz corpus" (implies live mutation each run — ours is frozen).

### Typed messages

**Refined view**:
A borrowed newtype wrapping a base `SipRequest`/`SipResponse` that encodes a
context guarantee proven once at a boundary — `InDialogRequest` (From/To tags
present), `InviteRequest` (single Contact), `SipResponseTagged` (To-tag
present). `Deref`s to the base, so it adds guarantees without hiding anything.
Built at the router/dispatch boundary; downstream code is never defensive.
_Avoid_: "subtype", "cast" (it is a validated projection, not a downcast).

**TypedHeader**:
The trait an integrator implements to add a typed, parsed custom header
(`msg.typed::<H>()`) — the open, compile-time replacement for the source's
declaration-merging + `SipHeaderRegistry.register`. Distinct from the raw
`get_header(name) -> Vec<&str>` escape hatch, which serves unknown headers only.
_Avoid_: "header registry" (there is no global mutable registry anymore).

**Policy rejection** / **Buggy rejection**:
The two classes a parser rejection of a grammar-valid input falls into.
*Policy* = a documented ADR-0007 strictness rule (expected; the grammar is
looser than the parser). *Buggy* = matches no known policy → a real parser
bug. A clean ABNF run has zero buggy rejections and zero silent misparses.

## HA replication glossary

The peer-to-peer call-replication vocabulary (ADR-0011 / `docs/plan/
on-proper-migration-of-lazy-pancake.md`). Each b2bua is *both* a replication
server (serves its change stream) and a client (pulls from every peer).

**Element**:
One replicated call replica — a `(partition, callRef)` entry in a backup's
store. "Which elements do I back up" is not a separate retrieval: it *is* the
contents of each peer's per-peer changelog (HRW 2nd-best backup, full mesh).
_Avoid_: "shard", "partition key" (the partition is just pri/bak).

**Failure domain** (informally **zone**):
The blast radius a **primary** and its **backup** must be split across so one
failure cannot take both. Expressed by a single configurable k8s topology key:
**`topology.kubernetes.io/zone`** by default (a rack, motivated by LAN/partition
isolation — *not* power/cooling), overridable to **`kubernetes.io/hostname`** so
the mechanism is exercisable on the kind endurance cluster (which has no zone
label). Read off each **`EndpointSlice`** endpoint's native `zone` / `nodeName`
field by the one `K8sMembership` informer — no extra RBAC, no node-get. Carried
as `Peer.failure_domain` → `WorkerEntry`.
_Avoid_: "zone" unqualified in code (overloaded with cloud AZ); "rack"/"node" as
fixed terms (the key is configurable — the domain is whatever the key names).

**Distinct-zone backup**:
The invariant that a call's **backup** sits in a different **failure domain**
than its **primary**. Enforced in **one place only** — the proxy's `w_bak`
selection (`encode_stickiness`), filtering HRW-2nd-best candidates to a foreign
domain; the b2bua never recomputes it (it echoes `w_bak` onto `topology.bak`),
so the stored **Element** lands in the chosen domain by construction. Frozen at
INVITE for the call's life — **safe** because workers are zone-pinned (one
StatefulSet per zone, hard `nodeAffinity`), so a rebooted primary always returns
to its own domain and can never drift into its backup's. **Degraded fallback:**
when no alive foreign-domain worker exists, fall back to a same-domain backup
(survives the common pod crash/restart, not a domain partition) and emit a
degraded-backup metric. With ≥3 zones this fallback only triggers if every other
domain is dead. A periodic b2bua self-scan (own zone vs `topology.bak`'s zone)
re-emits the metric as a safety net.
_Avoid_: re-deriving the constraint in the b2bua or repl layer (single picker =
the proxy).

**Reclaim stream** vs **Backup stream** (the two pull flows, from a node N's view):
*Reclaim* = N pulls the partition where **N is primary** — its own calls that a peer
backed up while N was down — and re-serves them (`partition=pri` on the wire; stored
`pri:{N}`; **timers armed**; N is **primary** for `(p,b)`). *Backup* = N pulls the
partition where the **peer is primary and N is its backup** (`partition=bak`; stored
`bak:{peer}`; **no timers**; N is **backup** for `(p,b)`). They run on **two separate
sockets** to distinct endpoints (no multiplexing), each with its **own watermark**, but
share one frame set and one keepalive mechanism. Direction synonyms (used in ADR-0014
prose): Reclaim = *Reverse* (backup→primary), Backup = *Forward* (primary→backup).
_Avoid_: "they ride the same pull stream" (was true pre-simplification; now two sockets);
"Forward/Reverse" as the primary names in new code (prefer Reclaim/Backup stream).

**Bootstrap phase** (of either stream):
The bulk store-scan prefix every flow runs on a cold connect: it scans the peer's
keyspace for its partition (`bak:{caller}` for **Reclaim**-serve, `pri:{self}`
filtered to the caller's backups for **Backup**-serve), streams the bodies in
batches, captures `W = changelog head at scan start`, then hands off to the tail.
The scan is mandatory (not a cold changelog pull): a created-then-quiescent call's
changelog entry compacts away, so only the store scan is a complete snapshot.
Correctness is bootstrap + conservative watermark + tail, *not* snapshot consistency.

**Re-hydration**:
The **Reclaim** stream's bootstrap phase specifically — a booting primary bulk
pre-seeding its **own** calls (`pri:{self}`) from the peers that backed them up,
then re-serving them (smoothed). Gates **readiness** (first Noop).

**Backup re-subscription**:
The **Backup** stream (bootstrap + tail) a node opens against each peer *after*
`Ready` to keep the calls it backs up current. Distinct socket, distinct
watermark from Reclaim; never gates readiness (metrics-only).

**Takeover copy** (acting-backup live copy) — *reactive only* (ADR-0014):
The live, in-memory call a backup materialises into its own call map **when the
proxy reroutes an in-dialog request to it** (`hydrate_from_replica`) — distinct
from the serialised backup **Element** (the `bak:{primary}` stored body). There is
**no** eager/membership-driven takeover: a quiescent failed-over dialog is not
made live on a survivor; it is recovered by the rebooting primary's **reclaim**.
The copy serves the dialog and owns the runtime timers; the Element is just bytes
+ a TTL.
_Avoid_: using "Element"/"replica" for the live copy, or "takeover copy" for the
stored body; "eager takeover" (removed).

**Reclaim** (active) — the sole quiescent-recovery path (ADR-0014):
A returning primary *re-serving* its calls — materialising each back into the live
map, re-arming its timers (keepalive backlog **smoothed**, oldest-first; see
ADR-0014 §4), and re-flushing under the new **incarnation-gen** — not merely
pulling the bodies into `pri:` storage. The storage-only step is **re-hydration**;
reclaim is re-hydration *plus* re-serving.
_Avoid_: "reclaim" for the storage-only (re-hydration) step.

**Self-release** (acting-backup takeover-copy lifecycle) — replaces Activate/Deactivate (ADR-0014):
A backup holds a **takeover copy** only *while actively serving* the rerouted
request(s). When the **last transaction** for that call reaches a terminal state
(2xx INVITE → Timer H after the ACK; non-INVITE → Timer J; failed leg → Timer B/F)
the txn layer emits `CallQuiesced` and the backup **self-releases**: it
`drop_local`s the live copy and stops its timers, reverting to a pure **Element**.
**Local-only** — no delete propagates; the `bak:` replica + reverse-flushed deltas
survive, so the call lives on at its reclaiming primary. There is **no** watermark
handshake and **no** time-based settle; correctness rests on `(p,b)` causality.
_Avoid_: "Deactivate"/"handback" (the removed watermark handshake); "ghost backup".

**Version vector `(p, b)`** (per-context reconciliation) — replaces LWW-by-`gen` (ADR-0014):
Each call carries `(p, b)` = `(primary_counter, backup_counter)` =
`CallTopology.{gen, bak_gen}`. **Each node bumps only its own** counter on a local
mutation, so the *other* counter on a propagated update is the **branch point**.
Merge is direction-aware: **Forward** (primary→backup) and **Bootstrap** apply
unless the stored vector dominates (follower defers to authority); **Reverse**
(backup→primary) applies iff `p_in == p_cur && b_in > b_cur` (untouched-by-primary
since the backup branched, genuinely newer backup mutation); **deletes** apply
unconditionally both ways. Closes the latent equal-`gen` divergence the single
counter suffered.
_Avoid_: "call_gen LWW"/"highest gen wins" (the reverse path is now the meaningful
guard; forward is monotone-authority).

**Informal aliases** (do not use in code or test names):
Conversational shorthands map onto the canonical terms above — "switch to backup"
/ "go to backup" = **(reactive) takeover**; "switchback" / "back to nominal" =
**reclaim** (+ backup **self-release**); "nominal" / "the nominal node" =
**primary**. The failover DSL and test names use the canonical terms only.

**Transparent failover** vs **Disruptive failover**:
A failover (crash / drain / reboot+reclaim) injected at a **safe-point** — a
point where the call's replicated state is quiescent (the last mutation has
settled to the backup) — is **transparent**: the externally observable SIP
exchange (what each UA sees) and the final CDR disposition are identical to a
no-failover baseline; tags/CSeq stay correct. A failover injected mid-replication
(a message not yet propagated) or under a **partition** is **disruptive**: it has
visible external impact and is asserted against bespoke expectations, never the
transparency oracle. The failover test matrix covers transparent cases first
(one uniform oracle, easy to multiply); disruptive cases are a separate, smaller
set with per-case expectations.

**Safe-point** (failover safe-point):
A point in a call scenario, declared by the scenario author, where replicated
state is quiescent so an injected crash/recovery is expected to be a
**transparent failover**. A callflow is a step list with safe-points between
steps; the failover-matrix driver auto-injects `(kill | drain)` ×
`(stay-dead | reboot-no-traffic | reboot-after-takeover)` at each safe-point and
asserts transparency. New callflows get failover coverage by declaring their
safe-points — no bespoke failover test per callflow.

**Incarnation-gen** vs **callGen** (the two-generations trap):
*Incarnation-gen* (`gen`) = per-worker-restart epoch, the high word of the
**watermark** — in prod it is **boot wall-clock seconds** (monotonic across pod
restarts, so `(new_gen, 0) > (old_gen, *)`). *callGen* (`CallTopology.gen`) = the
**primary counter `p`** of a call's `(p,b)` **version vector** (ADR-0014; `b` =
`CallTopology.bak_gen`), the per-context reconciliation key. Never conflate the
incarnation-gen (per node, in the watermark) with the call's `(p,b)` (per call).

**Watermark** `(gen, counter)`:
A puller's cursor, **one per `(peer, flow)`** — the **Reclaim** and **Backup**
streams to the same peer carry **distinct** watermarks. It is a position in that
peer's single changelog (a `Data` frame applies iff `(gen, counter) > watermark`,
then advances), filtered to the flow's partition. It is **purely** a changelog
position — never read from or written to a call's `(p,b)` version vector (the
two-generations trap). Retained per `(ordinal, flow)` across disconnects so a
returning peer resumes rather than re-bootstraps.

**Current flag** (`everCaughtUp`):
Set the instant the **first `Noop`** arrives on a stream — the server emits it on
the **catch-up edge** (backlog drained below one batch / to head), so it means "I
have drained this flow's backlog." **Sticky** across reconnects; a transient TCP
blip does not revert a node to NotReady. Tracked **per flow**: the **Reclaim**
stream's current flag gates readiness; the **Backup** stream's is **metrics-only**.

**Readiness states** (`NotReady → Ready → Draining`):
Self-reported via OPTIONS (`200` / `503 not-ready` / `503 draining`) and, in
k8s, via the `/ready` HTTP probe. **Ready** = every **Reclaim** stream to a
*reachable* peer has hit its first `Noop` (best-effort, hard-timer bounded so a
dead/slow peer cannot hang readiness). **Backup** streams are opened only *after*
`Ready` and **never gate it** (fire-and-forget; observable via the store + metrics,
not a readiness sub-state). **Draining** = latched on SIGTERM; terminal.

## HTTP call-decision adaptation

The vocabulary for the external decision layer (the future HTTP adapter; today
the in-process scripted adapter) that tells the B2BUA *how to treat a call* —
the substrate for numbering-plan services. The decision request carries the
inbound call context (R-URI, From/To, all non-structural `X-*` headers, body);
the response is a **call treatment**. See [ADR-0017](./docs/adr/0017-http-call-treatment-and-header-ownership.md).

**Call treatment**:
The single outcome the decision layer returns for a call, at *any* hop — the
initial decision and every failover hop draw from the **same** closed set:
**route** (bridge to a destination, with header/identity rewrites), **redirect**
(emit a 3xx to the caller with a Contact list), **reject** (a final failure
response the layer authors — code, reason-phrase, extra headers), and **relay**
(pass the last attempted b-leg's actual failure response back to the caller
verbatim). One enum, reused by the new-call and failure callbacks, so "all the
info of how to treat the call" is one vocabulary.
_Avoid_: separate outcome sets for new-call vs failover (they are unified);
"terminate" (the old fixed-486 path — superseded by per-call **reject**/**relay**).

**Header ownership**:
For each SIP header on a B2BUA-authored message, exactly one of two parties is
its author: the **decision layer** (HTTP) or the **core engine** (B2BUA). The
matrix, for a normal service: the decision layer owns the **From** and **To**
*URIs* (the numbers) but **never the tags**; it owns **Contact** *only* on a
**redirect** (302) response (the redirect targets); it freely *adds*
non-structural headers (PAI, PANI, any `X-*`). The core engine always owns the
**tags**, **Via**, **CSeq**, **Call-ID**, **branch**, **Max-Forwards**, and
**Contact** on every non-302 message. From/To URI and 302-Contact are the only
fields that flip to HTTP ownership for a normal service.
_Avoid_: letting the flat header-update map rewrite a **structural** header
(From/To/Via/CSeq/Call-ID/Contact) — it only *appends*; structural rewrites go
through typed identity fields, and tags are never HTTP-settable.

**Reroute plan** (the carried failover list):
The ordered remainder of destinations (plus the **on-exhausted** behaviour) a
call still has to try, stashed by the decision layer inside the call's opaque
**callback context** — the field the platform treats as a token it round-trips
untouched (a real HTTP backend would hold this state itself). On a b-leg
failure the failure callback pops the head, returns a **route** to it, and
re-stashes the tail; on an empty list it returns the plan's **on-exhausted**
**call treatment** (a **reject**, **relay**, or **redirect**). Because the plan
rides the *existing* opaque callback-context string (and `ext`), it is
serialised and replicated with the call for free — no new wire field, no
positional-codec change (ADR-0008), so it survives failover by construction.
_Avoid_: a structured top-level "routes" field on the call (would be a new
replicated field); parsing the callback context anywhere but the decision layer
(the platform keeps it opaque).

## Call state-machine services (Rust shape)

The Rust formalisation of the source's **callflow service** + **phase machine**
(both defined in [portsource/sipjsserver/CONTEXT.md](./portsource/sipjsserver/CONTEXT.md),
carried over unchanged) into explicit, doc-generated per-call state machines.
See [ADR-0016](./docs/adr/0016-callflow-service-state-machines.md).

**Machine** (state machine):
A named, single-cursor **per-call** state selector. A rule declares the machine
it belongs to, the states it is active in, and the transitions it may cause; the
rule is a candidate for the engine only while its machine sits in one of those
states. A machine selects *which rules are live* — it is a guard over the
existing layer-ranked first-match engine, **not** a separate dispatcher.
_Avoid_: "phase machine" in Rust code (the source's name for the same idea —
keep it for prose / the TS source); "FSM" unqualified.

**Global call machine**:
The always-on machine present on every call (= the existing `CallModelState`
`Active → Terminating → Terminated`, enriched only if a service needs finer
call-lifecycle states). Its published inbound interface is the call-lifecycle
subset of the action union (`BeginTermination`/`TerminateCall`/`Merge`/`Split`/…) —
a **service machine** influences the call by *emitting* one of those, never by
mutating call state directly (one hop, service → global).
_Avoid_: "main SM" / "core machine" (collides with `CORE_LAYER`).

**Service machine**:
The one machine a callflow **service** owns; its graph is exactly the service's
own rules. Leg SIP lifecycle (`LegState`) and `active_peer` are **data the rules
peek at**, not a machine — there is no wired per-leg machine tier.

**Machine cursor** (`sm_cursors`):
The current-state *label* of a machine. The single home for machine state, one
entry per machine per call — uniform across in-tree and out-of-crate services, so
the engine, the doc generator, observability, and HA all read it the same way.
The service's typed slice (in-tree) or opaque `ext` (integrator) holds the
associated *data* only, never the label.
_Avoid_: storing the state label in the service's data slice as well (the
mirror/slice divergence the single home exists to prevent).

**Tracked side effect**:
An outward consequence a service rule's handler emits that the framework makes a
service **declare** (type-forced via `sm_rule!`), **verifies** against what the
handler actually emits (`emitted ⊆ declared`, by category — the same
declare-then-check contract as `transitions`), and **renders** on the generated
diagram. There are exactly three: a **leg message**, a **call-lifecycle
command**, and a **guard timer**. They are categorised by the **attribution
principle** — *who authors the message*, not which bytes hit the wire:
a message the service rule authors is a *leg message* it owns; a wire message the
**global call machine** generates after the service merely commanded it is the
*global* machine's effect, not the service's.
_Avoid_: calling a cursor move or a local data write a "side effect" — see below.

**Leg message**:
A **tracked side effect**: a SIP message the service rule authors toward a leg —
originated (the MRF INVITE, an MSCML `INFO`, a re-INVITE to A/C), relayed
(transparent passthrough), a final response, or an unreliable provisional (early
media). It carries the **canonical `Method`** (the shared sip-stack method type)
plus a free, **unenforced label** (`"MSCML <play>"`, `"re-INVITE → A"`). The rule
owns the *semantic* payload (method, body, target, the headers it cares about);
the **core engine** fills the mechanical SIP layer (Contact, From-tag, Via, CSeq).
_Avoid_: attributing a leg message to the service when the *global call machine*
generated it (e.g. the BYEs that fall out of a `call-lifecycle command`).

**Call-lifecycle command**:
A **tracked side effect**: the one synchronous **service → global** hop (ADR-0016
X3) — the call-lifecycle subset of the action union
(`BeginTermination`/`TerminateCall`/`Merge`/`Split`). The service emits the
command; the **global call machine** interprets it and owns any wire messages it
then generates (those appear on the *global* diagram, never re-documented on the
service's).
_Avoid_: re-listing the global machine's downstream BYEs as service leg messages.

**Guard timer**:
A **tracked side effect**: a service safety/watchdog timer the rule **arms** (or
cancels) — e.g. the REFER re-INVITE-answer watchdog. (Re-uses the existing
`TimerType`; "armed" is the canonical verb, per the HA glossary above.)

**Machine deactivation**:
A machine leaving the call's active set by having its **cursor removed**
(`ClearState`, the declarative inverse of `SetState`). It is **not** a tracked
side effect — it is the machine's own terminal move, drawn as the transition to
the terminal state `[*]`. A `SetState` is likewise not a side effect: it *is* the
transition, already drawn as the edge. Pure data bookkeeping (CDR events, tag-map
writes, typed-slice writes, async-HTTP kicks) is **auto-allowed** — invisible to
the diagram and not something an author declares.
_Avoid_: "clear state" in prose for the *concept* (say a machine **deactivates**
/ reaches its **terminal** state); `ClearState` is the action that realises it.

**K8sMembership**:
The real `topology::Membership` source (S11): a kube EndpointSlice informer over
the headless worker Service. *Ready* endpoints → `Peer{ordinal = pod name, host
= pod IP}`; written once, consumed by both proxy and b2bua (ADR-0011 X7 / ADR-0012
D4). Its delta consumers **self-heal**: a `Lagged` broadcast re-reconciles from
`snapshot()` (never `return`s) and a periodic snapshot reconcile makes a missed
delta non-fatal (ADR-0012 D1/D2). The repl puller additionally resolves a
**stable per-pod DNS name fresh per connect** as defense-in-depth (ADR-0012 D3);
the proxy reaches workers by the informer-fed Pod IP (ADR-0012 D4). Consistency
is enforced on *identity + membership source*, not *address representation*
(ADR-0012 D5).

## E2E test-management vocabulary

The language of the test-management website + CLI (Axum/Maud/htmx front-end and a
CI/CD CLI over one shared run-core) that authors, launches, and displays
end-to-end SIP tests. See [ADR-0018](./docs/adr/0018-e2e-test-management-architecture.md)
(architecture) and [ADR-0019](./docs/adr/0019-e2e-check-model-and-anchors.md)
(checks/anchors). Its organising idea is **four orthogonal axes** — *what the
call does* (Callflow shape), *what the topology is* (Infra shape), *where the
endpoints live* (Endpoint config), and *what data fills the call* (Test case) —
that a fused `#[tokio::test]` currently interleaves and this layer pulls apart.

**Callflow shape**:
A compiled-Rust, registered message-sequence template (basic call, re-routing,
re-routing + PRACK) parameterised over a declared **input-data schema** and the
**checks** it supports. Built on the fluent `Harness`/`Agent` DSL. Selected — not
authored — from the website; a new shape is Rust + redeploy.
_Avoid_: "scenario" (triple-overloaded — the Rust `Scenario` struct, the TS
instance, and the JSON **Test case** all collide); "framework".

**Infra shape**:
A compiled-Rust topology + clock a Callflow shape runs under — **fake** (Alice/
Bob1/Bob2/LB/b2bua all in-process on `SimulatedSignalingNetwork`, paused clock)
or **real** (Alice/Bob* are real-socket **Test Agents**, the **SUT** is an
external kind cluster, wall clock). The *same* Callflow shape runs unchanged over
any Infra shape — the portability invariant.
_Avoid_: "topology"/"network" unqualified (the latter is the `SignalingNetwork`
trait).

**Test Agent**:
A scenario-driven UA the framework controls (Alice, Bob1, Bob2) — a simulated
endpoint under a fake **Infra shape**, a real UDP socket under a real one.
_Avoid_: "client"/"UA"/"endpoint" unqualified.

**SUT** (system under test):
What the **Test Agents** exercise (the LSBC LB + b2bua) — spawned in-process on
the sim fabric under a fake **Infra shape**, an external kind ip:port under a real
one. Never scenario-driven. The **LB is the sole boundary in BOTH directions**:
Test Agents reach the SUT only via the LB VIP (a-leg in), and the SUT reaches Test
Agents only *back through the LB* (b-leg out — `B2BUA_OUTBOUND_PROXY` = LB VIP),
**never pod-direct**. So in the real shape Alice/Bob must be reachable *from the LB
alone*; a b2bua contacting Bob directly is the known NAT-failure mode this
invariant exists to forbid (see the `force-b-leg-through-lb-proxy` finding).
"Fully fake" / "fully real" name the two ends of this.

**Test case** (a.k.a. **test data**):
A committed JSON file = input data (From/To/R-URI, timers, specific header
content) + **checks** + the **list of compatible Callflow shapes** it can drive
(validated against each shape's declared input schema at load). The unit a user
authors from the website.
_Avoid_: "scenario" (the overloaded word this replaces).

**Endpoint config**:
A JSON file binding an **Infra shape**'s logical roles (alice, bob1, sut…) to
concrete addresses + clock + recv-timeout. One per Infra shape (fake = loopback
sim ports; real = kind ip:ports). The `RECV_TIMEOUT` knob is part of it.

**Check**:
A declared assertion in a **Test case** / **Check set**, evaluated **post-call
over the recorded trace by the same engine as the 77 RFC rules** — a check is a
*parameterised audit rule contributed by the JSON*, surfaced as a verdict/anomaly
in the report exactly like an RFC finding, and identical across fake & real Infra
shapes. Checks are declarative and cannot influence the flow (no live Bob-side
branching). A check keys off a `<agent>.<anchor>` (**Message anchor**) and asserts
over a field: **URI-bearing headers** (From/To/PAI/PPI/R-URI/Diversion[]/Contact[])
expose typed helpers `.userInfo/.host/.port/.displayName/.tag/.param(x)`; **any
other header** gets `.present/.absent/.regex`; the **payload** gets `.body.regex`
(SDP-aware helpers later); and the **transport source/dest** (`source.ip/.port`,
`dest.ip/.port`, read from the recorded `from`/`to` `SocketAddr`) is assertable too,
so a Test case *may optionally* verify which IP a message came from (e.g. b-leg
source == LB VIP) — a capability, never a forced/default check. Values may bind a
Test-case input (`${input.from}`) or an Infra value (`${infra.lbVip}`).
_Avoid_: a separate live-assertion path in the Callflow shape (rejected — keeps
fake/real uniform and reuses the audit engine).

**Message anchor**:
A name from a **canonical, project-wide vocabulary** (`initialInvite`, `reInvite`,
`firstProvisional`, `answer`, `ack`, `bye`, `refer`, …) that a **Callflow shape**
*publishes* for each message it produces. Checks bind to `<agent>.<anchor>` rather
than a step index, so a **Check set** is portable across every shape that publishes
the anchors it references (validated at load — a referenced-but-unpublished anchor
is a compatibility error).
_Avoid_: per-shape free-string anchors (drift), raw `{method, nth}` selectors
(fragile).

**Check set**:
A committed, reusable JSON bundle of **checks** keyed by **Message anchor** (e.g.
`invite-identity` = From + PAI + R-URI + Diversion assertions on `initialInvite`).
Referenced by name from any **Test case** whose shape publishes the needed anchors,
so "verify the identity headers" is authored once and shared everywhere.
_Avoid_: re-authoring the same identity checks inline per Test case.

**Campaign**:
A JSON batch that crosses {**Test cases**} × {their compatible **Callflow
shapes**} × {**Infra shapes**}; launching it materialises one **Run** per cell and
collects the results.
_Avoid_: the TS meaning of "campaign" (a SUT-config *variant* — b2bonly / proxy+
b2b / HA); here a campaign is a test *batch*.

**Run** / **Result**:
One executed (**Test case**, **Callflow shape**, **Infra shape**) cell and its
recorded artifacts (SVG call diagram, wire trace, **check** verdicts, RFC audit,
received RTP). Generated, not committed.
