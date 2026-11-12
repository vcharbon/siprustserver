# siprustserver

Index of the vocabulary of the Rust port of [sipjsserver](./portsource/sipjsserver): one line per term, grouped by area. Grep a term here.
Each line's full definition, with its _Avoid_ list, its examples and its relationships, is on the area page the section links.
The SIP domain glossary (Worker, partition, replication channel, leg model, overload) lives in [portsource/sipjsserver/CONTEXT.md](./portsource/sipjsserver/CONTEXT.md).

## Language

Full definitions: [docs/glossary/language.md](docs/glossary/language.md).

- **Layer**: one row of MIGRATION_STATUS, a subsystem ported as a unit; one layer is one crate. Avoid: Effect Layer, module.
- **Slice**: one dated increment of work advancing one or more layers; a layer may take several. Avoid: phase.
- **Layer interface** (a.k.a. **DI seam**): the Rust trait a layer exposes so consumers depend on it, not on an impl. Avoid: service.
- **Parity oracle**: a second parser kept only for tests (`rvoip-sip-core`), diffed seed by seed against the production one.
- **Compliance matrix**: the test running the production parser over the shared fixture corpus, asserting accept or reject. Avoid: parity test.
- **Frozen corpus**: grammar-valid inputs generated once by `abnfgen`, replayed by the ABNF fuzz test. Avoid: fuzz corpus.
- **Callflow choreography** (`scenario_harness::callflow`): the inline helper a test calls to run INVITE/180/200/ACK; not a Callflow shape.
- **Refined view**: a borrowed newtype over a request or response encoding a guarantee proven once at a boundary. Avoid: subtype, cast.
- **TypedHeader**: the trait an integrator implements to add a typed, parsed custom header (`msg.typed::<H>()`).
- **Strict reading** / **Lenient reading**: the two modes of one reader; strict refuses non-RFC wire bytes, lenient keeps what reads. ADR-0053.
- **Reading role**: what a parser is built to read (`ingress`, `built`, `tool`, `read_stored`), named by its constructor. ADR-0053.
- **Exemption**: a non-RFC shape a deployment accepts, named in `SIP_EXEMPTIONS`, consulted only by the ingress gate. ADR-0001.
- **Single-valued header**: a header carried on one line at most, read as `msg.header::<H>()`; a second line is refused. ADR-0053.
- **Policy rejection** / **Buggy rejection**: a parser refusal of a grammar-valid input, by a documented rule or by a bug.

## Retransmission vocabulary

Full definitions: [docs/glossary/retransmission.md](docs/glossary/retransmission.md).

- **Ladder**: the scheduled re-sends of one message and their schedule, owned by a transaction or a dialog obligation. ADR-0032. Avoid: retry.
- **Rung**: one step of a ladder; rung 0 is the original send, and the index is the ladder's whole state.
- **Retained emission**: the exact serialized datagram of a message that owes repeats, replicated; a rung re-sends these bytes. ADR-0032.
- **Obligation**: the key that discharges a dialog-level ladder (`AckOf2xx`, `AwaitPrack`) when its ACK or PRACK arrives. Avoid: watchdog.
- **Provisional record**: one reliable provisional the call received, where the PRACK owed for it stands and how it was shown.
- **Request record**: one request a dialog of the call holds open, by origin: relayed, own, held or received. ADR-0055.
- **INVITE exchange**: what an INVITE's request record carries past its final: its ACK, a 2xx awaiting its ACK, a queued BYE.
- **Subscription record**: a subscription this stack notifies on a dialog, its state and its NOTIFY awaiting a final. ADR-0055.
- **Method table**: what differs by method for a request record when its dialog ends or its relay meets an ended leg.
- **PRACK disposition**: what a received PRACK draws from the face its provisional was shown: relayed, answered 200, or refused 481.
- **Dialog owed answer**: the RFC 3264 answer this side still owes a dialog's responder, with the message carrying it; one per dialog.
- **Message ring**: the capped, replicated per-leg history of the distinct SIP messages a leg received or sent. Avoid: history, log.
- **Decision log**: every decision the decision layer returned and this stack applied, one mark each; the ordinal stamps messages.
- **Termination record**: who ended the call and why (cause, by leg, cut), written once by the first termination. Avoid: reason.
- **Snooze**: a timer fire a rule puts off by one short step (`RuleAction::SnoozeTimer`), counted per key. Avoid: defer, hold.
- **Admission ladder**: the ordered rungs that may refuse a new INVITE, ending at the CPS bucket; a refusal draws one 503. ADR-0037.
- **Copy** (of a new INVITE): an initial INVITE matching a live call's Call-ID, From-tag and CSeq on another branch; answered 482.
- **Capacity gate**: the worker's normal and emergency ceilings on calls, transactions and RSS, and the backup ceilings. ADR-0037.
- **CPS bucket**: the worker's token bucket on new-dialog INVITEs, the admission ladder's last rung. Avoid: overdraft, debt.
- **Load-shedding primitives**: the token bucket, EWMA, `LoadSampler` seam and `Retry-After` policy every shedding process shares.
- **Proxy refusal**: the front proxy's own 503 at its self-gate or for a failed worker selection, remembered for 64·T1.
- **First worker view**: a starting proxy's first worker listing plus one completed OPTIONS round; `/readyz` waits for it. ADR-0050.
- **Metric catalogue**: a process's Prometheus families declared as data, one entry per family, checked by `metric-contract`.
- **Dispatch class**: what an event is to its call's dispatch queue; its table row states room, caps, permit pool and owed answer.
- **Owed answer**: what the router owes the sender of a request its call's queue discarded unrun, one per discard. Avoid: discard hook.

