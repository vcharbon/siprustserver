/**
 * The pivot schema lint a document a workspace module plays passes at
 * compile: `pivot-schema lint` on the cell's `scenario.json`,
 * its error-severity findings, each `rule at path: message`. The binary is
 * the one the pack makes ready (`@sip/lab-host` `Builds.pivotSchema`:
 * prebuilt, or the checkout's release build brought up to date).
 */
import { PivotSchemaCli } from "@sip/toolchain"
import * as ConfigProvider from "effect/ConfigProvider"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"

/** The error-severity findings of the document at `scenario`; none where it is lint-clean. */
export type Lint = (scenario: string) => Effect.Effect<ReadonlyArray<string>, Error>

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
