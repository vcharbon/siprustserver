# `ts/` — the TypeScript side of the replay toolchain

A pnpm workspace. Four packages, each one layer of the same stack:

| package | name | owns |
| --- | --- | --- |
| `contracts/` | `@sip/contracts` | Effect Schema mirrors of the Rust-owned wire formats, the canonical formatter, and nothing else |
| `toolchain/` | `@sip/toolchain` | typed adapters over the Rust CLIs (`sipflow`, `pivot-schema`, `replay`) — the ONLY package that knows subprocesses exist |
| `pipeline/` | `@sip/pipeline` | flows ingest, the correlation rule engine, the cut, and case assembly |
| `driver/` | `@sip/driver` | the campaign orchestrator: cells in, one `campaign.json` out, exit code is the verdict |

Dependencies point one way only: `driver → pipeline → toolchain → contracts`.

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
