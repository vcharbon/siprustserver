/**
 * THE CURRENT POPULATION: which (env, branch) makes the runs a call currently
 * stands at, held as the store's own one-row setting so the SQL views
 * (`current_runs`, `current_cells`) and the selectors (`@current`) read the
 * same answer.
 *
 * A run is current when it was made from that env on that branch, no component
 * of its code was dirty, and it finished: a run still in flight, or one of any
 * other environment, never moves what a call stands at. A store with no
 * setting has no current run at all, and asking it for its setting fails
 * ({@link NoCurrentPopulation}) rather than answer with an empty population.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

/** The env and branch whose clean, finished runs are current. */
export interface Population {
  readonly env: string
  readonly branch: string
}

/** The store states no current population. */
export class NoCurrentPopulation extends Schema.TaggedError<NoCurrentPopulation>()("Current.NoCurrentPopulation", {}) {
  override get message(): string {
    return "the store states no current population (current_population holds no row): nothing is current"
  }
}

export interface Interface {
  /** The setting; fails where the store states none. */
  readonly get: Effect.Effect<Population, NoCurrentPopulation | SqlError>
  /** States the current population, replacing any earlier setting. */
  readonly set: (population: Population) => Effect.Effect<void, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Current") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const get = Effect.gen(function*() {
      const rows = yield* sql<Population>`SELECT env, branch FROM current_population`
      const row = rows[0]
      if (row === undefined) return yield* new NoCurrentPopulation()
      return row
    })

    const set = Effect.fn("Current.set")(function*(population: Population) {
      yield* sql`
        INSERT INTO current_population (env, branch) VALUES (${population.env}, ${population.branch})
        ON CONFLICT (one) DO UPDATE SET env = EXCLUDED.env, branch = EXCLUDED.branch
      `
    })

    return Service.of({ get, set })
  })
)
