/**
 * The pivot schema lint a document a workspace module plays passes at
 * compile: `pivot-schema lint` on the cell's `scenario.json`,
 * its error-severity findings, each `rule at path: message`. The binary is
 * the one the pack makes ready (`@sip/lab-host` `Builds.pivotSchema`:
 * prebuilt, or the checkout's release build brought up to date).
 */
import { PivotSchemaCli } from "@sip/toolchain"
import { type CliFailed, type CliUnavailable, runner } from "@sip/toolchain/spawn"
import * as ConfigProvider from "effect/ConfigProvider"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"

/** The error-severity findings of the document at `scenario`; none where it is lint-clean. */
export type Lint = (scenario: string) => Effect.Effect<ReadonlyArray<string>, Error>

/** Why a tool's run failed, in its own words where it said any. */
const whyNot = (e: CliFailed | CliUnavailable): string =>
  e._tag === "Toolchain.CliFailed"
    ? `${e.binary} ${e.args.join(" ")} exited ${e.exitCode}${e.stderr.trim() === "" ? "" : `: ${e.stderr.trim().split("\n")[0]}`}`
    : `${e.binary} does not start: ${e.cause instanceof Error ? e.cause.message : String(e.cause)}`

/**
 * The `pivot-schema` binary `ready` makes ready and names, once it answers
 * `pivot-schema schema pivot`: a tool that does not run fails here, once,
 * naming it and why, rather than on every document it would lint.
 */
export const runningPivotSchema = <E, R>(ready: Effect.Effect<string, E, R>) =>
  Effect.gen(function*() {
    const bin = yield* ready
    const cli = yield* runner
    yield* cli.ok(bin, ["schema", "pivot"]).pipe(
      Effect.mapError((e) => new Error(`the pivot schema tool ${bin} does not run: ${whyNot(e)}`))
    )
    return bin
  })

/** The lint of the `pivot-schema` binary `ready` makes ready and names, made ready first. */
export const pivotLint = <E, R>(ready: Effect.Effect<string, E, R>) =>
  Effect.gen(function*() {
  const bin = yield* ready
  const spawner = yield* Effect.context<ChildProcessSpawner.ChildProcessSpawner>()
  const layer = PivotSchemaCli.layer.pipe(Layer.provide(ConfigProvider.layerAdd(ConfigProvider.fromUnknown({ PIVOT_SCHEMA_BIN: bin }))))
  const lint: Lint = (scenario) =>
    Effect.gen(function*() {
      const cli = yield* PivotSchemaCli.Service
      const report = yield* cli.lint(scenario)
      return report.diagnostics.filter((d) => d.severity === "error").map((d) => `${d.rule} at ${d.path}: ${d.message}`)
    }).pipe(
      Effect.provide(layer),
      Effect.provideContext(spawner),
      Effect.mapError((e) => new Error(`pivot-schema lint ${scenario}: ${e instanceof Error ? e.message : String(e)}`))
    )
  return lint
})
