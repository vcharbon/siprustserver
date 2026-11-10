# Call state-machine services (Rust shape)

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "Call state-machine services (Rust shape)".

The Rust formalisation of the source's **callflow service** + **phase machine**
(both defined in [portsource/sipjsserver/CONTEXT.md](../../portsource/sipjsserver/CONTEXT.md),
carried over unchanged) into explicit, doc-generated per-call state machines.
See [ADR-0016](../../docs/adr/0016-callflow-service-state-machines.md).

**Machine** (state machine):
A named, single-cursor **per-call** state selector. A rule declares the machine
it belongs to, the states it is active in, and the transitions it may cause; the
rule is a candidate for the engine only while its machine sits in one of those
states. A machine selects *which rules are live*. It is a guard over the
existing layer-ranked first-match engine, **not** a separate dispatcher.
_Avoid_: "phase machine" in Rust code (the source's name for the same idea;
keep it for prose / the TS source); "FSM" unqualified.

**Global call machine**:
The always-on machine present on every call (= the existing `CallModelState`
`Active → Terminating → Terminated`, enriched only if a service needs finer
call-lifecycle states). Its published inbound interface is the call-lifecycle
subset of the action union (`BeginTermination`/`TerminateCall`/`Merge`/`Split`/…),
a **service machine** influences the call by *emitting* one of those, never by
mutating call state directly (one hop, service → global).
_Avoid_: "main SM" / "core machine" (collides with `CORE_LAYER`).

**Service machine**:
The one machine a callflow **service** owns; its graph is exactly the service's
own rules. Leg SIP lifecycle (`LegState`) and `active_peer` are **data the rules
peek at**, not a machine. There is no wired per-leg machine tier.

**Machine cursor** (`sm_cursors`):
The current-state *label* of a machine. The single home for machine state, one
entry per machine per call, uniform across in-tree and out-of-crate services, so
the engine, the doc generator, observability, and HA all read it the same way.
The service's typed slice (in-tree) or opaque `ext` (integrator) holds the
associated *data* only, never the label.
_Avoid_: storing the state label in the service's data slice as well (the
mirror/slice divergence the single home exists to prevent).

**Tracked side effect**:
An outward consequence a service rule's handler emits that the framework makes a
service **declare** (type-forced via `sm_rule!`), **verifies** against what the
handler actually emits (`emitted ⊆ declared`, by category, the same
declare-then-check contract as `transitions`), and **renders** on the generated
diagram. There are exactly three: a **leg message**, a **call-lifecycle
command**, and a **guard timer**. They are categorised by the **attribution
principle**, *who authors the message*, not which bytes hit the wire:
a message the service rule authors is a *leg message* it owns; a wire message the
**global call machine** generates after the service merely commanded it is the
*global* machine's effect, not the service's.
_Avoid_: calling a cursor move or a local data write a "side effect", see below.

