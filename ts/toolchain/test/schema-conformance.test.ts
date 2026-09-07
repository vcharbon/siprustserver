/**
 * The mirrors in `@sip/contracts` against the JSON Schemas the REAL
 * `pivot-schema` binary publishes.
 *
 * Env-gated on `PIVOT_SCHEMA_BIN`: a fresh checkout has no `cargo build`, and a
 * suite that needs one is a suite nobody runs. When the binary is there this is
 * the strongest drift alarm the two languages have — it sees the OPTIONAL
 * top-level fields no committed fixture happens to exercise, which the byte
 * round-trip in `@sip/contracts` cannot.
 *
 * It lives here rather than in `@sip/contracts` because reaching a binary means
 * spawning one, and this package is the only one that knows subprocesses exist.
 */
import { Bundle, Pivot, Rules } from "@sip/contracts"
import { PivotSchemaCli } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import * as fs from "node:fs"
import { describe, expect, it } from "vitest"
import { rig } from "./harness.js"

const BIN = process.env.PIVOT_SCHEMA_BIN
const available = BIN !== undefined && BIN.length > 0 && fs.existsSync(BIN)

type Node = { $ref?: string; $defs?: Record<string, Node>; properties?: Record<string, unknown>; required?: Array<string> }

const inventory = (node: Node) => ({
  properties: Object.keys(node.properties ?? {}).sort(),
  required: (node.required ?? []).slice().sort()
})

/** schemars puts the top-level type behind a `$ref` into `$defs`; resolve it. */
const rootOf = (document: Node): Node => {
  if (document.$ref === undefined) return document
  const name = document.$ref.split("/").pop() ?? ""
  const target = document.$defs?.[name]
  if (target === undefined) throw new Error(`unresolvable $ref ${document.$ref}`)
  return target
}

const published = (contract: PivotSchemaCli.Contract) =>
  Effect.runPromise(
    Effect.gen(function* () {
      const cli = yield* PivotSchemaCli.Service
      return yield* cli.schema(contract)
    }).pipe(Effect.provide(rig(PivotSchemaCli.layer, { PIVOT_SCHEMA_BIN: BIN ?? "" })))
  )

const mirrors: Array<[PivotSchemaCli.Contract, Schema.Top]> = [
  ["pivot", Pivot.PivotV3],
  ["rules", Rules.RuleFile],
  ["run-config", Bundle.RunConfig],
  ["verdict", Bundle.RunVerdict],
  ["timing", Bundle.RunTiming],
  ["recording", Bundle.RecordedMessage],
  ["rfc", Bundle.RunRfcAudit]
]

describe.skipIf(!available)("every contract the binary publishes", () => {
  it.each(mirrors)("%s states the same keys, and the same ones required", async (contract, schema) => {
    const document = rootOf(JSON.parse(await published(contract)) as Node)
    const mine = Schema.toJsonSchemaDocument(schema as never).schema as Node
    expect(inventory(mine)).toEqual(inventory(document))
  })
})
