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
INVITE for the call's life. **Degraded fallback:** when no alive foreign-domain
worker exists, fall back to a same-domain backup (survives the common pod
crash/restart, not a domain partition) and emit a degraded-backup metric.
_Avoid_: re-deriving the constraint in the b2bua or repl layer (single picker =
the proxy).

**Forward replication** vs **Reverse replication**:
*Forward* = a primary pushing its own calls to the peer that backs them up
(`partition=bak` on the wire). *Reverse* = a rebooted primary reclaiming calls
its backup mutated while it was down (`partition=pri`). Both ride the **same**
pull stream — `partition` tags which is which.

**Re-hydration** (bootstrap):
A booting primary's bulk pre-seed: it scans a backup's `bak:{primary}` callRef
keys (brief lock), streams the bodies in ~128 batches, captures `W = changelog
head at scan start`, seeds its watermark to `W`, and keeps tailing. Correctness
is bootstrap + conservative watermark + tail, *not* snapshot consistency.

**Backup re-subscription**:
The steady-state tail a node opens against each peer (`PullRequest(Replog,
since=W)`) to keep its backups current. Same stream as re-hydration; bootstrap
is just its bulk prefix.

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
A puller's per-peer cursor; it applies a `Data` frame iff `(gen, counter) >
watermark`, then advances. Retained per ordinal across disconnects so a
returning peer resumes rather than re-bootstraps.

**Current flag** (`everCaughtUp`):
Set the instant the head `Noop` arrives on a peer's tail — "I have drained this
peer's backlog." **Sticky** across reconnects; a transient TCP blip does not
revert a node to NotReady.

**Readiness states** (`NotReady → Ready → Draining`):
Self-reported via OPTIONS (`200` / `503 not-ready` / `503 draining`) and, in
k8s, via the `/ready` HTTP probe. **Ready** = re-hydration done for all
*reachable* peers (best-effort, hard-timer bounded) **and** every forward pull
is current. **Draining** = latched on SIGTERM; terminal.

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
