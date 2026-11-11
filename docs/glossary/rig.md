# The rig

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "The rig".

The language of siprig, the pluggable base of the capture replay, the campaign runner, their
store and their site ([ADR-0060](../../docs/adr/0060-the-rig-is-a-pluggable-base.md), contracts
in its [appendix](../../docs/adr/appendix/0060-the-rig-is-a-pluggable-base.md)): the three
levels, the three scopes, the plug-ins found on the rig path, and how a cell is decided.

## Levels

**Rig** (siprig):
The capture replay, the campaign runner, their store and their site, as one tool. A product
extends it with a pack, a lab with a workspace, through one contract.
_Avoid_: "testkit" (the `*-testkit` crates are test doubles of one layer), "flow viewer",
"tools".

**Base**:
What this repository holds of the rig: the cut engine, the replay library and its plain binary,
the campaign runner, the store, the site, the lane model, the lifecycle, the minimal case form,
the image kit and the reference pack.

**Pack**:
One product's content for the rig, shipped in its image built from the base image: its
comparison and cut rules, its backend double, its part schemas, collectors, judges and
operations, its provisioning, its case sources and its binaries. `pack.toml` is its manifest.

**Test bed**:
One workspace of the rig and the lab it drives, owned by whoever runs the lab. It builds
nothing. What a test bed can define (lanes, part files, tags, named campaigns, collection levels,
its own plug-ins and cases), a pack can define the same way. `rig.toml` is its file.

**Reference pack**:
The pack this repository ships to exercise every seam: one sample per plug-in kind, a call from
alice to bob, one capture recorded here. It has no provisioning and no test value.

**Rig path**:
The pack, then the workspace, where manifests are found. Each entry holds `collectors/`,
`judges/`, `operations/` and `parts/`. A name defined twice is refused, naming both files.
_Avoid_: the HA tooling's **search path**, which is that runner's.

## Scopes

**Scope**:
A job, a batch or a cell. A collector, a judge and a verification each belong to one.
_Avoid_: "lane scope" (a job-scope collector runs once per part instance of the job's lanes),
"run scope".

**Job**:
One launched replay or campaign, the widest scope. A replay and a campaign go through one
lifecycle. The run rows of a capture replay are not a scope; its job is.

**Batch**:
The cells a lane runs between a stand and a drain. On a simulated lane it is the cells the
lane's driver runs in one pass: nothing is stood, and batch collectors still bracket it.

## Plug-ins

**Collector**:
A plug-in that reads the lab around a scope: `collect`, and optionally `open` before the scope
and `close` after it, sharing one state directory. `close` always runs, is idempotent and
restores what the collector changed. It may return records, answer verifications, attach files,
add keys to a cell, hold named locks and run `after` other collectors. A cell-scope collector
may state a delay and then runs that long after the cell ended, the lane not held.
_Avoid_: "gatherer", "hook", "probe" (an operation's recovery proof).

**Required collector**:
A collector no collection level and no lane switches off. A case claiming on a lane where it
cannot run refuses the batch before any call.

**Collection level**:
A named set of collector defaults (`always`, `on-failure`, `on-demand`, `never`) the workspace
file defines and a job is launched with.

**Judge**:
A plug-in that answers for the cells of a scope from their claims and their records: called once
per scope with every cell and the unclaimed and ambiguous records, it answers verification and
attachment lines. A TypeScript module or an executable.

**Operation**:
A plug-in that changes the lab: `inject`, `heal` and optionally `probe`, under the rules of the
HA tooling's incidents (ADR-0049 §2) in its own format. A named campaign lists operations as its
`setup`, a case names one as a step of its call, a lane file binds the name a case asks for.
_Avoid_: a case's own heal time (the wait a lane holds after a disruptive case).

**Part**:
A named property of a lane beyond its SIP target, the clock, the backend mode, the pacing and the
tags: a file `targets/<kind>/<name>.toml` several lanes may name, validated against a schema a
pack or a workspace declares. A lane's capabilities derive from its parts.

**Registration seam**:
What a product registers into the replay library, in process: its backend double, its simulated
system under test, its in-process verifications, its exports and keys. The product builds its own
binary; the base ships one with nothing registered.

**Case source**:
A module that yields compiled cells. The base ships the minimal case form and two shapes,
`basic_call` and `invite_reject`; a product's case vocabulary is its pack's.

## Deciding a cell

**Claim**:
What a case expects of one collector, opaque to the base. The interpreter never waits for it: the
cell ends when its call ends, the claim a pending verification named after the collector.

**Export**:
Records a cell's or a batch's own process leaves under a collector's name, as
`exports/<collector>.jsonl`, through the registration seam. A simulated system under test exports
the records it would have published.

**Record**:
One object a collector returned or a cell exported, joined to a cell by equality on the one key
its manifest names, or assigned to a cell by its collector.

**Unclaimed record**, **ambiguous record**:
A record whose key matches no cell, or several. Both are kept as an attachment of the scope and
handed to the judge, joined to no cell.

**Key**:
A named value a cell carries that a record is joined on. Every cell has `call_id` (every Call-ID
the test side sent or received), `caller`, `callee` and `request_uri_user`, plus the keys its
case declares and those the registration seam and earlier collectors added.

**Cell description**:
`cell.v1`, what a collector or a judge reads of each cell of its scope: identity, clock, keys,
legs, timeline, claims, records, verifications so far, directory, tags and needs.

**Verification**:
One named line of a cell, a batch or a job, with a source (the interpreter, the RFC audit, the
backend double, the in-process seam, a collector or a judge), an answer, a message and an optional
detail. It may be marked `informative` (it never decides the cell) or `waived` (a known bug the
lane file lists).
_Avoid_: **Check** (a declared assertion of the RFC audit), "finding".

**Answer**:
`pass`, `fail`, `not checkable` or `pending`. A collector or a judge that fails or times out
answers `not checkable`; a verification its manifest names and nobody answered is `not
checkable`.
_Avoid_: "missing": whoever looked states `fail` or `not checkable`.

**Not requested**:
A verification the job's collection level or the lane did not ask for. It is no line and does not
affect the final status; the job says why.

**Final status**:
What the verifications of a cell that ran come to, informative and waived ones aside: `pass`
(all pass), `fail` (one fails, none pending), `fail, still verifying`, `ok so far` (none fails,
some pending) or `unverified` (none fails or is pending, one is not checkable).

**Attachment**:
A file a collector or a judge attached to a job, a batch or a cell, with a kind: `log`,
`records`, `diff`, `table`, `json`, `file` or `link`. The site renders each kind with one
component; a pack ships no frontend code.
_Avoid_: **Evidence** (ADR-0055's message proving a call's step).

**Lab left changed**:
The mark of a job where a `close` or a `heal` failed twice.
