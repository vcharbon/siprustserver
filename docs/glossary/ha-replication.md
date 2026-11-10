# HA replication glossary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "HA replication glossary".

The peer-to-peer call-replication vocabulary (ADR-0011 / `docs/plan/
on-proper-migration-of-lazy-pancake.md`). Each b2bua is *both* a replication
server (serves its change stream) and a client (pulls from every peer).

**Element**:
One replicated call replica, a `(partition, callRef)` entry in a backup's
store. "Which elements do I back up" is not a separate retrieval: it *is* the
contents of each peer's per-peer changelog (HRW 2nd-best backup, full mesh).
_Avoid_: "shard", "partition key" (the partition is just pri/bak).

**Unbacked call**:
A call that has no backup for its whole life: admitted while the backlog
toward its backup was over the high-water mark, or refused by its backup at
its memory ceiling. It runs as on a non-HA B2BUA and is lost with its primary;
`b2bua_repl_calls_unbacked` counts the live ones (ADR-0055 §3).
_Avoid_: "degraded call" (degraded fallback is a same-zone backup).

**Backlog** (toward a backup):
The entries of the change log toward one backup above the position it
reported applying, and the age of the oldest. Over a high-water mark new calls
are admitted unbacked; under the low-water marks they get a backup again. No
call is refused for it (ADR-0055 §3).

**Replication sync level** (`off` | `applied`, `B2BUA_REPL_SYNC`):
When a turn's effects leave the node, relative to the write that records them.
Every effect a call's turns send off the node (SIP, requests to outside
services, limiter releases) leaves through the call's one ordered **outbox**, a
later entry never overtaking an earlier one; the level decides only when an
entry is released. Under `off` at once, the write logged first; under
`applied` (the default) once the backup applied the write that records it, or
at the **sync cap** counted from the entry's own enqueue, whichever comes
first. A call with no live, bootstrapped backup waits for nothing; the turn
that ends a call waits for its delete, the call resident until then (ADR-0055
§3).
_Avoid_: "sync replication" (no entry waits past the cap), "hold" (every
effect goes through the outbox).

**Turn commit**:
The one tail of a call's turn (`router/commit.rs`): it persists the landed
record, reports the turn logged, takes in the turn what never leaves the node,
queues what the turn sends on the call's outbox behind the write that records
it, and ends the call once that outbox drained. How a turn persists and ends is
one **commit kind**: quiet, update, write, delete before send, delete after
drain, or defer to the primary (a takeover copy's terminal while its primary
lives). Only the commit module pushes on a call's outbox and reports a turn's
`Logged` and `Left` marks (ADR-0055 §3).
_Avoid_: "interpret" (the commit decides the persist and the end, it does not
interpret effects one by one).

**Rider**:
What the router sends behind a call's latest write outside a handler turn's
commit: the caller's 100 after the creation write, the answer to a request the
router refused, the server transactions another copy answered.
It reports no turn mark.

**Silent stream**:
A replication stream that has delivered nothing for the idle bound (15 s; a
served stream carries a `Noop` at least every 5 s). The puller leaves it and
pulls again from its watermark through the backoff (ADR-0045).
_Avoid_: "dead peer" (the peer may be alive behind a partition).