## HA replication glossary

Full definitions: [docs/glossary/ha-replication.md](docs/glossary/ha-replication.md).

- **Element**: one replicated call replica, a `(partition, callRef)` entry in a backup's store. Avoid: shard, partition key.
- **Unbacked call**: a call with no backup for its life, admitted over the backlog high-water mark or shed by its backup. ADR-0055.
- **Backlog** (toward a backup): change-log entries above the position a backup reported; over high water, new calls run unbacked. ADR-0055.
- **Replication sync level** (`B2BUA_REPL_SYNC`): `off` or `applied`, when a turn's outbox entries leave relative to their write. ADR-0055.
- **Turn commit**: the one tail of a call's turn (`router/commit.rs`): persist, queue sends on the outbox, end the call. ADR-0055.
- **Rider**: what the router sends behind a call's latest write outside a turn's commit (the 100, a refusal); it reports no mark.
- **Silent stream**: a replication stream silent for the 15 s idle bound; the puller re-pulls from its watermark. ADR-0045.
- **Settled membership**: the membership view readiness reads, informer synced or the bootstrap hard timer passed. ADR-0045.
- **Failure domain** (informally **zone**): the blast radius a primary and its backup are split across, one k8s topology key.
- **Distinct-zone backup**: a call's backup sits in another failure domain, enforced only by the proxy's `w_bak` selection.
- **Unwired node**: a b2bua with no replication store (`B2buaDeps.replication = None`), building no replication part.
- **Reclaim stream** vs **Backup stream**: the two pull flows; N pulls its own partition back, or a partition it backs up. ADR-0014.
- **Bootstrap phase**: the bulk store-scan prefix either stream runs on a cold connect, before the changelog tail.
- **Re-hydration**: the Reclaim stream's bootstrap phase, a booting primary pre-seeding its own calls from peers (storage only).
- **Backup re-subscription**: the Backup stream a node opens to each peer once its Reclaim streams are ready. ADR-0014.
- **Takeover copy**: the live call a backup materialises for a rerouted request or a departed primary's INVITE. ADR-0014, ADR-0047.
- **Departure sweep**: a backup taking over a departed peer's calls with an INVITE in flight and discharging ended ones. ADR-0047.
- **Undecided call**: a live call still waiting on its first routing decision; materialised without a message, refused 503. ADR-0047.
- **Evidence**: a message proving the call took a step its record never saw; the call ends over every dialog named. ADR-0055.
- **Possibly confirmed**: a caller dialog held after evidence shows a callee answered while the caller is unanswered. ADR-0055.
- **Via-extension**: the signed `vx` parameter on the worker's own Via, read back off a response by a node with no record. ADR-0055.
- **Unheld response**: a response for a call this node holds neither live nor as a replica it may serve. ADR-0055.
- **Reclaim** (active): a returning primary re-serving its calls (live map, timers, new incarnation-gen); re-hydration plus serving. ADR-0014.
- **Materialisation**: turning a stored Element into a live copy, re-arming timers and seeds; same on takeover and reclaim. Avoid: hydration.
- **Seed**: a transaction materialisation rebuilds from a request record, one path for every method.
- **Self-release**: a backup dropping its takeover copy, locally, once the call's last transaction ends. ADR-0014. Avoid: handback.
- **Stand-down**: a takeover copy released locally on a primary write that contains it, or on the primary's delete. ADR-0047.
- **Version vector `(p, b)`**: a call's `(primary_counter, backup_counter)`, each node bumping only its own per write. ADR-0014.
- **State stamp**: the vector of a copy's last write that changed a compared field (all but the exempt list). ADR-0055.
- **Containment**: R contains L when L's state stamp is at or below R's vector; at a meeting the container is taken. ADR-0055.
- **Leg-id space**: `b-{n}` on the creating node, `c-{n}` elsewhere, so two copies never mint one id for two legs. ADR-0055.
- **Call incarnation**: which of the successive calls on one `call_ref` a record is, the limiter key. ADR-0014. Not incarnation-gen.
- **Informal aliases**: "switch to backup" = takeover, "switchback" = reclaim, "nominal" = primary; never in code or test names.
- **Transparent failover** vs **Disruptive failover**: injected at a safe-point, a failover is invisible to the UAs and the CDR; else disruptive.
- **Safe-point**: a scenario point its author declares where replicated state is quiescent; the failover matrix injects there.
- **Incarnation-gen** vs **callGen**: the per-boot epoch in the watermark vs a call's primary counter `p`; never conflate. ADR-0014.
- **Watermark** `(gen, counter)`: a puller's changelog cursor, one per (peer, flow), never a call's version vector.
- **Position frame**: the puller's "applied up to `at`" frame, read only by a draining worker's exit. ADR-0031. Avoid: Ack.
- **Current flag** (`everCaughtUp`): set on a stream's first `Noop`, sticky, per flow; gates readiness. ADR-0014.
- **Readiness states** (`NotReady → Ready ⇄ Cordoned → Draining`): serving dialogs vs accepting calls; Release-all is an operator act. ADR-0050.

