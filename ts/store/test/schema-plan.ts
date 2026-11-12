/**
 * A two-set store binding the suites migrate scratch stores by: set `a` of
 * two migrations, then set `b` of two, each creating one table, each set
 * recording its ids in its own table.
 */
import * as StoreBinding from "../src/binding.js"
import * as Effect from "effect/Effect"
import * as Migrator from "effect/unstable/sql/Migrator"
import { SqlClient } from "effect/unstable/sql/SqlClient"

/** A migration creating `table`. */
const creating = (table: string) =>
  Effect.flatMap(SqlClient, (sql) => sql.unsafe(`CREATE TABLE ${table} (id int PRIMARY KEY)`))

/** Set `a`: its table and its record. */
export const A: StoreBinding.MigrationSet = {
  table: "schema_a_migrations",
  record: { "1_first_a": creating("first_a"), "2_second_a": creating("second_a") }
}

/** Set `b`: its table and its record. */
export const B: StoreBinding.MigrationSet = {
  table: "schema_b_migrations",
  record: { "1_first_b": creating("first_b"), "2_second_b": creating("second_b") }
}

/** `set` applied by the effect migrator over its own table, the entries `keep` holds only. */
export const apply = (set: StoreBinding.MigrationSet, keep: (key: string) => boolean = () => true) =>
  Migrator.make({})({
    loader: Migrator.fromRecord(
      Object.fromEntries(Object.entries(set.record).filter(([key]) => keep(key))) as Record<
        string,
        Effect.Effect<void, unknown, SqlClient>
      >
    ),
    table: set.table
  })

/** The remedy the binding's refusals name. */
export const REMEDY = "run `store migrate` first"

/** Every set, `a` then `b`, applied in order. */
export const migrate = Effect.gen(function*() {
  const a = yield* apply(A)
  const b = yield* apply(B)
  return [...a, ...b]
})

/** The binding: its sets `a` then `b`, {@link migrate} applying them, its name and {@link REMEDY}. */
export const BINDING: StoreBinding.Interface = {
  ...StoreBinding.STORE,
  name: "the test store",
  sets: [A, B],
  migrate,
  remedy: REMEDY
}

/** {@link BINDING} as a layer. */
export const Binding = StoreBinding.layerWith(BINDING)
