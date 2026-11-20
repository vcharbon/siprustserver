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
 * does not exist has every migration pending. A migration at or below that
 * newest id which the table does not record under its id and name is SKIPPED:
 * the migrator never applies it (two branches took one number, and the store
 * ran the other's). The guard and the unattended step refuse such a store.
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

/** The `<id>_<name>` keys `table` records; none where the table does not exist. */
const recordedIn = (table: string): Effect.Effect<ReadonlySet<string>, SqlError, SqlClient> =>
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const [present] = yield* sql<{ readonly present: boolean }>`SELECT to_regclass(${table}::text) IS NOT NULL AS present`
    if (present?.present !== true) return new Set<string>()
    const rows = yield* sql<{ readonly key: string }>`SELECT migration_id || '_' || name AS key FROM ${sql(table)}`
    return new Set(rows.map((row) => row.key))
  })

/** A set's migrations as its record keys them, oldest first; a key that is no `<id>_<name>` is left out. */
const entriesOf = (set: StoreBinding.MigrationSet): ReadonlyArray<Pending> =>
  Object.keys(set.record)
    .flatMap((key) => {
      const match = /^(\d+)_(.+)$/.exec(key)
      return match === null ? [] : [{ table: set.table, id: Number(match[1]), name: match[2]! }]
    })
    .sort((a, b) => a.id - b.id)

/** One migration a store skipped, and the newest id its table records. */
export interface Skipped extends Pending {
  readonly newest: number
}

/**
 * The migrations of `sets` the store skipped: numbered at or below the newest
 * id their table records, and not recorded there under their id and name.
 */
export const skippedOf = (
  sets: ReadonlyArray<StoreBinding.MigrationSet>
): Effect.Effect<ReadonlyArray<Skipped>, SqlError, SqlClient> =>
  Effect.gen(function*() {
    const skipped: Array<Skipped> = []
    for (const set of sets) {
      const newest = yield* latestIn(set.table)
      if (newest === 0) continue
      const recorded = yield* recordedIn(set.table)
      skipped.push(
        ...entriesOf(set)
          .filter((entry) => entry.id <= newest && !recorded.has(keyOf(entry)))
          .map((entry) => ({ ...entry, newest }))
      )
    }
    return skipped
  })

/** The migrations of `sets` the store has not applied, in the order they would run. */
export const pendingOf = (
  sets: ReadonlyArray<StoreBinding.MigrationSet>
): Effect.Effect<ReadonlyArray<Pending>, SqlError, SqlClient> =>
  Effect.gen(function*() {
    const pending: Array<Pending> = []
    for (const set of sets) {
      const latest = yield* latestIn(set.table)
      pending.push(...entriesOf(set).filter((entry) => entry.id > latest))
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

/** A store that skipped a migration: no migration run will ever apply it, so nothing starts on the store. */
export class SkippedMigrations extends Schema.TaggedError<SkippedMigrations>()("StoreSchema.SkippedMigrations", {
  store: Schema.String,
  table: Schema.String,
  skipped: Schema.String,
  newest: Schema.Number
}) {
  override get message(): string {
    return `${this.store} never applied ${this.skipped} and already holds migration ${this.newest} of ${this.table}: ` +
      `number ${this.skipped} past ${this.newest} (a migration is numbered past every one its base branch holds)`
  }
}

/** Fails with {@link SkippedMigrations}, naming the first, where the store skipped a migration of `binding`'s sets. */
export const refuseSkipped = (
  binding: Pick<StoreBinding.Interface, "name" | "sets">
): Effect.Effect<void, SkippedMigrations | SqlError, SqlClient> =>
  Effect.gen(function*() {
    const [first] = yield* skippedOf(binding.sets)
    if (first === undefined) return
    return yield* new SkippedMigrations({
      store: binding.name,
      table: first.table,
      skipped: keyOf(first),
      newest: first.newest
    })
  })

/**
 * Fails with {@link SkippedMigrations} where the store skipped a migration of
 * its binding's sets, else with {@link PendingMigrations} where it has one to
 * apply.
 */
export const guard: Effect.Effect<
  void,
  PendingMigrations | SkippedMigrations | SqlError,
  SqlClient | StoreBinding.Service
> = Effect.gen(
  function*() {
    const binding = yield* StoreBinding.Service
    yield* refuseSkipped(binding)
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
): Layer.Layer<ROut | SqlClient, E | PendingMigrations | SkippedMigrations | SqlError, RIn | StoreBinding.Service> =>
  Layer.effectDiscard(guard).pipe(Layer.provideMerge(client))
