/**
 * The store binding: a selector parsed through it reads in the deployment's
 * spelling, and the default binding is the store's own spelling and grammar;
 * its migrate brings an empty database to every migration its sets name. The
 * schema suite builds and drops its own database.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as StoreBinding from "../src/binding.js"
import * as Resolver from "../src/resolver.js"
import * as Selector from "../src/selector.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_binding_test")

const terms = (parsed: Selector.Selector | { readonly refused: string }) =>
  Selector.isRefused(parsed) ? parsed.refused : parsed.terms

/** The id of a migration key `<id>_<name>`. */
const idOf = (key: string): number => Number.parseInt(key, 10)

describe("StoreBinding", () => {
  it("parses in the deployment's spelling of the current term", async () => {
    const binding = StoreBinding.layerWith({ ...StoreBinding.STORE, spelling: { current: "@main" } })
    const read = await Effect.runPromise(
      Effect.all([StoreBinding.parse("@main/@lane:fake"), StoreBinding.parse("@current"), StoreBinding.current]).pipe(
        Effect.provide(binding)
      )
    )
    expect(terms(read[0])).toEqual([{ _tag: "current" }, { _tag: "lane", lane: "fake" }])
    expect(terms(read[1])).toContain("one of @main,")
    expect(read[2]).toBe("@main")
  })

  it("defaults to the store's own spelling and grammar", async () => {
    const binding = await Effect.runPromise(StoreBinding.Service.pipe(Effect.provide(StoreBinding.layer)))
    expect(binding.spelling).toEqual(Selector.DEFAULT_SPELLING)
    expect(binding.grammar).toBe(Resolver.DEFAULT_GRAMMAR)
  })

})

describe.skipIf(!Server.served)("StoreBinding's default schema on an empty database", () => {
  beforeAll(async () => {
    await Server.fresh(SCRATCH)
  }, 60_000)

  afterAll(async () => {
    await Server.drop(SCRATCH)
  }, 60_000)

  it("migrates to every key of its sets: each set's table records each of its keys, the migrate answering each", async () => {
    const { applied, recorded, expected } = await Server.on(
      SCRATCH,
      Effect.gen(function*() {
        const sql = yield* SqlClient
        const binding = yield* StoreBinding.Service
        const applied = yield* binding.migrate
        const recorded = yield* Effect.forEach(binding.sets, (set) =>
          sql<{ readonly key: string }>`
            SELECT migration_id || '_' || name AS key FROM ${sql(set.table)} ORDER BY migration_id
          `.pipe(Effect.map((rows) => rows.map((row) => row.key))))
        const expected = binding.sets.map((set) => Object.keys(set.record).sort((a, b) => idOf(a) - idOf(b)))
        return { applied: applied.map(([id, name]) => `${id}_${name}`), recorded, expected }
      }).pipe(Effect.provide(StoreBinding.layer))
    )
    expect(expected.flat().length).toBeGreaterThan(0)
    expect(recorded).toEqual(expected)
    expect(applied).toEqual(expected.flat())
  }, 60_000)
})
