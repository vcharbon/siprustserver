/**
 * `pivot-schema` — the JSON Schemas a mirror is checked against, the normative
 * canonical formatter, and the semantic lint a schema cannot state.
 *
 * `lint --json` exits 1 with a VALID report on stdout when it finds an
 * error-severity diagnostic. That is the tool answering the question, not the
 * tool failing, so {@link Interface.lint} decodes the report and hands it back;
 * the caller decides what an error costs. `fmt --check` reads the same way for
 * its one expected non-zero.
 */
import { Lint } from "@sip/contracts"
import type * as Config from "effect/Config"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import type * as Schema from "effect/Schema"
import { PIVOT_SCHEMA } from "./binaries.js"
import { type CliFailed, type CliUnavailable, refused, runner } from "./spawn.js"

/** The contracts `pivot-schema schema` publishes. */
export const CONTRACTS = ["pivot", "rules", "run-config", "verdict", "timing", "recording", "rfc"] as const
export type Contract = (typeof CONTRACTS)[number]

export interface Interface {
  /** `pivot-schema schema <contract>` → that contract's JSON Schema, as text. */
  readonly schema: (contract: Contract) => Effect.Effect<string, CliFailed | CliUnavailable>
  /**
   * `pivot-schema fmt --check <file>` → whether the file is already canonical.
   * A parse error is a failure, not a `false`: a formatter that passes malformed
   * input through is not a contract.
   */
  readonly fmtCheck: (file: string) => Effect.Effect<boolean, CliFailed | CliUnavailable>
  /** `pivot-schema lint --json <file>` → the decoded report, findings and all. */
  readonly lint: (
    file: string
  ) => Effect.Effect<Lint.Report, CliFailed | CliUnavailable | Schema.SchemaError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/toolchain/PivotSchemaCli") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function* () {
    const cli = yield* runner
    const bin = yield* PIVOT_SCHEMA

    const schema = Effect.fn("PivotSchemaCli.schema")(function* (contract: Contract) {
      return yield* cli.ok(bin, ["schema", contract])
    })

    const fmtCheck = Effect.fn("PivotSchemaCli.fmtCheck")(function* (file: string) {
      const args = ["fmt", "--check", file]
      const output = yield* cli.all(bin, args)
      if (output.exitCode === 0) return true
      // The one non-zero this command ANSWERS with rather than fails on.
      if (output.stderr.includes("not canonically formatted")) return false
      return yield* refused(bin, args, output)
    })

    const lint = Effect.fn("PivotSchemaCli.lint")(function* (file: string) {
      const args = ["lint", "--json", file]
      const output = yield* cli.all(bin, args)
      // Exit 1 rides a valid report: an error-severity finding is data.
      if (output.stdout.trim().length === 0) return yield* refused(bin, args, output)
      return yield* Lint.decodeLintReport(JSON.parse(output.stdout) as unknown)
    })

    return Service.of({ schema, fmtCheck, lint })
  })
)

/** The binary this service will run, for a caller that wants to say so out loud. */
export const binary: Config.Config<string> = PIVOT_SCHEMA

export * as PivotSchemaCli from "./pivot-schema-cli.js"
