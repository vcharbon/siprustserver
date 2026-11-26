/**
 * The unattended step and the guard over a two-set store binding, against a real
 * postgres: a database the server lacks is created and migrated with no dump;
 * an empty one is migrated with no dump; a current one is left alone with no
 * dump; one with a pending migration gets a custom-format dump named after
 * that migration, then is migrated. A dump that fails migrates nothing. The
 * guard refuses a store with a pending migration, naming the binding's store and
 * remedy, and lets a current one through. A store that skipped a migration
 * (another migration took its number, or a later one ran first) is refused by
 * the guard and by the step, which then dumps and applies nothing.
 */
import { NodeServices } from "@effect/platform-node"
import type * as StoreBinding from "../src/binding.js"
import { layerFromUrl } from "../src/client.js"
import { drop, fresh, on, scratch, served, urlFor } from "./server.js"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, afterEach, beforeEach, describe, expect, it } from "vitest"
import * as AutoMigrate from "../src/auto-migrate.js"
import * as SchemaState from "../src/schema-state.js"
import { A, apply, B, BINDING, Binding, migrate, REMEDY } from "./schema-plan.js"

const MISSING = await scratch("store_schema_missing")
const EMPTY = await scratch("store_schema_empty")
const CURRENT = await scratch("store_schema_current")
const BEHIND = await scratch("store_schema_behind")
const SKIPPED = await scratch("store_schema_skipped")

let backups: string
beforeEach(() => {
  backups = fs.mkdtempSync(path.join(os.tmpdir(), "store-schema-"))
})
afterEach(() => {
  fs.rmSync(backups, { recursive: true, force: true })
})

afterAll(async () => {
  if (!served) return
  for (const database of [MISSING, EMPTY, CURRENT, BEHIND, SKIPPED]) await drop(database)
})

/** `step` over the test binding and the node services. */
const provided = <A, E>(step: Effect.Effect<A, E, StoreBinding.Service | NodeServices.NodeServices>) =>
  step.pipe(Effect.provide(Binding), Effect.provide(NodeServices.layer))

const auto = (database: string) => Effect.runPromise(provided(AutoMigrate.run(urlFor(database), backups)))

const pending = (database: string) => on(database, SchemaState.pendingOf(BINDING.sets))

const dumps = () => fs.readdirSync(backups)

const EVERY = ["1_first_a", "2_second_a", "1_first_b", "2_second_b"]

/** `BEHIND` holding every migration but the last of set `b`. */
const oneBehind = async () => {
  await fresh(BEHIND)
  await on(BEHIND, apply(A))
  await on(BEHIND, apply(B, (key) => key !== "2_second_b"))
  expect((await pending(BEHIND)).map(SchemaState.keyOf)).toEqual(["2_second_b"])
}

/** Set `a` as another branch numbered it: its second migration under another name. */
const A_OF_ANOTHER_BRANCH: StoreBinding.MigrationSet = {
  table: A.table,
  record: {
    "1_first_a": A.record["1_first_a"],
    "2_taken_a": Effect.flatMap(SqlClient, (sql) => sql.unsafe("CREATE TABLE taken_a (id int PRIMARY KEY)"))
  }
}

/** `SKIPPED` holding set `a` as the other branch numbered it: `2_second_a` never ran, and 2 is recorded. */
const numberTaken = async () => {
  await fresh(SKIPPED)
  await on(SKIPPED, apply(A_OF_ANOTHER_BRANCH))
}

const tables = (database: string) =>
  on(
    database,
    Effect.flatMap(SqlClient, (sql) =>
      sql<{ readonly name: string }>`SELECT tablename AS name FROM pg_tables WHERE schemaname = 'public' ORDER BY 1`)
  ).then((rows) => rows.map((row) => row.name))