**Settled membership**:
The membership view readiness reads: synced (the informer completed its first
listing), or past the bootstrap hard timer since the supervisor started. Past
the timer a worker goes Ready with the view it has; peers join as it fills
(ADR-0045).
_Avoid_: "synced" for this bounded notion (synced is the informer's own flag).

**Failure domain** (informally **zone**):
The blast radius a **primary** and its **backup** must be split across so one
failure cannot take both. Expressed by a single configurable k8s topology key:
**`topology.kubernetes.io/zone`** by default (a rack, motivated by LAN/partition
isolation, *not* power/cooling), overridable to **`kubernetes.io/hostname`** so
the mechanism is exercisable on the kind endurance cluster (which has no zone
label). Read off each **`EndpointSlice`** endpoint's native `zone` / `nodeName`
field by the one `K8sMembership` informer (no extra RBAC, no node-get). Carried
as `Peer.failure_domain` → `WorkerEntry`.
_Avoid_: "zone" unqualified in code (overloaded with cloud AZ); "rack"/"node" as
fixed terms (the key is configurable; the domain is whatever the key names).

**Distinct-zone backup**:
The invariant that a call's **backup** sits in a different **failure domain**
than its **primary**. Enforced in **one place only**, the proxy's `w_bak`
selection (`encode_stickiness`), filtering HRW-2nd-best candidates to a foreign
domain; the b2bua never recomputes it (it echoes `w_bak` onto `topology.bak`),
so the stored **Element** lands in the chosen domain by construction. Frozen at
INVITE for the call's life, **safe** because workers are zone-pinned (one
StatefulSet per zone, hard `nodeAffinity`), so a rebooted primary always returns
to its own domain and can never drift into its backup's. **Degraded fallback:**
when no alive foreign-domain worker exists, fall back to a same-domain backup
(survives the common pod crash/restart, not a domain partition) and emit a
degraded-backup metric. With ≥3 zones this fallback only triggers if every other
domain is dead. A periodic b2bua self-scan (own zone vs `topology.bak`'s zone)
re-emits the metric as a safety net.
_Avoid_: re-deriving the constraint in the b2bua or repl layer (single picker =
the proxy).

**Unwired node** (replication off):
A b2bua with no replication store (`B2buaDeps.replication = None`, the runners'
default). It still echoes the proxy's `w_pri`/`w_bak` onto `topology` (the
cookie stays the one placement authority), but `CallState::flush`/`remove`
never touch the call store: the topology says where a backup *would* live, the
wired store says whether one is kept. The one switch is `CallState.repl`, the
replicating store and its terminate writer together, read by `flush` and by
`remove`. It constructs no replication part at all: no writer task, no
supervisor, no changelog server, no fail-back channel (its receiver exists
only alongside the retained sender, in the same arm of the core's replication
match). `b2bua_core::unwired_tests` asserts this by construction.
_Avoid_: "legacy path", "non-HA path" (they name a code age, not the switch).

**Reclaim stream** vs **Backup stream** (the two pull flows, from a node N's view):
*Reclaim* = N pulls the partition where **N is primary**, its own calls that a peer
backed up while N was down, and re-serves them (`partition=pri` on the wire; stored
`pri:{N}`; **timers armed**; N is **primary** for `(p,b)`). *Backup* = N pulls the
partition where the **peer is primary and N is its backup** (`partition=bak`; stored
`bak:{peer}`; **no timers**; N is **backup** for `(p,b)`). They run on **two separate
sockets** to distinct endpoints (no multiplexing), each with its **own watermark**, but
share one frame set and one keepalive mechanism. Direction synonyms (used in ADR-0014
prose): Reclaim = *Reverse* (backup→primary), Backup = *Forward* (primary→backup).
_Avoid_: "they ride the same pull stream" (they run on two sockets);
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
The **Reclaim** stream's bootstrap phase specifically, in which a booting primary bulk
pre-seeds its **own** calls (`pri:{self}`) from the peers that backed them up,
then re-serves them (smoothed). Gates **readiness** (first Noop).

**Backup re-subscription**:
The **Backup** stream (bootstrap + tail) a node opens against each peer once
its **Reclaim** streams are ready, to hold and keep current the calls it backs
up. Distinct socket, distinct watermark from Reclaim. Gates **readiness** too
(first Noop, ADR-0014 §11): a node reads ready once it holds the calls its
peers named it backup of.

**Takeover copy** (acting-backup live copy) (ADR-0014, ADR-0047):
The live, in-memory call a backup materialises into its own call map **when the
proxy reroutes an in-dialog request to it** (`router::materialise`), or when its
primary departs with an INVITE of the call in flight (the **departure sweep**),
distinct from the serialised backup **Element** (the `bak:{primary}` stored body).
A quiescent failed-over dialog is not made live on a survivor; it is recovered by
the rebooting primary's **reclaim**, or by a message for it.
The copy serves the dialog and owns the runtime timers; the Element is just bytes
+ a TTL.
_Avoid_: using "Element"/"replica" for the live copy, or "takeover copy" for the
stored body; "eager takeover" (no node takes over every call, quiet ones too).

**Departure sweep** (ADR-0047, ADR-0055 §5, `router::departure`): what a backup
does when a peer leaves the membership set while its flows run. It takes over
every call of the peer's backup partition that names an INVITE transaction in
flight, and every `Terminating` one with any request record open; it discharges
every ended one (ADR-0047 D8) and every `Terminating` one with nothing left to
serve; every other call is left to the reactive path by user ruling, a silent
one ending at the endpoints' timers or the replica's TTL. _Avoid_: "eager
takeover" for it.

**Undecided call** (ADR-0047, `router::undecided`): a live call still waiting on
its first routing decision (caller unanswered, no decision applied, nothing
dialled). Materialised with no message (the departure sweep, a reclaim), it is
refused 503 without Retry-After, the decision never asked again.

**Evidence** (ADR-0055 §4, `rules::evidence`): a message proving the call took a
step its record never saw. It is a final above its dialog's sequence or at a number
this side re-used, an ACK of an INVITE the record holds unanswered on a call a
takeover copy served (on any other the dialog gate drops and counts it), a
callee's BYE on an early dialog, or a 2xx naming a leg the record lacks. It is owed what RFC
3261 requires and the call is ended over every dialog the record or the evidence
names. _Avoid_: "lost round" (evidence is not limited to an INVITE).

**Possibly confirmed** (ADR-0055 §4, `rules::actions::possibly_confirmed`): a
caller dialog held after evidence proves a callee answered while the record
holds the caller unanswered: she is sent her non-2xx under the tag a relayed
2xx carried, and the dialog is BYEd on her ACK naming it or at 64·T1.

**Via-extension** (ADR-0055 §12, `stack_identity`): the `vx` parameter on the
worker's own Via (the leg, the Request-URI and the offer flag of the request,
signed with the cluster key over them and the Via's own call, leg, incarnation,
branch and method), read back off a response by a node that holds no record of
the request.

