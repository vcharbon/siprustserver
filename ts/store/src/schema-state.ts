/**
 * A store's schema state over its binding (`binding.ts` `StoreBinding`: its
 * migration sets, its name, its remedy): the migrations of each set its tables
 * have not recorded, whether the database holds anything at all, and the GUARD
 * every entry point but the migration itself builds over its client: a store
 * with a pending migration refuses the entry point, naming the binding's
 * remedy.
 *
 * A set's migration is pending when its id is past the newest id its table
 * records, which is the rule the migrator applies them by; a set whose table
 * does not exist has every migration pending.
 */
import * as StoreBinding from "./binding.js"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

/** One migration a store has not applied. */
export interface Pending {
  readonly table: string
  readonly id: number
  readonly name: string
}

/** A pending migration as its record keys it: `<id>_<name>`. */
export const keyOf = (pending: Pending): string => `${pending.id}_${pending.name}`

/** The newest id `table` records; 0 where the table does not exist or records none. */
const latestIn = (table: string): Effect.Effect<number, SqlError, SqlClient> =>
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const [present] = yield* sql<{ readonly present: boolean }>`SELECT to_regclass(${table}::text) IS NOT NULL AS present`
    if (present?.present !== true) return 0
    const [row] = yield* sql<{ readonly latest: number }>`
      SELECT coalesce(max(migration_id), 0)::int AS latest FROM ${sql(table)}`
    return row?.latest ?? 0
  })

/** The migrations of `sets` the store has not applied, in the order they would run. */
export const pendingOf = (
  sets: ReadonlyArray<StoreBinding.MigrationSet>
): Effect.Effect<ReadonlyArray<Pending>, SqlError, SqlClient> =>
  Effect.gen(function*() {
    const pending: Array<Pending> = []
    for (const set of sets) {
      const latest = yield* latestIn(set.table)
      const entries = Object.keys(set.record)
        .flatMap((key) => {
          const match = /^(\d+)_(.+)$/.exec(key)
          return match === null ? [] : [{ table: set.table, id: Number(match[1]), name: match[2]! }]
        })
        .filter((entry) => entry.id > latest)
        .sort((a, b) => a.id - b.id)
      pending.push(...entries)
    }
    return pending
  })

/** Whether the database holds no relation of its own: a store nothing was ever written to. */
export const isEmpty: Effect.Effect<boolean, SqlError, SqlClient> = Effect.gen(function*() {
  const sql = yield* SqlClient
  const [row] = yield* sql<{ readonly empty: boolean }>`
    SELECT NOT EXISTS (
      SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
        AND n.nspname !~ '^pg_(toast|temp)'
    ) AS empty`
  return row?.empty === true
})

/** A store an entry point will not start on: what it has not applied, and what its plan says to do. */
export class PendingMigrations extends Schema.TaggedError<PendingMigrations>()("StoreSchema.PendingMigrations", {
  store: Schema.String,
  count: Schema.Number,
  last: Schema.String,
  remedy: Schema.String
}) {
  override get message(): string {
    return `${this.store} has ${this.count} pending migration(s), the last ${this.last}: ${this.remedy}`
  }
}

/** Fails with {@link PendingMigrations} where the store has a migration of its binding's sets to apply. */
export const guard: Effect.Effect<void, PendingMigrations | SqlError, SqlClient | StoreBinding.Service> = Effect.gen(
  function*() {
    const binding = yield* StoreBinding.Service
    const left = yield* pendingOf(binding.sets)
    if (left.length === 0) return
    return yield* new PendingMigrations({
      store: binding.name,
      count: left.length,
      last: keyOf(left[left.length - 1]!),
      remedy: binding.remedy
    })
  }
)

/** `client` with the {@link guard} run once it connects: the layer fails on a store with a pending migration. */
export const guarded = <ROut, E, RIn>(
  client: Layer.Layer<ROut | SqlClient, E, RIn>
): Layer.Layer<ROut | SqlClient, E | PendingMigrations | SqlError, RIn | StoreBinding.Service> =>
  Layer.effectDiscard(guard).pipe(Layer.provideMerge(client))
