# `ts/` — the TypeScript side of the replay toolchain

A pnpm workspace. Eighteen packages, four of them one layer each of the same stack:

| package | name | owns |
| --- | --- | --- |
| `contracts/` | `@sip/contracts` | Effect Schema mirrors of the Rust-owned wire formats, the canonical formatter, pure readings of those formats (call key, step order, a cell's main error), and the report formats the tooling publishes (`sip.compare/1`, `sip.run-report/1`, their JSON Schemas under `contracts/schemas/`) |
| `toolchain/` | `@sip/toolchain` | typed adapters over the Rust CLIs (`sipflow`, `pivot-schema`, `replay`) and the process primitives above them (a command into a file or a log, a detached command and its liveness, `git`, a node child's heap), where each Rust binary is (a prebuilt directory a variable names, else the checkout's release build) and whether an optional host tool is on `PATH` — the ONLY package that adapts a Rust CLI |
| `pipeline/` | `@sip/pipeline` | flows ingest, the correlation rule engine, the cut, and case assembly |
| `driver/` | `@sip/driver` | one cell of a campaign, either shape, into its caller's run dir; the `campaign.json` line it answers is the caller's to fold |
| `store/` | `@sip/store` | the run store: the postgres record of cuts, runs and what each call said, the readers a queue and a comparison use, and the run selectors |
| `viewer-core/` | `@sip/viewer-core` | the callflow viewer's core: pivot types generated from the Rust schema, the pure ladder layouts, their SVG components, the run-bundle and capture-flows readers, and the text export |
| `viewer-app/` | `@sip/viewer-app` | the viewer's Vite + React SPA over a deployment's server: cases, runs, the triage queue and study, and the compare and run report pages over the report formats |
| `corpus/` | `@sip/corpus` | the orchestration of a capture corpus over a deployment's seams (`deployment/`): extraction, cut and replay on a lane, jobs and requests, and the compare and run reports a command prints and a viewer serves |
| `cli/` | `@sip/cli` | the command lines over `@sip/corpus`: flag sets and command builders taking a deployment's facts |
| `rig/` | `@sip/rig` | the rig's plug-in contract (ADR-0060): the pack, collector, judge and operation manifests, the rig path that finds them, the collection levels and lane wiring checked before a job opens, and the `cell.v1` description built from a cell's bundle |
| `basic-case/` | `@sip/basic-case` | the minimal case form: two parties as plain SIP addresses, the basic call and the rejected INVITE on a pivot step builder, compiled to the cells a case source yields |
| `case-modules/` | `@sip/case-modules` | a workspace's TypeScript case files as modules of a tooling's SDK: imported at their on-disk version with the SDK's specifier resolved to the tooling's own entry, typechecked against the SDK's declaration text alone, and that text generated from the tooling's sources — on the TypeScript compiler and node's module hooks, no other package of this workspace |
| `workspace/` | `@sip/workspace` | one directory holding a tooling's roots, its store and its one configuration file: its layout and the environment it states, the campaigns and tags it defines, the site's public URL, and the workspace as a run's provenance |
| `lab-host/` | `@sip/lab-host` | the host a lab runs on: its user session and the boxes its children run in, its binaries built under one box-wide lock, its roots and env files, its inbox, its run bundles, the checks a lane owes before it is driven, and the batch and SUT processes a hosted lane stands |
| `campaign-runner/` | `@sip/campaign-runner` | a campaign run over a pack's cases: its cells compiled and replayed on each lane under their locks and the lane's pacing, retried, judged into verification lines, recorded in the store's campaign record, and reported (JUnit included) |
| `site/` | `@sip/site` | the viewer's server: an Effect v4 HttpApi over case directories, run bundles, the store, campaign runs and the job console, serving the app's build and the documents it draws, over what a deployment's pack states of itself (`site-pack.ts`) |
| `siprig/` | `@sip/siprig` | the base deployment the image kit ships (`tooling/siprig/`): its reading of a pack manifest before a build, and the workspace, store, site and campaign commands the base image runs for a pack that ships none of its own |
| `reference-pack/` | `@sip/reference-pack` | the reference pack (ADR-0060 item 17): one sample per seam of the rig on the upstream B2BUA, its campaign run by `siprig`'s in this workspace's tests |

Dependencies point one way only. `contracts` depends on nothing, and every other
package but the leaves `viewer-core` and `case-modules`, the deployment `siprig` and the pack `reference-pack` depends on it; beyond it, each
package's own edges are: `pipeline`, `toolchain`, `store` and `rig` none,
`driver → {pipeline, toolchain}`, `basic-case → {driver, rig}` (its tests also
`toolchain`), `corpus → {driver, pipeline, rig, store, toolchain, viewer-core}`,
`cli → {corpus, store}`, `workspace → {cli, rig, store, toolchain}`,
`lab-host → {cli, corpus, rig, store, toolchain, workspace}`,
`campaign-runner → {corpus, rig, store, toolchain, workspace}`,
`site → {campaign-runner, corpus, lab-host, pipeline, rig, store, viewer-core,
workspace}`, `siprig → {campaign-runner, cli, corpus, lab-host, rig, site, store,
workspace}`, `reference-pack → {basic-case, rig}` (its tests also `siprig`, `store`) and
`viewer-app → viewer-core` (the app reads the report formats' types
from `contracts`, nothing else).
The viewer's server is `@sip/site`, a package of its own beside the app: the app
never imports it and meets it over HTTP only, so the SPA's build carries no node
side. A deployment runs the site under its own entry point, its pack provided
once (`site-pack.ts`); `@sip/siprig` `site-main.ts` is the base's. A policy package never imports the
store: what it reads of a run reaches it as contract vocabulary. Only
`@sip/toolchain` runs a Rust CLI; `@sip/corpus` runs every child through it, and
`@sip/lab-host` stands the host's own processes (a build, a lane's batch and SUT).