**Unheld response** (ADR-0055 §4, `unheld_response`): a response to a request of
ours for a call this node holds neither live nor as a replica it may serve:
answered as an unknown dialog would be.

**Reclaim** (active, the sole quiescent-recovery path, ADR-0014):
A returning primary *re-serving* its calls, materialising each back into the live
map, re-arming its timers (keepalive backlog **smoothed**, oldest-first; see
ADR-0014 §4), and re-flushing under the new **incarnation-gen**, not merely
pulling the bodies into `pri:` storage. The storage-only step is **re-hydration**;
reclaim is re-hydration *plus* re-serving.
_Avoid_: "reclaim" for the storage-only (re-hydration) step.

**Materialisation**: turning a stored Element into a live copy and re-arming
what this node needs to serve it: timers, in-flight transactions (seeds), and
for a takeover the self-release watch. The same step on takeover and on
reclaim; only the origin differs. _Avoid_: "hydration" for it (that is the
storage-only step).

**Seed**: a transaction the materialisation rebuilds from a **request
record**, one path for every method: a client INVITE from the sent image of an
INVITE awaiting its final, re-sent once at the seed when the record holds no
provisional for it (RFC 3261 §17.1.1.2), a Proceeding server INVITE holding the request
rebuilt from the a-leg snapshot or a pending relayed re-INVITE's record, an
Accepted server INVITE (RFC 6026) holding a 2xx a dialog retains awaiting its
ACK, and for a non-INVITE a client transaction retransmitting its sent request
under its branch and a server transaction holding its received request. A copy the call's
record takes whole at a meeting seeds the Accepted kind. The 2xx's repeats, a PRACK or a re-ACK are never a transaction's: those
are ADR-0032 obligations replicated with the call.

**Self-release** (acting-backup takeover-copy lifecycle, ADR-0014):
A backup holds a **takeover copy** only *while actively serving* the rerouted
request(s). When the **last transaction** for that call reaches a terminal state
(2xx INVITE → its ACK on the branch, else Timer L; non-INVITE → Timer J; failed leg → Timer B/F)
the txn layer emits `CallQuiesced` and the backup **self-releases**: it
`drop_local`s the live copy and stops its timers, reverting to a pure **Element**.
**Local-only**. No delete propagates; the `bak:` replica + reverse-flushed deltas
survive, so the call lives on at its reclaiming primary. There is **no** watermark
handshake and **no** time-based settle; two copies meet by containment (ADR-0055 §7).
A copy that ends a call whose primary has **departed** does not defer: it
discharges the call itself (CDR, limiter release, delete); the departure sweep
discharges an ended call deferred to a peer that then departs; and a node of the
departed ordinal that still holds the call live drops it, with no CDR, on that
delete (ADR-0047 D8).
_Avoid_: "Deactivate"/"handback" (there is no watermark handshake); "ghost backup".