**Leg message**:
A **tracked side effect**: a SIP message the service rule authors toward a leg,
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
X3), the call-lifecycle subset of the action union
(`BeginTermination`/`TerminateCall`/`Merge`/`Split`). The service emits the
command; the **global call machine** interprets it and owns any wire messages it
then generates (those appear on the *global* diagram, never re-documented on the
service's).
_Avoid_: re-listing the global machine's downstream BYEs as service leg messages.

**Guard timer**:
A **tracked side effect**: a service safety/watchdog timer the rule **arms** (or
cancels), e.g. the REFER re-INVITE-answer watchdog. Declared by its `TimerType`
(a service timer: its owning service and its key) and the kind of **timer
scope** its key guards; every key the rule arms or cancels is held to a
declaration. ("Armed" is the canonical verb, per the
[HA replication glossary](ha-replication.md).)

**Machine deactivation**:
A machine leaving the call's active set by having its **cursor removed**
(`ClearState`, the declarative inverse of `SetState`). It is **not** a tracked
side effect. It is the machine's own terminal move, drawn as the transition to
the terminal state `[*]`. A `SetState` is likewise not a side effect: it *is* the
transition, already drawn as the edge. Pure data bookkeeping (CDR events, tag-map
writes, typed-slice writes, async-HTTP kicks) is **auto-allowed**, invisible to
the diagram and not something an author declares.
_Avoid_: "clear state" in prose for the *concept* (say a machine **deactivates**
/ reaches its **terminal** state); `ClearState` is the action that realises it.

**Lifecycle event** (ADR-0042):
What the engine tells every active service at the end of a turn, whichever
rule claimed the turn: an **attempt** started (with the routing answer it
dials) or ended (with its outcome), a leg ended (with how). A service hears
one through a machine-bound rule matching it; its handler may write call ext,
move its own machine's cursor and arm or cancel its own service timers,
nothing else. A machine the turn deactivated still hears the events the turn
caused, for its ext writes.
_Avoid_: "observer rule" (no rule runs beside a claim; a service hears
lifecycle events); "callback".

**K8sMembership**:
The real `topology::Membership` source: a kube EndpointSlice informer over
the headless worker Service. Every endpoint in the slice → `Peer{ordinal = pod
name, host = pod IP, ready, terminating}`; written once, consumed by both proxy
and b2bua (ADR-0011 X7 / ADR-0012 D4), each with its own predicate: the proxy
routes to *ready* peers, the b2bua pulls every peer (ADR-0031 D1). Its delta consumers **self-heal**: a `Lagged` broadcast re-reconciles from
`snapshot()` (never `return`s) and a periodic snapshot reconcile makes a missed
delta non-fatal (ADR-0012 D1/D2). The repl puller additionally resolves a
**stable per-pod DNS name fresh per connect** as defense-in-depth (ADR-0012 D3);
the proxy reaches workers by the informer-fed Pod IP (ADR-0012 D4). Consistency
is enforced on *identity + membership source*, not *address representation*
(ADR-0012 D5).

**Withdrawal**:
The instant a worker's endpoint leaves the routable set (`ready=false` in its
EndpointSlice, or the endpoint gone). The proxy drops the ordinal and tombstones
its address; the process is untouched but **observes** it: the informer shows a
worker its own endpoint, and once it has seen itself routable, a `terminating` or
absent own endpoint (never a readiness flap: `Draining` is terminal) latches
`Draining` whether or not SIGTERM has come (ADR-0031 D6; `is_withdrawn`
on the supervisor, the `b2bua_withdrawn_running` gauge), and the worker
drains and exits as on SIGTERM (D5). A static membership
never shows a worker itself. ADR-0031 states the four ways out: graceful, abrupt,
vanished, restarted in place.

**Worker identity**:
One boot of one worker ordinal: the orchestrator's pod UID (the informer's
`targetRef.uid`, its own from the downward API as `POD_UID`), or a per-boot
token in a static pool. Membership binds it to the ordinal (`topology::Peer`);
a changed identity is a changed binding, handled as a changed address. Compared
for equality only, never ordered (ADR-0055 §6). A request record names the
worker that relayed it by ordinal and identity (`call::RelayOwner`). Not the
**incarnation-gen**, and not the **call incarnation**.

**Superseded**:
A worker that saw its ordinal bound to its own worker identity and then sees it
bound to another: the orchestrator recreated the ordinal while this process
still runs (a forced delete). Sticky and terminal: the worker stops authoring at
once (no SIP, no write a peer can pull, no CDR) and exits without a drain
(ADR-0055 §6; `is_superseded` on the core). Another identity seen before its
own is a predecessor still listed, not a supersession.

**Terminating member**:
A worker the orchestrator has begun to remove and that still runs. Still a
**replication peer** (pulled until it is gone from membership), not a
**routing target** (ADR-0031 D1: one membership snapshot, two predicates;
*pullable* is "still a member", *routable* is "ready").
_Avoid_: "withdrawn" for the replication event (**withdrawal** is routing-only;
the replication event is the peer **departing**, parked).

**Departed-address tombstone**:
The proxy registry's memory of an address that left the set: resolvable by
`lookup_by_address` as its last ordinal with health `Dead` for Timer H, keyed by
address, so a response still arriving from it reverse-fails to the cookie's
backup; a join at the address clears it. Never affects ordinal resolution. A
dialog-creating request from a tombstoned address is Record-Routed with a cookie
naming that ordinal and an alive backup (ADR-0031 D4).