The store's tests build and drop their own databases on a postgres server:
`STORE_TEST_URL` names one (any database of it, `postgresql:///postgres?host=…`),
exported or stated in the workspace's untracked `ts/.env.local` (an exported value
wins), else the client's default socket. A suite whose server does not answer fails,
naming the url; `STORE_TEST_SKIP=1` skips them instead. A
deployment states its own configuration — database url, current population,
selector spelling, capture names, document features — and nothing of it lives
in the store.

The toolchain's suites held to the real `pivot-schema` run `PIVOT_SCHEMA_BIN`,
else the release build (`cargo build --release -p pivot-schema`). Where neither
is on disk they fail, naming the path and the build; `TOOLCHAIN_TEST_SKIP=1`
skips them instead.

## Nothing generic knows a deployment

`@sip/pipeline` decides what CROSSED THE WIRE and never what one platform does.
Every reading that would need to know — a derived Call-ID, a handover cause, a
family label, a lane verdict, a declared failure, a refusal — arrives as a
`CasePolicy`, and the default (`neutralPolicy`) STATES NOTHING rather than
guessing. Extension is Layer substitution and nothing else:

```ts
import { CaseAssembler, LogicExtractor, Reclassifier } from "@sip/pipeline"
import { LanePresets, RoutingCompiler } from "@sip/driver"
import { Layer } from "effect"

const composed = Layer.mergeAll(
  CaseAssembler.layer.pipe(Layer.provide(LogicExtractor.layerWith(myPolicy))),
  Reclassifier.layerWith(myReading),      // the post-run escape hatch
  LanePresets.layerWith(myLanes),         // what a lane NAME binds or spawns
  RoutingCompiler.layerWith(myCompiler)   // routing intent → the §4.3 overlay
)
```

`LanePresets` has no usable default on purpose: a guessed lane block would run a
case against whatever process happened to be listening. `RoutingCompiler`'s
default is the cell's own `overlay`, which is the escape hatch a pilot needs and
is visible in the cell rather than buried in a layer.

The orchestration above the driver reads the same way. What only a deployment
can state is a `@sip/corpus` seam, substituted with `layerWith`:

| seam | what the deployment states |
| --- | --- |
| `Roots` | its artifact store, session tree, census runs, verdict registry, pcap roots, lock root |
| `Tools` | how its extractor, case generator, interpreter and managed SUT are built, the wrapper a measured phase runs under, and which paths decide a cut |
| `Boxing` | how a child is capped in memory and CPU, why the host cannot box one now, the executable a cell boxes its interpreter through, and the variables that tell the interpreter its box |
| `Extraction` | its anonymiser: how a raw flows document becomes the one the store keeps, the seed it keeps per capture name, and how a failed extraction is recorded |
| `LaneHosting` | what a lane name means to the orchestrator — clock, exclusivity, ceilings, batch process, SUT, preflight — and the one place a lane's clock is stated; the default refuses every lane by name |
| `CaseLayout` | the numbered side rows a case carries for the interpreter's peers |
| `CellHost` | what a cell runs under, and how a landed cell's charges and rule hits are read |
| `Identity` | the environment, branch and repositories a run is recorded under |
| `Launcher` | the entry point each job kind runs, the hooks around every job, and the command that freezes a capture's case ids |
| `Tracker` | its ticket tracker: the state of a registry record's ticket, and every ticket's texts |