**Stand-down** (ADR-0047 D7, `router::stand_down`): a takeover copy released on a
write of its primary the backup applied (a returning node's reclaim write, an answer,
an ending) when the primary's copy **contains** the copy and holds every request
record it keeps open, or on the primary's delete of the call when it holds none.
Local-only like a self-release, and its transactions are **relinquished** (deleted
with nothing sent), never orphaned. _Avoid_: "self-release" for it (that waits for the
copy's transactions to clear).

**Version vector `(p, b)`** (per-context reconciliation, ADR-0014):
Each call's **store envelope** carries `(p, b)` = `(primary_counter, backup_counter)` =
`CallTopology.{gen, bak_gen}`. **Each node bumps only its own** counter, once per
write it makes, so the *other* counter on a propagated update is the **branch
point**; a quiet turn and a meeting are not writes. **Bootstrap** (a node recovering
its own partition) applies unless the stored vector dominates; the **Forward** and
**Reverse** flows apply a write at or past the stored vector on both counters and
meet two concurrent copies by **containment**; **deletes** apply unconditionally
except Forward, which yields for the **answer** alone: an `Active` Element whose
caller was answered and to which no forward flush, taken or refused, has ever
carried the authority's own answer (ADR-0031).
_Avoid_: "call_gen LWW"/"highest gen wins".

**State stamp** (ADR-0055 §7, `CallTopology.{stamp_gen, stamp_bak_gen}`): the vector
of a copy's last write that changed a **compared field**, set only where the vector is
bumped (`CallState::update`). Every field of the call record is compared except the
closed **exempt list** (`call::helpers::compared`): the vector, each dialog's
sequences, the message and turn counters, the timer generation, the limiter's change
counter (maximum of two copies); the CDR events and the message rings (union); the
keepalive records, the keepalive and limiter-refresh ledger entries, the ladder
rungs, the trace fields (the container's).

**Containment** (ADR-0055 §7, `store::containment`): of two copies of one call
incarnation, R **contains** L when L's state stamp is at or below R's vector on both
counters, which means R read every compared write L made (replication is pull-only and no vector
is ever taken without its body). At a **meeting** the copy that contains the other is
taken whole with the other's exempt fields merged in; when each contains the other the
primary's stands; when neither does, the copies **diverged** and the meeting node ends
the call cleanly from the copy ranked higher on the lifecycle, over the union of both
copies' dialogs (`router::reclaim::diverged`). _Avoid_: "fold", "adopted counter",
"rounds fold" (no per-axis reconciliation exists; containment compares whole copies).

**Leg-id space** (ADR-0055 §7, `call::LegSpace`): one of a call's two sets of leg
ids. The node the call was created on mints `b-{n}`, any other node acting on the
call `c-{n}`, so two copies acting at once never mint one id for two legs and a
leg id names one leg in any union of their records. Which space a node mints from
is an exempt field: the container's value stands.

**Call incarnation** (`call::Call::incarnation`):
Which of the successive calls on one `call_ref` a record is. A retried INVITE
reuses its Call-ID and From tag, so it is born on the ref of the call it
retries. The value is the call's limiter key, unique over time. `(p, b)` orders
versions of one incarnation only; a write of another incarnation is a new call,
and a delete's resurrection tombstone buries the incarnation it removed, not
the ref (ADR-0014). Its timer fires, transaction timeouts, callout results and
the messages answering its Via or Contact (`ci` mark) name it, and the router
drops one naming another incarnation. Not the node's **incarnation-gen**.

**Informal aliases** (do not use in code or test names):
Conversational shorthands map onto the canonical terms of this page. "Switch to backup"
/ "go to backup" = **(reactive) takeover**; "switchback" / "back to nominal" =
**reclaim** (+ backup **self-release**); "nominal" / "the nominal node" =
**primary**. The failover DSL and test names use the canonical terms only.

**Transparent failover** vs **Disruptive failover**:
A failover (crash / drain / reboot+reclaim) injected at a **safe-point** (a
point where the call's replicated state is quiescent, the last mutation
settled to the backup) is **transparent**: the externally observable SIP
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
safe-points, with no bespoke failover test per callflow.

**Incarnation-gen** vs **callGen** (the two-generations trap):
*Incarnation-gen* (`gen`) = per-worker-restart epoch, the high word of the
**watermark**. In prod it is **boot wall-clock seconds** (monotonic across pod
restarts, so `(new_gen, 0) > (old_gen, *)`). *callGen* (`CallTopology.gen`) = the
**primary counter `p`** of a call's `(p,b)` **version vector** (ADR-0014; `b` =
`CallTopology.bak_gen`), the per-context reconciliation key. Never conflate the
incarnation-gen (per node, in the watermark) with the call's `(p,b)` (per call).
The incarnation-gen resets a puller's changelog position; it never decides which
of two live processes of one ordinal authors. The **worker identity** does, by
equality.

**Watermark** `(gen, counter)`:
A puller's cursor, **one per `(peer, flow)`**. The **Reclaim** and **Backup**
streams to the same peer carry **distinct** watermarks. It is a position in that
peer's single changelog (a `Data` frame applies iff `(gen, counter) > watermark`,
then advances), filtered to the flow's partition. It is **purely** a changelog
position, never read from or written to a call's `(p,b)` version vector (the
two-generations trap). Retained per `(ordinal, flow)` across disconnects so a
returning peer resumes rather than re-bootstraps.

