/**
 * The mirrors in `@sip/contracts` against the JSON Schemas the REAL
 * `pivot-schema` binary publishes.
 *
 * The binary is `./pivot-schema-bin.js`'s: missing, the suite fails unless the
 * run opts out. It is the strongest drift alarm the two languages have — it
 * sees the OPTIONAL top-level fields no committed fixture happens to exercise,
 * which the byte round-trip in `@sip/contracts` cannot.
 *
 * It lives here rather than in `@sip/contracts` because reaching a binary means
 * spawning one, and this package is the only one that knows subprocesses exist.
 */
import { Bundle, Pivot, Rules } from "@sip/contracts"
import { PivotSchemaCli } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { expect, it } from "vitest"
import { rig } from "./harness.js"
import { BIN, describeBinary } from "./pivot-schema-bin.js"

type Node = {
  $ref?: string
  $defs?: Record<string, Node>
  anyOf?: Array<Node>
  oneOf?: Array<Node>
  properties?: Record<string, unknown>
  required?: Array<string>
}

/**
 * The keys a value may state and the keys it must, `$ref`s resolved in `defs`.
 * A union of object arms (one shape per wire arm) may state any arm's key and
 * must state only the keys every arm requires.
 */
const keysOf = (node: Node, defs: Record<string, Node>): { properties: Set<string>; required: Set<string> } => {
  if (node.$ref !== undefined) {
    const target = defs[node.$ref.split("/").pop() ?? ""]
    if (target === undefined) throw new Error(`unresolvable $ref ${node.$ref}`)
    return keysOf(target, defs)
  }
  const arms = node.anyOf ?? node.oneOf
  if (arms !== undefined && arms.length > 0) {
    const each = arms.map((arm) => keysOf(arm, defs))
    return {
      properties: new Set(each.flatMap((arm) => [...arm.properties])),
      required: new Set([...each[0]!.required].filter((key) => each.every((arm) => arm.required.has(key))))
    }
  }
  return { properties: new Set(Object.keys(node.properties ?? {})), required: new Set(node.required ?? []) }
}

const inventory = (node: Node, defs: Record<string, Node> = {}) => {
  const keys = keysOf(node, defs)
  return { properties: [...keys.properties].sort(), required: [...keys.required].sort() }
}

const published = (contract: PivotSchemaCli.Contract) =>
  Effect.runPromise(
    Effect.gen(function* () {
      const cli = yield* PivotSchemaCli.Service
      return yield* cli.schema(contract)
    }).pipe(Effect.provide(rig(PivotSchemaCli.layer, { PIVOT_SCHEMA_BIN: BIN })))
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

describeBinary("every contract the binary publishes", () => {
  it.each(mirrors)("%s states the same keys, and the same ones required", async (contract, schema) => {
    // schemars puts the top-level type behind a `$ref` into `$defs`.
    const document = JSON.parse(await published(contract)) as Node
    const mine = Schema.toJsonSchemaDocument(schema as never)
    expect(inventory(mine.schema as Node, mine.definitions as Record<string, Node>))
      .toEqual(inventory(document, document.$defs ?? {}))
  })
})