## HTTP call-decision adaptation

Full definitions: [docs/glossary/http-call-decision.md](docs/glossary/http-call-decision.md).

- **Call treatment**: the one outcome the decision layer returns at any hop: route, redirect, reject or relay. Avoid: terminate.
- **Header ownership**: which party authors each header: the decision layer (From/To URIs, 302 Contact, added headers) or the core.
- **Reroute plan**: the ordered failover destinations and on-exhausted treatment, carried in the opaque callback context. ADR-0008.

## Outbound backend vocabulary

Full definitions: [docs/glossary/outbound-backend.md](docs/glossary/outbound-backend.md).

- **Backend client**: one backend's rotated `reqwest` client, resolved target, in-flight cap, breaker and retry rule (`http_net`).
- **Resolved target**: a backend's address, or its name looked up on the request path; forget, refresh, serve stale.
- **Backend breaker**: opens when its window saw requests sent and no HTTP response at all; any answer to a health probe closes it.
- **Decision backend failure**: no usable answer from the decision backend; the stack answers a bare 503. A reject is not one.
- **Backend gate** (OPTIONS): an open backend breaker refusing new calls on a Ready node, `X-Readiness: new=refuse; cause=backend`.

## Call state-machine services (Rust shape)

Full definitions: [docs/glossary/call-state-machine-services.md](docs/glossary/call-state-machine-services.md).

