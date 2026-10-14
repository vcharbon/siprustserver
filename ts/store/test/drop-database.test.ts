/**
 * A scratch database is dropped whatever still holds it: this role's own
 * sessions are ended first and the drop is never forced, so a backend the
 * role may not end (an autovacuum worker, run as the bootstrap superuser) is
 * left to the drop's own signal instead of refusing it. A drop refused as in
 * use (SQLSTATE 55006) is retried; any other refusal is final.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { SqlError, SqlSyntaxError, UnknownError } from "effect/unstable/sql/SqlError"
import { describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"

/** The `pg` driver's error for SQLSTATE `code`. */
const pgError = (code: string, message: string): Error => Object.assign(new Error(message), { code })

describe("Client.inUse", () => {
  it("reads a drop refused because a session holds the database as in use", () => {
    const cause = pgError("55006", "database \"x\" is being accessed by other users")
    const error = new SqlError({
      reason: new UnknownError({ cause, message: "Failed to execute statement", operation: "execute" })
    })
    expect(Client.sqlState(error)).toBe("55006")
    expect(Client.inUse(error)).toBe(true)
  })

  it("reads any other refusal as final", () => {
    const cause = pgError("42601", "syntax error")
    const error = new SqlError({
      reason: new SqlSyntaxError({ cause, message: "Failed to execute statement", operation: "execute" })
    })
    expect(Client.inUse(error)).toBe(false)
  })
})

const SCRATCH = await Server.scratch("store_drop_database_test")

describe.skipIf(!Server.served)("Client.dropDatabase", () => {
  it("drops a database a session of this role still holds", async () => {
    await Server.fresh(SCRATCH)
    const held = Effect.runPromise(
      Effect.gen(function*() {
        const sql = yield* SqlClient
        yield* sql`SELECT pg_sleep(30)`
      }).pipe(Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH))), Effect.exit)
    )
    await new Promise((resolve) => setTimeout(resolve, 300))
    await Server.drop(SCRATCH)
    const left = await Server.on(
      "postgres",
      Effect.flatMap(SqlClient, (sql) => sql<{ n: number }>`SELECT count(*)::int AS n FROM pg_database WHERE datname = ${SCRATCH}`)
    )
    expect(left[0]!.n).toBe(0)
    await held
  }, 20_000)
})
