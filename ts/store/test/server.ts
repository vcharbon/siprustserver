/**
 * The postgres server the DB-bound suites build their own databases on:
 * `STORE_TEST_URL` (any database of it), else the client's default.
 *
 * A DB-bound suite whose server does not answer FAILS: importing this module
 * throws, naming the url it tried. `STORE_TEST_SKIP=1` is the one way to skip
 * them instead, stated by whoever runs the suite with no server on purpose.
 * Other packages' DB-bound suites import it as `@sip/store/test-server`.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as Client from "../src/client.js"
import * as Current from "../src/current.js"
import * as Migrator from "../src/migrator.js"
import { CURRENT_POPULATION } from "./identity.js"

const SERVER = process.env["STORE_TEST_URL"] ?? Client.DEFAULT_URL

/** The url of `database` on the test server. */
export const urlFor = (database: string): string => Client.atDatabase(SERVER, database)

/** Whether the run opted out of the DB-bound suites. */
const skipping = process.env["STORE_TEST_SKIP"] === "1"

/** Whether the server answers; with no opt-out, a silent server is an error, never a skip. */
export const served: boolean = await Effect.runPromise(
  Effect.gen(function*() {
    const sql = yield* SqlClient
    yield* sql`SELECT 1`
  }).pipe(
    Effect.provide(Client.layerFromUrl(urlFor("postgres"))),
    Effect.match({ onFailure: () => false, onSuccess: () => true })
  )
)

if (!served && !skipping) {
  throw new Error(
    `no postgres answers at ${urlFor("postgres")}: name one with STORE_TEST_URL, or skip the DB-bound ` +
      "suites with STORE_TEST_SKIP=1"
  )
}

/** `database` gone (`Client.dropDatabase`). */
export const dropDatabase = Client.dropDatabase

/** `effect` run against `database` of the test server. */
export const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

/** `database` made empty: dropped and created again. */
export const fresh = (database: string): Promise<void> =>
  on(
    "postgres",
    Effect.gen(function*() {
      const sql = yield* SqlClient
      yield* Client.dropDatabase(database)
      yield* sql.unsafe(`CREATE DATABASE ${database}`)
    })
  )

/** `database` gone, off the test server. */
export const drop = (database: string): Promise<void> => on("postgres", Client.dropDatabase(database))

/** A suite's own database name under this checkout (`Client.scratchDatabase`). */
export const scratch = (base: string): Promise<string> =>
  Effect.runPromise(Client.scratchDatabase(base, process.cwd()).pipe(Effect.provide(NodeServices.layer)))

/** A fresh store: the schema migrated, and {@link CURRENT_POPULATION} stated as current. */
export const migrated: Effect.Effect<void, unknown, SqlClient> = Effect.gen(function*() {
  yield* Migrator.run
  yield* Effect.flatMap(Current.Service, (current) => current.set(CURRENT_POPULATION)).pipe(
    Effect.provide(Current.layer)
  )
})
