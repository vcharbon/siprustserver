/**
 * The connection to the store's postgres database.
 *
 * `DATABASE_URL` names it; without it the default is a local server reached
 * over its unix socket, so no secret is needed. A deployment with a server of
 * its own hands its url to {@link layerFromUrl}.
 */
import * as Config from "effect/Config"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as Redacted from "effect/Redacted"
import * as Schedule from "effect/Schedule"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import { PgClient } from "@effect/sql-pg"
import * as crypto from "node:crypto"
import * as path from "node:path"

/** A local server's `sip_store` database, over the unix socket. */
export const DEFAULT_URL = "postgresql:///sip_store?host=/var/run/postgresql"

export const url = Config.redacted("DATABASE_URL").pipe(
  Config.withDefault(Redacted.make(DEFAULT_URL))
)

export const layerFromUrl = (value: string) => PgClient.layer({ url: Redacted.make(value) })

/** The default wiring: `DATABASE_URL`, else {@link DEFAULT_URL}. */
export const layer = Layer.unwrap(Effect.map(url, (value) => PgClient.layer({ url: value })))

/** The same server's database `database`: `url` with its database name replaced. */
export const atDatabase = (value: string, database: string): string => {
  const parsed = new URL(value)
  parsed.pathname = `/${database}`
  return parsed.toString()
}

/** The directory holding `.git` at or above `from`: the checkout, else `from` itself. */
const checkoutOf = Effect.fn("Client.checkoutOf")(function*(from: string) {
  const fs = yield* FileSystem.FileSystem
  let dir = path.resolve(from)
  for (;;) {
    if (yield* fs.exists(path.join(dir, ".git")).pipe(Effect.orElseSucceed(() => false))) return dir
    const up = path.dirname(dir)
    if (up === dir) return path.resolve(from)
    dir = up
  }
})

/**
 * A test suite's own database name, distinct per checkout:
 * `<base>_<checkout name>_<hash of its path>`, the checkout being the directory
 * holding `.git` above `cwd`, reduced to what postgres takes unquoted and to
 * its 63 characters. Two worktrees running one suite at once build and drop
 * two databases, never each other's, even where their directories share a
 * name.
 */
export const scratchDatabase = Effect.fn("Client.scratchDatabase")(function*(base: string, cwd: string) {
  const checkout = yield* checkoutOf(cwd)
  const name = path.basename(checkout).toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "")
  const hash = crypto.createHash("sha256").update(checkout).digest("hex").slice(0, 8)
  const head = `${base}_${name}`.slice(0, 63 - hash.length - 1).replace(/_+$/, "")
  return `${head}_${hash}`
})

/** The SQLSTATE a statement failed with, as the `pg` driver states it; `undefined` where it states none. */
export const sqlState = (error: SqlError): string | undefined => {
  const cause = (error.reason as { readonly cause?: unknown }).cause
  const code = typeof cause === "object" && cause !== null ? (cause as { readonly code?: unknown }).code : undefined
  return typeof code === "string" ? code : undefined
}

/** Whether a drop was refused because a session still holds the database (SQLSTATE 55006). */
export const inUse = (error: SqlError): boolean => sqlState(error) === "55006"

/**
 * `database` gone. This role's own sessions on it are ended first, then the
 * database is dropped WITHOUT `FORCE`: a forced drop ends every backend
 * itself and is refused (42501) by any it may not end — an autovacuum worker
 * runs as the bootstrap superuser — while a plain drop signals autovacuum
 * workers to exit on its own. A drop refused as in use is retried for up to
 * five seconds, while an ended session goes away.
 */
export const dropDatabase = (database: string): Effect.Effect<void, SqlError, SqlClient> =>
  Effect.flatMap(SqlClient, (sql) =>
    Effect.gen(function*() {
      yield* sql`SELECT pg_terminate_backend(pid) FROM pg_stat_activity
        WHERE datname = ${database} AND pid <> pg_backend_pid() AND usename = current_user`
      yield* sql.unsafe(`DROP DATABASE IF EXISTS ${database}`)
    })).pipe(
      Effect.retry({ schedule: Schedule.spaced("250 millis"), times: 20, while: inUse })
    )