Besides them, `@sip/store` `StoreBinding` states its selector spelling,
capture grammar and migration, and `@sip/cli`'s `CliFacts` what the parser
needs before any service exists (the default lane, the host env files, the
help every entry point ends with) and the words its lines are printed in
(how a command is invoked, the viewer's address, the selector spelling, why
a refusal token was minted). `@sip/cli` holds the commands themselves —
campaign, source, cut, replay, evidence, request, job, compare, case,
retire, delete, cleanup and the store verbs — each a builder over the facts;
a deployment's entry point runs one under its own layer, beneath the store
the line's roots name (`Store.layer`), and adds verbs of its own to the
store command where it has any. Every default layer builds; roots, tools,
the anonymiser, a lane, the cell host, a job's entry point and the tracker's
texts refuse by name when a command reaches for them, and the rest state nothing.
The case-id grammar has one source, the binding: the resolver
(`StoreBinding.resolver`), the id forms and the citation scan all read it.

## What a campaign leaves on disk

```text
<run-dir>/
  campaign.json                 the aggregate index (`E2e.CampaignIndex`)
  specs/<cell>.run-spec.json    what the driver asked the interpreter for
  <cell>/                       IS the interpreter out_dir — it WIPES this
  <cell>/error.txt              a cell that never produced a result
```

A cell is `{case, shape, infra}` where `shape` is `pivot-replay` or `rust-test`
and `infra` is the lane or the crate. `@sip/driver` runs one cell
(`runCell`); the replay of `@sip/corpus` (`replay/replay.ts`) runs a cut's
cells under its ceilings and pace and writes `campaign.json`.

## Rust is the source of truth

Every schema in `@sip/contracts` that a Rust crate writes mirrors a serde struct
in this repository, and each module's doc comment names the Rust file it
mirrors. **A contract question
is answered by that file, never by the mirror.** When the two disagree, the Rust
side is right and the mirror is a bug.

The mirrors are not trusted to stay in step by review:

- the pivot documents and the run bundle decode the crate's own committed
  fixtures (`crates/pivot-schema/tests/fixtures`), re-encode through the
  canonical formatter, and must come back **byte-identical**;
- the e2e records are checked against the JSON Schemas committed under
  `e2e/schemas`, key by key;
- with `PIVOT_SCHEMA_BIN` set, `@sip/toolchain` runs the real binary and
  compares the published JSON Schemas against the mirrors.

Two byte disciplines live side by side and must not be mixed: the pivot and its
run bundle are **canonically sorted** (`Canonical.format` / `formatLine`); the
e2e campaign records are `serde_json::to_string_pretty`, i.e. **declaration
order** (`Canonical.formatDeclared`, via each record's `*Json` builder).

## Running it

```sh
pnpm install
pnpm -r typecheck
pnpm -r test
```

Effect v4 is pinned exactly (`4.0.0-rc.111`, and the same for
`@effect/platform-node`). There is no build step: every package exports its
TypeScript source, and consumers run through `tsx` or `vitest`.

The workspace root installs `effect` at `ts/node_modules/effect`. A workspace
that links these packages (`link:`) takes that copy for its own `effect`
(`overrides: effect: link:<path to ts>/node_modules/effect`); the link resolves
once this workspace is installed. A schema, a tagged error or a check built on
one side is then read by the other, which two copies of `effect` do not
guarantee.

The CLI is `effect/unstable/cli`, Effect v4's own home for what used to be
`@effect/cli` — the same move that put the platform services and the process
spawner into core. There is no 4.x `@effect/cli` to depend on; the published one
still peers on `effect@^3` and `@effect/platform`.

Binaries are found under `target/release` by default and overridden per binary
by env var — `SIPFLOW`, `PIVOT_SCHEMA_BIN`. `replay` is built by the deployment
rather than by this repository, so `REPLAY_BIN` has no default at all.
