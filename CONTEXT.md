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
