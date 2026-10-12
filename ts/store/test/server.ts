/**
 * The postgres server the DB-bound suites build their own databases on:
 * `STORE_TEST_URL` (any database of it), else the client's default. A suite
 * skips where no server answers.
 */
import * as Effect from "effect/Effect"
import type { SqlClient } from "effect/unstable/sql/SqlClient"
import * as Client from "../src/client.js"
import * as Current from "../src/current.js"
import * as Migrator from "../src/migrator.js"
import { CURRENT_POPULATION } from "./identity.js"

const SERVER = process.env["STORE_TEST_URL"] ?? Client.DEFAULT_URL

/** The url of `database` on the test server. */
export const urlFor = (database: string): string => Client.atDatabase(SERVER, database)

/** A fresh store: the schema migrated, and {@link CURRENT_POPULATION} stated as current. */
export const migrated: Effect.Effect<void, unknown, SqlClient> = Effect.gen(function*() {
  yield* Migrator.run
  yield* Effect.flatMap(Current.Service, (current) => current.set(CURRENT_POPULATION)).pipe(
    Effect.provide(Current.layer)
  )
})