**Position frame** (the drain's own signal, ADR-0031 D2):
The one client→server frame past the opening `PullRequest`: a puller answers each
post-bootstrap `Data` and every `Noop` with "I have applied everything up to
`at`". The serving node records it per `(peer, flow)` and reads it in ONE place, to tell whether a draining worker's live calls are held by the peers that back them up.
Never an apply gate, a retention hint or a readiness input.
_Avoid_: "Ack" (there is no retention frame); calling it a handshake (nothing
waits for it).

**Current flag** (`everCaughtUp`):
Set the instant the **first `Noop`** arrives on a stream. The server emits it on
the **catch-up edge** (backlog drained below one batch / to head), so it means "I
have drained this flow's backlog." **Sticky** across reconnects; a transient TCP
blip does not revert a node to NotReady. Tracked **per flow**: both the
**Reclaim** and the **Backup** stream's current flags gate readiness (ADR-0014
§11).

**Readiness states** (`NotReady → Ready ⇄ Cordoned → Draining`):
Two axes, read apart by the proxy (ADR-0050): **serving dialogs** (OPTIONS `200`,
every state but NotReady) and **accepting new calls** (`X-Readiness: new=accept`, or
`new=refuse; cause=backend|cordoned|draining`). NotReady answers OPTIONS `503`; in k8s
the `/ready` HTTP probe reports NotReady / Ready / Draining and ignores acceptance. A
**backend gate** open on a `Ready` node reports `cause=backend`. **Cordoned** = set and
cleared by an operator (`/admin/cordon`, gauge `b2bua_cordoned`): refuses every new
INVITE with a bare `503` before any decision, its record that of a call no decision
reached; serves every dialog; no timer; `/ready` reads it Ready. A cordon is refused
(`409`) while NotReady or Draining; SIGTERM drains a cordoned node as a Ready one.
**Release-all** = an operator's `POST /admin/release-all`, never implied by a cordon
or a SIGTERM: every answered call ends with no BYE (an owed ACK is sent, a still-ringing
leg CANCELled; its record closed under `operator_release`, its holds released, its
replica deleted, a later request drawing `481`); a call in setup is aborted (`503` to
the caller, CANCEL to the callee). Counted on `b2bua_operator_released_total{kind}`.
**Ready** = every **Reclaim** stream to a
*reachable* peer has hit its first `Noop` (best-effort, hard-timer bounded so a
dead/slow peer cannot hang readiness). **Backup** streams are opened only *after*
`Ready` and **never gate it** (fire-and-forget; observable via the store + metrics,
not a readiness sub-state). **Draining** = latched on SIGTERM, or on the worker's own observation that its
endpoint is withdrawn; terminal. The drain exits on the first of: live calls
cleared (`quiescent`), every live call's backup holding its changelog head once
the floor `B2BUA_DRAIN_MIN_MS` has passed, for a worker that has observed its own
withdrawal (`caught_up`), or the grace `B2BUA_DRAIN_GRACE_MS`, as `grace` for a
worker that is not withdrawn (quiescence-or-grace) and as `grace_peers_behind`
for one that is, which means a flush window was lost (ADR-0031 D2, D6). The four
reasons are the `reason` label of `b2bua_drain_exits_total`. Before exiting the
worker flushes its queued limiter releases within `B2BUA_DRAIN_RELEASE_FLUSH_MS`,
re-checks a clean exit after the flush, and gives up, counted, what is left
(ADR-0031 D2, ADR-0040 decision 9). Every exit then closes the CDR sink within
`B2BUA_DRAIN_CDR_FLUSH_MS`: the queued records are written and the broker's
outstanding confirms awaited; what the bound leaves counts in `cdr_dropped_total`.
