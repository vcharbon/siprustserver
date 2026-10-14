# `ts/` — the TypeScript side of the replay toolchain

A pnpm workspace. Nine packages, four of them one layer each of the same stack:

| package | name | owns |
| --- | --- | --- |
| `contracts/` | `@sip/contracts` | Effect Schema mirrors of the Rust-owned wire formats, the canonical formatter, pure readings of those formats (call key, step order, a cell's main error), and the report formats the tooling publishes (`sip.compare/1`, `sip.run-report/1`, their JSON Schemas under `contracts/schemas/`) |
| `toolchain/` | `@sip/toolchain` | typed adapters over the Rust CLIs (`sipflow`, `pivot-schema`, `replay`) and the process primitives above them (a command into a file or a log, a detached command and its liveness, `git`, a node child's heap) — the ONLY package that knows subprocesses exist |
| `pipeline/` | `@sip/pipeline` | flows ingest, the correlation rule engine, the cut, and case assembly |
| `driver/` | `@sip/driver` | the campaign orchestrator: cells in, one `campaign.json` out, exit code is the verdict |
| `store/` | `@sip/store` | the run store: the postgres record of cuts, runs and what each call said, the readers a queue and a comparison use, and the run selectors |
| `viewer-core/` | `@sip/viewer-core` | the callflow viewer's core: pivot types generated from the Rust schema, the pure ladder layouts, their SVG components, the run-bundle and capture-flows readers, and the text export |
| `viewer-app/` | `@sip/viewer-app` | the viewer's Vite + React SPA over a deployment's server: cases, runs, the triage queue and study, and the compare and run report pages over the report formats |
| `corpus/` | `@sip/corpus` | the orchestration of a capture corpus over a deployment's seams (`deployment/`): extraction, cut and replay on a lane, jobs and requests, and the compare and run reports a command prints and a viewer serves |
| `cli/` | `@sip/cli` | the command lines over `@sip/corpus`: flag sets and command builders taking a deployment's facts |

Dependencies point one way only: `driver → pipeline → toolchain → contracts`,
`store → contracts`, `viewer-app → viewer-core` (the app reads the report
formats' types from `contracts`, nothing else), and `cli → corpus → {driver,
store, viewer-core}`. The viewer's server is the deployment's: it serves the
app's build and the documents the app draws. A policy package never imports the
store: what it reads of a run reaches it as contract vocabulary. Only
`@sip/toolchain` spawns a process; `@sip/corpus` runs every child through it.

The store's tests build and drop their own databases on a postgres server:
`STORE_TEST_URL` names one (any database of it, `postgresql:///postgres?host=…`),
else the client's default socket. A suite whose server does not answer fails,
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
is visible in the campaign document rather than buried in a layer.

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
help every entry point ends with). Every default layer builds; roots, tools,
the anonymiser, a lane, the cell host, a job's entry point and the tracker's
texts refuse by name when a command reaches for them, and the rest state nothing.
The case-id grammar has one source, the binding: the resolver
(`StoreBinding.resolver`), the id forms and the citation scan all read it.

## What a campaign leaves on disk

```text
<run-dir>/
  campaign.json                 the aggregate index (`e2e_core::CampaignIndex`)
  specs/<cell>.run-spec.json    what the driver asked the interpreter for
  <cell>/                       IS the interpreter out_dir — it WIPES this
  <cell>/error.txt              a cell that never produced a result
```

A cell is `{case, shape, infra}` where `shape` is `pivot-replay` or `rust-test`
and `infra` is the lane or the crate. Cells run one at a time unless a campaign
says otherwise: a replay cell binds real sockets and a crate cell compiles, and a
flaky matrix is worse than a slow one.

```sh
pnpm --filter @sip/driver exec tsx src/main.ts run campaign.json --run-dir runs/1
```

The neutral entry point runs rust-test cells and refuses every pivot-replay cell
— it composes no lane presets. A deployment ships its own entry point with its
layers merged in.

## Rust is the source of truth

Every schema in `@sip/contracts` mirrors a serde struct in this repository, and
each module's doc comment names the Rust file it mirrors. **A contract question
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

The CLI is `effect/unstable/cli`, Effect v4's own home for what used to be
`@effect/cli` — the same move that put the platform services and the process
spawner into core. There is no 4.x `@effect/cli` to depend on; the published one
still peers on `effect@^3` and `@effect/platform`.

Binaries are found under `target/release` by default and overridden per binary
by env var — `SIPFLOW`, `PIVOT_SCHEMA_BIN`. `replay` is built by the deployment
rather than by this repository, so `REPLAY_BIN` has no default at all.
