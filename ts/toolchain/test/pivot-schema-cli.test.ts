/**
 * The `pivot-schema` adapter. What it owes a caller is the distinction the
 * binary's exit code alone does not make: a non-zero that ANSWERS the question
 * (a document that is not canonical, a document with an error-severity finding)
 * against a non-zero that refuses it (a file that is not there).
 */
import { PivotSchemaCli } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import { fixture, rig } from "./harness.js"

const STUB = { PIVOT_SCHEMA_BIN: fixture("stub-pivot-schema.sh") }

const exec = <A, E>(use: (cli: PivotSchemaCli.Interface) => Effect.Effect<A, E>) =>
  Effect.runPromiseExit(
    Effect.gen(function* () {
      return yield* use(yield* PivotSchemaCli.Service)
    }).pipe(Effect.provide(rig(PivotSchemaCli.layer, STUB)))
  )

const value = async <A, E>(use: (cli: PivotSchemaCli.Interface) => Effect.Effect<A, E>): Promise<A> => {
  const exit = await exec(use)
  if (exit._tag !== "Success") throw new Error(`failed: ${JSON.stringify(exit)}`)
  return exit.value
}

describe("schema", () => {
  it("publishes each contract this crate owns", async () => {
    for (const contract of PivotSchemaCli.CONTRACTS) {
      expect(JSON.parse(await value((cli) => cli.schema(contract)))).toEqual({ title: contract })
    }
  })
})

describe("fmt --check", () => {
  it("answers true for a document already canonical", async () => {
    expect(await value((cli) => cli.fmtCheck(fixture("canonical.json")))).toBe(true)
  })

  it("answers false — not a failure — for one that is not", async () => {
    expect(await value((cli) => cli.fmtCheck(fixture("unsorted.json")))).toBe(false)
  })

  it("fails on a parse error, because a formatter that passes it through is not a contract", async () => {
    const exit = await exec((cli) => cli.fmtCheck(fixture("not-json.json")))
    expect(exit._tag).toBe("Failure")
    expect(JSON.stringify(exit)).toContain("Toolchain.CliFailed")
  })
})

describe("lint --json", () => {
  it("decodes the report of a clean document", async () => {
    expect(await value((cli) => cli.lint(fixture("clean.json")))).toEqual({ diagnostics: [] })
  })

  it("treats exit 1 with a valid report as DATA, not as a failure", async () => {
    const report = await value((cli) => cli.lint(fixture("broken.json")))
    expect(report.diagnostics.length).toBe(1)
    expect(report.diagnostics[0].rule).toBe("references/leg-undeclared")
    expect(report.diagnostics[0].severity).toBe("error")
  })

  it("fails when there is no report to read at all", async () => {
    const exit = await exec((cli) => cli.lint(fixture("no-such-file.json")))
    expect(exit._tag).toBe("Failure")
    expect(JSON.stringify(exit)).toContain("Toolchain.CliFailed")
  })
})