- **Machine**: a named single-cursor per-call state selector guarding which rules are live; not a dispatcher. Avoid: phase machine, FSM.
- **Global call machine**: the always-on `Active → Terminating → Terminated` machine; services drive it by lifecycle commands.
- **Service machine**: the one machine a callflow service owns, its graph exactly the service's rules; legs are data, not machines.
- **Machine cursor** (`sm_cursors`): the current-state label of a machine, one entry per machine per call, the one home of state.
- **Tracked side effect**: an outward effect a service rule declares and the framework verifies: leg message, lifecycle command, guard timer.
- **Leg message**: the tracked side effect of a SIP message the service rule authors toward a leg, with method and free label.
- **Call-lifecycle command**: the tracked side effect of the service → global hop (`BeginTermination`, `TerminateCall`, `Merge`, `Split`). ADR-0016.
- **Guard timer**: the tracked side effect of a service safety timer the rule arms or cancels, declared by `TimerType` and scope.
- **Machine deactivation**: a machine leaving the active set by its cursor removed (`ClearState`); not a side effect.
- **Lifecycle event**: what the engine tells every active service at a turn's end: attempt started or ended, leg ended. ADR-0042.
- **K8sMembership**: the EndpointSlice informer behind `topology::Membership`; the proxy routes to ready peers, the b2bua pulls all. ADR-0012.
- **Withdrawal**: a worker's endpoint leaving the routable set; a worker observing its own latches `Draining`. ADR-0031.
- **Worker identity**: one boot of one worker ordinal (pod UID or per-boot token), compared for equality only. ADR-0055.
- **Superseded**: a worker seeing its ordinal bound to another identity; it stops authoring and exits without a drain. ADR-0055.
- **Terminating member**: a worker being removed that still runs: still a replication peer, no routing target. ADR-0031.
- **Departed-address tombstone**: the proxy's memory of a departed address, `Dead` for Timer H, so late responses fail over. ADR-0031.

## Call admission set vocabulary

Full definitions: [docs/glossary/call-admission-set.md](docs/glossary/call-admission-set.md).

- **Admission set**: the limiter entries (id and cap) a call holds under its key; every change replaces it whole. Avoid: reservation, permit.
- **Held**: the set the limiter last stated it holds for the call, under that statement's change number.
- **Target**: the set the call's latest admit asked for; the held set after a refused or superseded change.
- **Counted**: the held set is not empty; the call refreshes its lease while Active.
- **Change number**: the rising number every admit of a call carries; the limiter refuses one not above what it knows.
- **Runs uncounted**: the call runs on an id the held set lacks (fail open, or a set a service or route moved it onto).

## Call model vocabulary

Full definitions: [docs/glossary/call-model.md](docs/glossary/call-model.md).

- **Attempt**: one routing answer that dials and the legs it dials, with a cause, a ring deadline and an outcome. ADR-0042. Avoid: try.
- **Ring deadline** (`no_answer_ms`): how long a dialled leg may ring unanswered, run from its first 180–189. Avoid: no-answer timeout.
- **Timer scope**: what a timer guards (call, attempt, leg, obligation, setup); leaving the scope retires its timers. ADR-0042, ADR-0055.

## Dialog model vocabulary

Full definitions: [docs/glossary/dialog-model.md](docs/glossary/dialog-model.md).

- **Dialog identity**: the leg's Call-ID plus the dialog's our tag and peer tag (RFC 3261 §12). Avoid: identity tag, local/remote tag.
- **Dialog state**: `Early`, `Confirmed`, `Ending`, `Ended` or retired; a dialog record stays until its call is released.
- **Retired dialog**: an early dialog the call let go of without a BYE, such as a losing callee fork. Avoid: pruned, dropped.
- **Fronting**: a caller-facing dialog fronts the callee dialogs it relays. Avoid: tag map.
- **Outbound INVITE**: the dialog-creating INVITE a leg sent; every dialog a response to it creates is minted from it.
- **Leg state**: read off the leg's dialogs and its INVITE's facts, never stored (`Leg::state`).
- **Dialog gate**: the engine step resolving a message to its dialog before any rule, refusing what the dialog does not take.
- **Not now**: a relayed request no dialog can carry yet while its own dialog lives: 500 with `Retry-After`.

## Call reaper vocabulary

Full definitions: [docs/glossary/call-reaper.md](docs/glossary/call-reaper.md).

- **Call reaper**: the module releasing every admitted call exactly once, with one CDR, through panic handling and a liveness sweep.
- **Release obligation**: a consequence of admitting a call, discharged once at release from the `Call` alone: the owed CDR, one limiter release.
- **Release queue**: the worker's unreplicated queue of limiter releases, sent batched and given up after one lease. ADR-0040.
- **Last-touched stamp**: the node-local per-call activity timestamp, the reaper's only liveness input.
- **Acting-backup terminal contract**: an acting backup ends a call only when serving an explicit BYE or CANCEL.

## E2E test-management vocabulary

Full definitions: [docs/glossary/e2e-test-management.md](docs/glossary/e2e-test-management.md).

