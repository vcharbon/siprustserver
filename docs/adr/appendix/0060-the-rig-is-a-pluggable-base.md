# Appendix: The rig is a pluggable base

The decision part: [ADR-0060](../0060-the-rig-is-a-pluggable-base.md). Item numbers are the
ADR's.

## 1. Three levels

| Level | Holds | Builds |
|---|---|---|
| Base | cut engine, replay library and its plain binary, campaign runner, store, site, lane model, lifecycle, minimal case form, image kit, reference pack | this repository's build |
| Pack | comparison and cut rules, backend double, part schemas, collectors, judges, operations, provisioning, case sources, product binaries | one image, from the base image |
| Test bed | lanes, part files, tags, named campaigns, collection levels, its own collectors, judges, operations, part schemas and cases | nothing |

Base names: `rig.toml` (the workspace file), `pack.toml` (a pack's manifest), the `SIPRIG_`
variable prefix, the `siprig` image. A pack names its wrapper command, its image tag, its site
title and its lanes.

## 2. The rig path

```
<pack or workspace>/
  collectors/<name>/collector.toml   + its executables
  judges/<name>/judge.toml           + its executable or module
  operations/<name>/operation.toml   + its executables
  parts/<kind>.schema.json           a JSON Schema
```

An executable runs with its own directory as working directory and these variables:

| Variable | Names |
|---|---|
| `SIPRIG_CONTEXT` | a JSON file it reads (`context.v1`): the scope, its window, the cells (item 6), the lane (its name, clock, SIP target and backend, as the base reads them) and its parts, its parameters |
| `SIPRIG_OUT` | a directory it writes attachment files into |
| `SIPRIG_STATE` | a directory kept from `open` to `close` of one scope, or from `inject` to `heal`; an `open` or `inject` finds it empty and its context fresh |
| `SIPRIG_STEP` | the step it runs as (`open`, `collect`, `close`, `inject`, `heal`, `probe`, `judge`), so one file may serve several |

It answers on stdout, one JSON object per line, or in `lines.jsonl` under `SIPRIG_OUT`; its
stderr goes to the step's log. Exit status 0 means it did its work. Any other status, or the
manifest's timeout, means it did not, and the base quotes the log's last line. `open` and
`inject` may leave a process running; `close` and `heal` end it.

## 3. The order of a job

```
job     pre-checks, provisioning, setup operations injected in order
        open the job collectors
batch     stand what the lane stands, open the batch collectors
cell        the call          -> the call's verifications, its claims pending, its exports
            cell collectors, immediate or delayed         the lane is not held
batch     drain, collect and close the batch collectors, join, judge
          retries: a failed cell that declares them runs again in a batch of its own
job     collect and close the job collectors, join, judge
        no verification is pending
        report, then heal the setup operations in reverse order
```

- A pre-check is a job-scope or batch-scope collector with `at = "before"` and no `open`. A
  `fail` it answers refuses the job or the batch before any call, with its message on the job
  (an event `refused`). A `not checkable` refuses nothing and stays the scope's line, except a
  `required` pre-check's: that one, its failed or timed-out `collect` included, refuses as a
  `fail` does. A batch's pre-checks run before the lane stands anything.
- A `required` pre-check, of either scope, that a real-clock lane of the job names no part of
  refuses the job before any pre-check runs, as a missing `required` claim does (item 4).
- Provisioning is one phase the pack fills (`pack.toml`, `[provision]`), once per job.
- A retry is decided when the batch's verifications have answered. Job-scope verifications
  apply to the attempt kept for each cell and start no retry.
- On a simulated lane a batch is the cells the lane's driver runs in one pass. Nothing is stood,
  and batch collectors still bracket it.
- A batch does not stand its system under test down before the delayed collectors of its cells
  have answered or reached their deadline.
- A capture replay's run rows (one per capture and lane) are not a scope. Its job is.

## 4. Claims and exports

A claim is what a case expects of one collector, opaque to the base:

```json
"postconditions": { "claims": { "call-records": { "count": 1 } } }
```

A case source may add claims beside the document. The interpreter settles a call on its flow
and on the system reporting no active call. It never waits for a claim. The claim becomes a
pending verification of the cell, named after the collector, and the cell description carries
it (item 6).

An export is a file `exports/<collector>.jsonl` in the cell's directory, or in the batch's for
what the batch process knows, written through the registration seam (item 13) when the cell or
the batch ends. Its lines are record lines (item 5). A simulated system under test exports the
records it would have published. A backend double exports what it alone saw of a call.

| Case | What answers the claim |
|---|---|
| a collector of that name is wired on the lane | its judge, over its records and the exports |
| no collector, exports exist, a judge of that name is on the path | that judge, over the exports |
| neither, and the collector is `required` | the batch is refused before any call |
| neither, not `required` | the verification is *not requested* and the job says why |

## 5. The collector manifest

```toml
schema   = "collector.v1"
name     = "call-records"
scope    = "batch"             # job | batch | cell
at       = "after"             # before | after
needs    = ["records-store"]   # lane parts
default  = "always"            # always | on-failure | on-demand
required = false               # true: no level and no lane switches it off
after    = ["sut-log"]         # same scope: collectors whose keys and records it reads
answers  = ["call-records"]    # the verifications it, or its judge, answers
judge    = "call-records"      # optional
locks    = ["records-chain"]   # box-wide, held from open to close
delay_s  = 10                  # cell scope: wall clock, after the cell ended
holds_cell_locks = true        # cell scope: the cell's own locks are kept until it answered

[params]                       # a lane file may override each
poll_s = 1

[join]
key = "call_id"                # omitted: the collector assigns records to cells itself

[timeouts]
open_s = 30
collect_s = 300
close_s = 30

[run]
open    = "./open"             # optional
collect = "./collect"
close   = "./close"            # optional
```

Rules:

- `open` runs whenever the collector is wired, whatever its default.
- `on-failure` is read when `collect` would run: the collector runs if a cell of its scope has
  a failed verification by then, and receives only those cells. A source that keeps no history
  is collected `always` and cut per cell.
- An `on-demand` collector runs when the site or the CLI asks for it, on the cells named.
- A failed or timed-out `open` or `collect` answers `not checkable` for every verification the
  manifest names, or for one named after the collector where it names none, on every cell of
  the scope and on the scope, quoting what failed. So does a line of `collect` that is none of
  the table below, names a cell outside the scope or a verification the manifest does not name,
  or attaches no file under `SIPRIG_OUT`.
- `close` runs whatever ended the scope, after a failed or interrupted `open` too. A `close`
  that fails is run once more. A second failure marks the job *lab left changed*.
- `locks` are taken per scope: the union of its collectors' locks, once, before its first
  `open`, given back after its last `close`, so collectors of one scope naming one lock share
  it. Where they are refused, each collector naming one runs none of its steps and answers
  `not checkable`. A lock named by collectors of two scopes is refused at load: the outer
  scope's would hold it while the inner one's waits.
- A collector or judge named like a source the base (`interpreter`, `rfc audit`, `driver`, `rig`) answers lines
  as is refused at load. A collector's line merged into a landed cell replaces only a `pending`
  line of its name and source, never an answered one.
- Timeouts default to 30 s for `open` and `close`, 300 s for `collect`.
- `after` orders collectors of one scope. A wider scope always runs after a narrower one ended
  and reads its keys and records without declaring it. An `after` naming a collector that is
  not wired on the lane orders nothing. Collectors free to run go by name. A cycle of `after`
  among one scope's collectors is refused at load.
- A job-scope collector that needs a part runs once per distinct part file among the job's
  lanes, over the cells of the lanes naming it.
- A delayed collector has a deadline of `delay_s` plus `collect_s`.
- No cell-scope collector runs on a simulated lane (item 12).

The lines of `collect`:

| Line | Meaning |
|---|---|
| `{"record": {…}, "key": "<value>"}` | a record the base joins by the manifest's key |
| `{"record": {…}, "cell": "<id>"}` | a record the collector assigned itself |
| `{"verification": "<name>", "cell": "<id>", "answer": "pass\|fail\|not-checkable", "message": "…", "detail": {…}}` | an answer; without `cell` it is the scope's own |
| `{"key": "<name>", "cell": "<id>", "values": ["…"]}` | a key added to a cell |
| `{"attachment": {"kind": "log", "title": "…", "file": "<path under SIPRIG_OUT>"}, "cell": "<id>"}` | an attachment; without `cell` it is the scope's own; `name` defaults to the instance's (the collector's name, `-<lane>` added per part set) |

The base copies an attached file under its holder's `collected/<instance>/` and lists it in the
holder's `attachments.jsonl` (§9). The TypeScript helper `defineCollector`
(`@sip/rig/define-collector`) hides the files and the lines: one module serves every step,
`SIPRIG_STEP` choosing. A collector in another language needs no library.

## 6. The join, the cell description and the judge

The base indexes the scope's cells by their values of the manifest's key once, then reads the
records of a name in one pass, looking each up. The records of a name are those its instances
returned and those the scope's exports hold. They are joined once the name's last instance
collected, so a later collector reads them.

- A record matching one cell is joined to it: a line of `joined/<name>.jsonl` in the cell's
  directory, the file `records` names. A record a collector assigned names its cell by id.
- A record matching no cell is *unclaimed*. A record matching several is *ambiguous*. Both are
  joined to no cell and kept on the scope: the attachments `<name>-unclaimed` and
  `<name>-ambiguous` (kind `log`, `joined/<name>.unclaimed.jsonl` and `.ambiguous.jsonl` in its
  directory, each line as its writer stated it, an ambiguous one with the `cells` it matched).
- An export names a cell by its directory's name: the process that writes it does not know the
  id its scope reads the cell by (a retry's, a repeat's). The base reads the name as the scope's
  cell whose directory bears it, in a cell's own export that cell alone.
- The keys exports and collectors add are kept in the cell's `keys.json`. A wider scope
  describes the cell anew off its directory and reads them, and its joined records, there; a
  `keys.json` or `joined/` there that cannot be read leaves the cell undescribed, as does a
  `pivot.json` whose postconditions (the cell's stated claims) cannot be read.
- A cell with no record reaches the judge with an empty list.
- A verification nobody answered for a cell, the cell's own lines counted, is `not checkable`
  where it is promised: those a manifest's `answers` names, a collector's own name where it
  names a judge and no `answers`, a judge of exports alone its name, on a cell the base could
  not describe too. A collector naming neither may answer its own name and promises none.
- An export line that is none, a key naming no one cell, a line other than a record or a key, an
  export file or directory that cannot be read: each is skipped and said, the rest read.
- Where the scope's input was not handed whole (an export skipped in part, a cell the base could
  not describe, keys or records the base could not write), every `pass` its collectors and
  judges answered is `not checkable` on the cell it concerns (every cell where it is the
  scope's) and on the scope; a `fail` or other answer is kept, a `not checkable` beside it. The
  base answers its own line `input` (source `rig`) `not checkable` on each of them, whether or
  not an entry answered there: the cell never passes, whatever a wider scope answers of it.
- A collect or a judge's run that dies in the base's own reading answers `not checkable` on
  every cell and the scope, saying it died; the scope's other answers stand.
- Exports judged with no collector of their name (item 4) are a batch's. They join on the
  `[join] key` of the path's collector of that name, wired or not, else on `call_id`.

Every collector and judge reads the cells of its scope in `cell.v1`:

| Field | Content |
|---|---|
| `id`, `case`, `sub_case`, `lane`, `batch`, `attempt` | identity |
| `clock` | `real` or `simulated` |
| `keys` | name to list of values. Always: `call_id` (every Call-ID the test side sent or received), `caller`, `callee`, `request_uri_user` (the users `sip-message` reads off each leg's first INVITE, stated in the recording line's `identities`). Plus the keys the case declares, and those the registration seam and earlier collectors added |
| `legs` | per leg: role, Call-ID, From, To, request-URI, and its start, answer and end instants |
| `timeline` | start and end of the cell, each on the lane's clock and on the wall clock |
| `claims` | per collector name, what the case expects |
| `records` | per collector name, the file of the records joined to the cell so far |
| `verifications` | the lines answered so far: name, source, answer, message, detail |
| `dir` | the cell's directory: its bundle, its exports, its attachments |
| `tags`, `needs` | as the case states them |

```toml
schema = "judge.v1"
name   = "call-records"
module = "./judge.ts"          # or: run = "./judge"
[timeouts]
judge_s = 120
```

A judge is called once per scope with every cell, the unclaimed and the ambiguous records: its
`context.v1` states `judging` (the name, the two files). A module's default export is called
with `{context, out}` and returns its lines as objects (an iterable, a Promise or an Effect of
one); an executable runs with `SIPRIG_STEP` = `judge` and answers on stdout and in
`SIPRIG_OUT/lines.jsonl`. It answers `verification` and `attachment` lines, for the
verifications its collector names (its own name with no collector). A judge that fails (a
module that throws, rejects, dies or returns no iterable included), times out or answers
another line answers `not checkable` on every cell and the scope. Only an executable judge's
timeout is enforced whatever it does: a module judge is trusted code in the base's process
(item 18), timed out only while it awaits, and one that blocks synchronously blocks the base.

## 7. The operation manifest

```toml
schema = "operation.v1"
name   = "restart-worker"
needs  = ["orchestrator"]
locks  = []
[params]
grace_s = 5
[timeouts]                     # defaults: 60, 120, 60, 1
inject_s = 60
heal_s = 120
probe_s = 10                   # the whole probe, its "not yet" answers included
probe_interval_s = 1
[run]
inject = "./inject"
heal   = "./heal"
probe  = "./probe"             # optional
```

- `heal` always runs and is idempotent: after a failed or interrupted `inject` too. `probe`
  proves recovery after `heal`: exit 0 is recovered, 75 is not yet (called again every
  `probe_interval_s` within `probe_s`), anything else is failed. A step concludes only from a
  positive answer. These are the rules of ADR-0049 §2; the file format and the variables are
  not its.
- Each executable runs in a process group of its own. A timeout kills the group; a signal to
  the runner reaches only the runner, which then heals.
- A named campaign's `setup` list injects after provisioning and heals in reverse at the end of
  the job, whatever happened, an interrupted job and a SIGINT, SIGTERM or SIGHUP to the runner
  included; the runner's output to a terminal or pipe that is gone never stops a heal.
  A setup name is an operation of the path. One that needs parts runs once per distinct set of
  those part files among the job's lanes, with the first lane naming the set as its context.
  One that needs no part runs once. A name the path lacks, or parts no lane names, refuse the
  job before it opens.
- A case step injects at a named point of its call and heals when the cell ends. The case
  carries a tag that runs alone.
- A lane file's `[operations]` table binds the name a case asks for to an operation of the path.
  A lane where the name resolves to nothing, or that lacks a part the operation needs, cannot
  run the case.
- A heal attempt is `heal` and its `probe`. A failed attempt is run once more. A second failure
  marks the job *lab left changed*, which closes it failed however it ended. One operation's
  failed heal never stops the heals after it.
- Each phase is an event of the job with its instant on both clocks (the store's
  `job_events`): `inject`, `injected`, `heal`, `healed`, `recovered`, `failed` and
  `lab-left-changed`, the job's mark.
- A cell whose case asks for an operation its lane binds none of is `not runnable` there.
- A case's own heal time (the wait a lane holds after a disruptive case) is another thing and
  keeps its name.

## 8. Verifications and the final status

A line: `name`, `source` (the interpreter, the RFC audit, the backend double, the in-process
seam, a collector or a judge), `answer`, `message`, and an optional `detail` the site renders
when present: `step`, `expected`, `seen`, `side`. Two marks: `informative` (it never decides the
cell) and `waived` (a failure the lane file lists as a known bug). An expected failure a case
declares is a `pass` of the line that expected it.

A cell that ran:

| Its lines, informative and waived ones aside | Final status | JUnit |
|---|---|---|
| all `pass` | `pass` | passed |
| one `fail`, none pending | `fail` | failure, typed by `side` |
| one `fail`, some pending | `fail, still verifying` | (job still open) |
| no `fail`, some pending | `ok so far` | (job still open) |
| no `fail`, none pending, one `not checkable` | `unverified` | error |

A cell that did not run keeps the outcome it has today, with its reason: `excluded` (a tag),
`not runnable` (a need or an operation the lane lacks), `not ported`, each a JUnit skip, and
`error` (the cell could not be driven), a JUnit error. A verification the job's collection
level or the lane did not ask for is *not requested*: no line, and no effect on the status.

## 9. Attachment kinds

| Kind | File | Rendering |
|---|---|---|
| `log` | text or JSON lines | a filterable log |
| `records` | JSON lines of flat objects | one table per record |
| `diff` | expected and actual, per field | a two-column delta |
| `table` | rows with a header | a table |
| `json` | one document | a tree |
| `file` | anything | a download |
| `link` | a URL | a link |

The site compares the `records` attachments of one name between two cells with the `diff`
component, record by position. A `diff` row may carry a `class`, the writer's word for how the
difference reads; the rows of the classes its `quiet` names are listed on demand. A job's text
export includes its `log` attachments.

A writer lists what it attaches in its holder's directory, one JSON line each in
`attachments.jsonl` (`@sip/contracts` `Attachment`): `name`, `kind`, `title`, `file`, `source`.
The last line naming an attachment states it. The store holds one row per name per holder.

## 10. Wiring

```toml
# rig.toml
[collection]
default = "standard"
[collection.quick]
never = ["capture"]
[collection.paranoid]
always = ["capture", "sut-log"]

# a lane file
[collectors]
exclude = ["capture"]
[collectors.call-records]
delay_s = 40
poll_s = 2
```

A level sets a collector to `always`, `on-failure`, `on-demand` or `never`. A level or a lane
that names a `required` collector, or a name the path does not define, refuses the job before
it opens.

## 11. The lane file

```toml
schema = "lane.v1"
clock  = "real"                # real | simulated
order  = 10
[sip]                          # a real lane only
lifecycle = "standing"         # managed | standing
ingress = "192.0.2.10:5060"
endpoint_host = "192.0.2.1"
exclusive = false
up = "…"                       # how a standing system is brought up, said when its ingress is silent
[backend]                      # a real lane only
mode = "served"                # served | proxied | none
listen = "192.0.2.1:8080"      # stated unless mode = "none"
holder = "…"                   # what may hold listen, named when it is taken
[backend.http_budget]
new_call_ms = 2000
follow_up_ms = 2000
transit_ms = 1
[pacing]                       # what a replay takes where nobody states it
concurrency = 100
batch_size = 4000
starts_per_second = 10
[pacing.ceilings]              # the most the lane takes
cells = 600
starts_per_second = 50
[pacing.campaign]
concurrency = 50
starts_per_second = 10
timing_tolerance_ms = 20
[timers]                       # the system's own timers a case may name, in ms
keepalive_ms = 30000
known_bugs = ["…"]             # failures this lane waives
[parts]
records-store = "lab-a"        # targets/records-store/lab-a.toml
[tags]
exclude = ["disruptive"]
[collectors]
[operations]
restart-worker = "restart-worker-compose"
```

A part file `targets/<kind>/<name>.toml` is validated against `parts/<kind>.schema.json` of the
path, a JSON Schema whose root is an object, written in the Draft 2020-12 subset Effect's importer
translates: a keyword outside it refuses the schema, naming its JSON Pointer. A refusal of a part
file names the file and the dotted field. Several lanes may name one file. The schema's root keyword `x-siprig` states what the base reads of the kind:

```json
"x-siprig": {
  "shared": true,
  "capabilities": { "records-readable": { "reader": true }, "records-writable": { "mode": ["append", "reset"] } }
}
```

- `shared`: the lanes of one job name one file of the kind, or none on every lane.
- `capabilities`: what a part of the kind gives the lane, each when the fields it lists hold:
  `true` for present, a list for one of those values. It names no capability the base derives.

A lane's capabilities derive from its clock and backend mode (`scripted-routing` where the rig
serves the backend or the lane is simulated, `routing-visible` where it also relays it,
`real-clock` or `paused-clock`), from its parts, and `operation:<name>` for each name its
`[operations]` binds. A binding to an operation the path does not define, or whose `needs` the
lane's parts lack, refuses the lane.

## 13. The registration seam

| A product registers | Called |
|---|---|
| its backend double | per batch, to serve or relay the system's decision requests |
| its simulated system under test | per cell on a simulated lane |
| in-process verifications | at a cell's end and at a batch's drain; each answers lines |
| exports and keys | at a cell's end and at a batch's drain |

How a managed lane stands its system under test stays a module of the pack's deployment.

## 14. The minimal case form

```ts
defineBasicCase({
  title: "alice calls bob",
  shape: "basic_call",          // or "invite_reject"
  caller: "sip:alice@atlanta.example.com",
  callee: "sip:bob@biloxi.example.com",
  final: [{ field: "status", eq: 200 }],
  claims: { "call-records": { count: 1 } }
})
```

`@sip/basic-case` compiles it to one cell: a pivot v3 document between a caller
leg `A` and a callee leg `B`, its id the title's slug, replayable on every lane
(`lanes: {"*": "ok"}`), its CDR expectation left to the claims
(`postconditions.cdr.absent: "claims"`). `basic_call` rings, answers, ACKs and
ends with a BYE from the party `params.ends` names (the caller by default);
`invite_reject` answers the INVITE with `params.status` (486 by default), a
status RFC 3261 lets stand without a header beside it and a proxy relays as
sent (never 503, relayed as 500, §16.7). The `final` checks land
on the caller's final. A key the form does not name, an address that is no
`sip:`/`sips:` URI naming a user, a title naming no cell, or a final status
the shape does not end with is refused when the case is defined.

## 16. The pack manifest

```toml
schema  = "pack.v1"
name    = "reference"
title   = "siprig reference"
wrapper = "rigref"
component = "rigref"           # the product's component name; absent, the pack's name
[image]
packages = []                  # OS packages the pack's image adds
binaries = "./bin"             # product binaries, the replay one included when it registers
[provision]
run = ""                       # empty: no provisioning
[cases]
sources = ["./cases.ts"]       # modules that yield compiled cells
[modules]
deployment = "./deployment.ts" # the pipeline and corpus seams the pack fills
lanes = "./lanes"              # default lane and part files
[decides]
extraction = ["./anonymiser"]  # the pack's paths whose content decides an extraction
cut = ["./rules"]              # and a cut: a change under one re-runs the step
```

Every path the manifest names exists in the pack's tree when the rig path loads it.

A case source is a module whose default export is an array of compiled cells
(`@sip/rig` `CompiledCell`: id, case, sub-case, title, the pivot v3 document,
its resources by path, its claims). `CaseSource.load` refuses, naming the
module, one that does not load, an export that is no such array, a cell that
does not decode, a body naming a resource the cell does not carry, and two
cells of one id. A module, not a glob: a pack keeps its own case form and
compiler behind the module, and the runner reads compiled cells alone.

## 17. The reference pack

| Sample | Seam it exercises |
|---|---|
| `sut-log`, a batch collector cutting the B2BUA's log per cell | bracket, `log` attachment |
| `capture`, a job collector: `open` starts a packet capture, `collect` cuts it per cell | state directory, `file` attachment, `on-failure` |
| `call-records`, a batch collector reading the B2BUA's record queue, with its judge | claim, join, judge, `records` and `diff` attachments, a lock |
| `calls-ended`, a delayed cell collector reading the worker's active-call gauge | delay, the lane not held |
| `restart-worker`, an operation on the reference lane | operation, `setup`, a case step |
| `alice-calls-bob` and `bob-is-busy` | the minimal case form and both shapes |
| a simulated lane exporting its records | export, the plain replay binary |
| one capture of a forked call with reliable provisionals, recorded through the B2BUA runner, the forking made by the callee side | ingest with an identity anonymiser, cut, replay |

Names, numbers and addresses are those the RFCs reserve for examples.