describe.skipIf(!served)("the unattended step", () => {
  it("refuses a store that skipped a migration, dumping and applying nothing", async () => {
    await numberTaken()
    const exit = await Effect.runPromiseExit(provided(AutoMigrate.run(urlFor(SKIPPED), backups)))
    expect(String(exit)).toContain(
      `the test store never applied 2_second_a and already holds migration 2 of ${A.table}: number 2_second_a past 2`
    )
    expect(dumps()).toEqual([])
    expect(await tables(SKIPPED)).toEqual(["first_a", A.table, "taken_a"].sort())
  })

  it("creates a database the server lacks and migrates every set, with no dump", async () => {
    await drop(MISSING)
    const outcome = await auto(MISSING)
    expect(outcome.created).toBe(true)
    expect(outcome.backup).toBeUndefined()
    expect(outcome.applied.map(([id, name]) => `${id}_${name}`)).toEqual(EVERY)
    expect(await pending(MISSING)).toEqual([])
    expect(dumps()).toEqual([])
  })

  it("migrates an empty database with no dump, every set pending before in the plan's order", async () => {
    await fresh(EMPTY)
    expect(await on(EMPTY, SchemaState.isEmpty)).toBe(true)
    expect((await pending(EMPTY)).map((entry) => `${entry.table} ${SchemaState.keyOf(entry)}`)).toEqual([
      `${A.table} 1_first_a`,
      `${A.table} 2_second_a`,
      `${B.table} 1_first_b`,
      `${B.table} 2_second_b`
    ])
    const outcome = await auto(EMPTY)
    expect(outcome.created).toBe(false)
    expect(outcome.backup).toBeUndefined()
    expect(await pending(EMPTY)).toEqual([])
    expect(await on(EMPTY, SchemaState.isEmpty)).toBe(false)
    expect(dumps()).toEqual([])
  })

  it("leaves a current store alone, with no dump", async () => {
    await fresh(CURRENT)
    await on(CURRENT, migrate)
    const outcome = await auto(CURRENT)
    expect(outcome).toEqual({ database: CURRENT, created: false, backup: undefined, applied: [] })
    expect(dumps()).toEqual([])
  })

  it("dumps a store with one pending migration, says where, then migrates it", async () => {
    await oneBehind()
    const told: Array<{ file: string; there: boolean }> = []
    const outcome = await Effect.runPromise(
      provided(AutoMigrate.run(urlFor(BEHIND), backups, (file) =>
        Effect.sync(() => {
          told.push({ file, there: fs.existsSync(file) })
        })))
    )
    expect(outcome.applied.map(([id, name]) => `${id}_${name}`)).toEqual(["2_second_b"])
    expect(dumps()).toHaveLength(1)
    const [file] = dumps()
    expect(file).toMatch(/^\d{8}T\d{6}Z-before-2_second_b\.dump$/)
    expect(outcome.backup).toBe(path.join(backups, file!))
    expect(told).toEqual([{ file: outcome.backup, there: true }])
    expect(fs.readFileSync(outcome.backup!).subarray(0, 5).toString()).toBe("PGDMP")
    expect(await pending(BEHIND)).toEqual([])
  })

  it("runs two concurrent steps one after the other: one dump, one migration", async () => {
    await oneBehind()
    const [a, b] = await Promise.all([auto(BEHIND), auto(BEHIND)])
    expect([a.applied.length, b.applied.length].sort()).toEqual([0, 1])
    expect([a.backup, b.backup].filter((x) => x !== undefined)).toHaveLength(1)
    expect(dumps()).toHaveLength(1)
    expect(await pending(BEHIND)).toEqual([])
  })

  it("creates a missing database once when two steps race for it", async () => {
    await drop(MISSING)
    const [a, b] = await Promise.all([auto(MISSING), auto(MISSING)])
    expect([a.created, b.created].filter(Boolean).length).toBeLessThanOrEqual(1)
    expect(await pending(MISSING)).toEqual([])
  })

  it("migrates nothing when the dump fails", async () => {
    await oneBehind()
    const bin = fs.mkdtempSync(path.join(os.tmpdir(), "store-schema-bin-"))
    fs.writeFileSync(path.join(bin, "pg_dump"), "#!/bin/sh\necho 'dump refused' >&2\nexit 3\n", { mode: 0o755 })
    const before = process.env.PATH
    process.env.PATH = `${bin}:${before ?? ""}`
    try {
      const exit = await Effect.runPromiseExit(provided(AutoMigrate.run(urlFor(BEHIND), backups)))
      expect(Exit.isFailure(exit)).toBe(true)
      expect(String(exit)).toContain("pg_dump exited 3, nothing migrated: dump refused")
    } finally {
      process.env.PATH = before
      fs.rmSync(bin, { recursive: true, force: true })
    }
    expect(dumps()).toEqual([])
    expect((await pending(BEHIND)).map(SchemaState.keyOf)).toEqual(["2_second_b"])
  })
})

describe.skipIf(!served)("the schema guard", () => {
  const guardedOn = (database: string) =>
    Effect.runPromiseExit(
      Effect.void.pipe(Effect.provide(SchemaState.guarded(layerFromUrl(urlFor(database))).pipe(Layer.provide(Binding))))
    )

  it("refuses a store with a pending migration, naming the store, the last pending and the remedy", async () => {
    await oneBehind()
    const exit = await guardedOn(BEHIND)
    expect(Exit.isFailure(exit)).toBe(true)
    expect(String(exit)).toContain(`the test store has 1 pending migration(s), the last 2_second_b: ${REMEDY}`)
  })

  it("refuses a store whose newest migration ran over one it never applied, before any pending one", async () => {
    await fresh(SKIPPED)
    await on(SKIPPED, apply(A, (key) => key === "2_second_a"))
    const exit = await guardedOn(SKIPPED)
    expect(Exit.isFailure(exit)).toBe(true)
    expect(String(exit)).toContain(`the test store never applied 1_first_a and already holds migration 2 of ${A.table}`)
    expect((await on(SKIPPED, SchemaState.skippedOf(BINDING.sets))).map(SchemaState.keyOf)).toEqual(["1_first_a"])
  })

  it("refuses a store where another migration took a number, naming the one never applied", async () => {
    await numberTaken()
    expect(String(await guardedOn(SKIPPED))).toContain("the test store never applied 2_second_a")
  })

  it("lets a current store through", async () => {
    await fresh(CURRENT)
    await on(CURRENT, migrate)
    expect(await on(CURRENT, SchemaState.skippedOf(BINDING.sets))).toEqual([])
    expect(Exit.isSuccess(await guardedOn(CURRENT))).toBe(true)
  })
})