- **Callflow shape**: a registered compiled-Rust message-sequence template over an input schema and checks. ADR-0021. Avoid: scenario.
- **Infra shape**: the label naming the topology and clock a run was under: fake (in-process, paused clock) or real (kind cluster).
- **Test Agent**: a scenario-driven UA the framework controls (Alice, Bob1, Bob2). Avoid: client, UA, endpoint.
- **SUT** (system under test): the LB and b2bua the Test Agents exercise; the LB is the sole boundary in both directions.
- **Test case** (a.k.a. **test data**): a committed JSON of input data, checks and compatible shapes; it also feeds the load fleet. ADR-0021.
- **Endpoint config**: a JSON binding logical roles to addresses, recv-timeout and egress policy, labelled with its Infra shape; loadgen reads it.
- **Check**: a declared assertion over the recorded trace, run post-call by the RFC audit engine, keyed by a message anchor.
- **Message anchor**: a project-wide message name (`initialInvite`, `answer`, `bye`, …) a shape publishes and checks bind to.
- **Check set**: a committed reusable JSON bundle of checks keyed by message anchor, referenced by name.

## Loadgen fusion vocabulary

Full definitions: [docs/glossary/loadgen-fusion.md](docs/glossary/loadgen-fusion.md).

- **Shape descriptor**: the one declaration of a Callflow shape in the open `ShapeRegistry`, one id for Test cases and the load fleet.
- **Load body**: a shape's fallible `Send` body the load fleet drives. ADR-0021.
- **Binding pool**: a Test case's wrap-allowed pool of per-call input overlays, so one case dials a subscriber population.
- **Correlation strategy** (`loadgen::Correlation`): how the per-call token crosses the SUT: `header`, `header_templated` or `to_user`.
- **Load profile**: the authored JSON run spec of the load fleet: rate, duration, robustness, overload refusal, shape mix.
- **Accepted ending**: one named way a scenario's call may end and still be a success; nothing else is accepted by default.
- **Failed step**: where a call failed, keyed `<who>[:<kind>#<index>]@<barrier>` (`scenario_harness::actor::FailedAt`).
- **check_fail** (result class): the class a sampled call takes when an attached check fails; a per-sample oracle, not a gate.

## HA tooling vocabulary

Full definitions: [docs/glossary/ha-tooling.md](docs/glossary/ha-tooling.md).

- **Search path**: `tooling/hakit/content/` then the directories of `HAKIT_PATH`; a name defined twice is refused. Avoid: plug-in.
- **Incident**: a named thing that happens to the lab (`inject`, `heal`, optional `probe`); it never states a tolerance.
- **Window** (incident window): the span an event's impact is charged to, from its inject to `settle_s` after its recovery.
- **Impact class**: what went wrong with a call or a run (`setup_failed`, `call_cut`, …), from outcome records and the catalog.
- **Impact budget**: the impact one window allows per class; an unnamed class allows 0, impact outside every window is a finding.
- **Run profile**: one run in `profiles/<name>.yaml`: load streams, incident schedule, window budgets. Avoid: Load profile.
- **Load stream**: one named flow of calls of one shape at one rate from one generator. Avoid: role.
- **Call-outcome record**: the one record a caller-side generator writes per call, what the caller observed (`call-outcome.v1`).
- **Treatment catalog**: the shapes a system under test offers, how to request each and its success endings (`treatment-catalog.v1`).
- **Verdict**: a run's `verdict.json`: per-window impact against budget, unattributed impact, integrity; pass, fail or invalid.

## The HA-blind boundary

Full definitions: [docs/glossary/ha-blind-boundary.md](docs/glossary/ha-blind-boundary.md).

- **Blind crate**: a crate of single-node logic only (`side = "blind"`), its files passing the word net. Avoid: HA-free.
- **Aware crate**: a crate that may know HA (`side = "aware"`): the shell, replication, proxy, runners, harnesses, test crates.
- **Word net**: the words a blind crate never holds (`takeover`, `reclaim`, `tombstone`, `materialise`, …); no allowlist.
- **Store envelope**: what the store keeps beside a call record: worker pair, version vector, state stamp; the core reads none.
- **Off-turn request**: an HTTP request a call sends off its turn and awaits; a node serving the call from its record resends it. ADR-0039.

## Failover sweep vocabulary

Full definitions: [docs/glossary/failover-sweep.md](docs/glossary/failover-sweep.md).

- **Turn point**: one sub-point (`logged`, `applied`, `left`) of one turn at the primary, where a sweep incident arms. Avoid: safe-point.
- **Failover sweep**: every shape cut at every turn point by each sweep incident at each sync level (`just sweep`). Avoid: matrix.
- **Sweep ratchet**: `failover-sweep.ratchet`, the cells that fail today with their owning ADR section; it only shrinks. Avoid: waiver.
- **Clean end**: the system under test ending a call as RFC 3261 allows after an incident: a non-2xx final, a BYE, a NOTIFY. ADR-0055.
- **Window table**: every deviation an incident may cause, one named `W-…` row each with its tier, incidents and witness. ADR-0055.
- **Tier** (Guaranteed / Degraded / Off): how much a cell may deviate, measured from its run. ADR-0055.
- **Declared window**: an RFC-rule window or dialog-verdict waiver of the window table, open from its incident's strike. Avoid: waiver.

## The rig

Full definitions: [docs/glossary/rig.md](docs/glossary/rig.md).

- **Rig** (siprig): the capture replay, the campaign runner, their store and their site, as one tool. ADR-0060. Avoid: testkit, flow viewer, tools.
- **Base**: what this repository holds of the rig. ADR-0060.
- **Pack**: one product's content for the rig, shipped in its image.
- **Test bed**: one workspace of the rig and the lab it drives. It builds nothing.
- **Rig path**: the pack then the workspace, where manifests are found. Not the HA tooling's search path.
- **Scope**: a job, a batch or a cell. Avoid: lane scope, run scope.
- **Job**: one launched replay or campaign. Its scope is the widest.
- **Batch**: the cells a lane runs between a stand and a drain; a `batches` row under its job, or its sweep where the replay is no job.
- **Claim**: what a case expects of one collector. The cell ends without waiting for it.
- **Export**: records a cell's or a batch's own process leaves under a collector's name.
- **Collector**: a plug-in that reads the lab around a scope: records, verifications, attachments, keys. Avoid: gatherer, hook, probe.
- **Judge**: a plug-in that answers for the cells of a scope from their claims and their records.
- **Operation**: a plug-in that changes the lab, with `inject`, `heal` and optionally `probe`. An incident of the HA tooling follows the same rules in another format.
- **Setup**: the operations a named campaign injects after provisioning and heals in reverse when its job ends, however it ends.
- **Job event**: one phase of an operation on a job (`inject`, `injected`, `heal`, `healed`, `recovered`, `failed`, `lab-left-changed`), on the runner's clock and the wall clock; a `job_events` row.
- **Verification**: one named line of a cell, a batch or a job, with a source and an answer. Avoid: check (the RFC audit's), finding.
- **Answer**: `pass`, `fail`, `not checkable` or `pending`.
- **Not requested**: a verification the collection level or the lane did not ask for. It is no line.
- **Final status**: what a cell's verifications come to: `pass`, `fail`, `fail, still verifying`, `ok so far`, `unverified`.
- **Attachment**: a file attached to a job, a batch or a cell, with a kind, listed in its holder's `attachments.jsonl`; an `attachments` row. Avoid: evidence (ADR-0055's).
- **Collection level**: a named set of collector defaults a job is launched with.
- **Required collector**: one no collection level and no lane switches off.
- **Pre-check**: a job- or batch-scope collector `at = "before"`; a `fail` it answers refuses its scope before any call.
- **Record**: one object a collector returned or a cell exported, joined to a cell by a key or assigned to it.
- **Unclaimed record**, **ambiguous record**: one whose key matches no cell, or several.
- **Join**: the base matching a name's records to a scope's cells on one key, in one pass over the records. Avoid: correlation.
- **Key**: a named value a cell carries, that a record is joined on.
- **Cell description**: `cell.v1`, what a collector or a judge reads of each cell.
- **Part**: a named property of a lane beyond its SIP target, a file several lanes may name, with a schema a pack or a workspace declares.
- **Registration seam**: what a product registers into the replay library: its backend double, its simulated system under test, its in-process verifications, its exports.
- **Case source**: a module that yields compiled cells.
- **Reference pack**: the pack this repository ships to exercise every seam. It has no test value.
- **Lab left changed**: the mark of a job where a `close` or a `heal` failed twice.
