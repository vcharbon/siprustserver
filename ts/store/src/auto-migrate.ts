/**
 * A store brought up to date unattended over its binding (`binding.ts`
 * `StoreBinding`: its migration sets and the migration applying them):
 * the database created where its server answers that it lacks it, then, where
 * a migration is pending and the database holds anything, a custom-format
 * `pg_dump` written to `<backups>/<UTC stamp>-before-<last pending>.dump`,
 * then the binding's migration. No migration runs without its backup: a dump
 * that fails stops the step, and only a finished dump carries the name.
 *
 * One step at a time per database: the pending read, the dump and the
 * migrations run under a session advisory lock, so a second step started
 * meanwhile waits, then finds nothing pending.
 */
import * as StoreBinding from "./binding.js"
import { atDatabase, layerFromUrl, sqlState } from "./client.js"
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import * as Stream from "effect/Stream"
import * as ChildProcess from "effect/unstable/process/ChildProcess"
import { ChildProcessSpawner } from "effect/unstable/process/ChildProcessSpawner"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as path from "node:path"
import * as SchemaState from "./schema-state.js"

/** Every migration a run applied, in order, as `[id, name]`. */
export type Applied = ReadonlyArray<readonly [id: number, name: string]>

/** The database names this step creates: what postgres takes as one identifier, quoted. */
const NAME = /^[A-Za-z0-9_]{1,63}$/

/** The server database a creation connects to, reached only once the target is answered missing. */
const MAINTENANCE = "postgres"

/** SQLSTATE of a connection to a database the server does not hold. */
const MISSING_DATABASE = "3D000"

/**
 * SQLSTATEs of a `CREATE DATABASE` another session got to first: the name
 * found taken (42P04), or the catalog's unique index hit by a creation
 * committing alongside (23505).
 */
const DUPLICATE_DATABASE: ReadonlySet<string> = new Set(["42P04", "23505"])

/** The advisory lock key one step holds on its database for its whole length. */
export const LOCK_KEY = 765_000_001

/** Why a step stopped before any migration ran. */
export class Refused extends Schema.TaggedError<Refused>()("AutoMigrate.Refused", {
  reason: Schema.String
}) {
  override get message(): string {
    return this.reason
  }
}

/** What one step did. */
export interface Outcome {
  readonly database: string
  readonly created: boolean
  /** The dump written before the migrations ran; undefined where none was needed. */
  readonly backup: string | undefined
  readonly applied: Applied
}

/** The database `url` names, or why this step will not create it. */
export const databaseOf = (url: string): Effect.Effect<string, Refused> => {
  const parsed = URL.parse(url)
  const name = parsed === null ? "" : decodeURIComponent(parsed.pathname.replace(/^\//, ""))
  return NAME.test(name)
    ? Effect.succeed(name)
    : Effect.fail(new Refused({ reason: `the store url names no database this step creates: '${name}'` }))
}

/** Whether `error` is one of the states `codes`. */
const isState = (codes: string | ReadonlySet<string>) => (error: SqlError): boolean => {
  const state = sqlState(error)
  return state !== undefined && (typeof codes === "string" ? state === codes : codes.has(state))
}

/**
 * `database` on the server of `url`, created where a connection to it is
 * answered missing (another session creating it meanwhile is no failure);
 * answers whether this step created it.
 */
export const ensureDatabase = (url: string, database: string) =>
  Effect.provide(Effect.succeed(false), layerFromUrl(url)).pipe(
    Effect.catchIf(isState(MISSING_DATABASE), () =>
      Effect.flatMap(SqlClient, (sql) => sql.unsafe(`CREATE DATABASE "${database}"`)).pipe(
        Effect.as(true),
        Effect.catchIf(isState(DUPLICATE_DATABASE), () => Effect.succeed(false)),
        Effect.provide(layerFromUrl(atDatabase(url, MAINTENANCE)))
      ))
  )

/** `effect` under this step's advisory lock on the client's database, held by one reserved session. */
export const exclusively = <A, E, R>(effect: Effect.Effect<A, E, R>) =>
  Effect.scoped(Effect.gen(function*() {
    const sql = yield* SqlClient
    const session = yield* sql.reserve
    yield* Effect.acquireRelease(
      session.executeRaw("SELECT pg_advisory_lock($1)", [LOCK_KEY]),
      () => Effect.ignore(session.executeRaw("SELECT pg_advisory_unlock($1)", [LOCK_KEY]))
    )
    return yield* effect
  }))

/** `millis` as a compact UTC stamp: `20261101T103000Z`. */
export const stampOf = (millis: number): string =>
  new Date(millis).toISOString().replace(/\.\d{3}Z$/, "Z").replace(/[-:]/g, "")

/** `url` without its password, and the password, which the child reads from `PGPASSWORD`, never its argv. */
export const credentialsOf = (url: string): { readonly url: string; readonly password: string | undefined } => {
  const parsed = URL.parse(url)
  if (parsed === null || parsed.password === "") return { url, password: undefined }
  const password = decodeURIComponent(parsed.password)
  parsed.password = ""
  return { url: parsed.toString(), password }
}

/**
 * A custom-format dump of the database `url` names, written to `file`
 * through a partial file of this process's own, renamed on success.
 */
export const dump = (url: string, file: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const spawner = yield* ChildProcessSpawner
    const partial = `${file}.${process.pid}.partial`
    const credentials = credentialsOf(url)
    yield* fs.makeDirectory(path.dirname(file), { recursive: true })
    const ran = yield* Effect.scoped(Effect.gen(function*() {
      const handle = yield* spawner.spawn(
        ChildProcess.make("pg_dump", ["--format=custom", `--file=${partial}`, `--dbname=${credentials.url}`], {
          stdin: "ignore",
          extendEnv: true,
          ...(credentials.password === undefined ? {} : { env: { PGPASSWORD: credentials.password } })
        })
      )
      const [stderr, code] = yield* Effect.all([
        Stream.mkString(Stream.decodeText(handle.stderr)),
        handle.exitCode,
        Stream.runDrain(handle.stdout)
      ], { concurrency: "unbounded" })
      return { stderr, code: Number(code) }
    })).pipe(
      Effect.mapError((error) => new Refused({ reason: `pg_dump did not run (is it on PATH?): ${String(error)}` }))
    )
    if (ran.code !== 0) {
      yield* fs.remove(partial, { force: true }).pipe(Effect.ignore)
      return yield* Effect.fail(
        new Refused({ reason: `pg_dump exited ${ran.code}, nothing migrated: ${ran.stderr.trim().slice(-2000)}` })
      )
    }
    yield* fs.rename(partial, file)
    return file
  })

/**
 * The store `url` names brought up to date by its binding's migration, its
 * backup under `backups` first where one is needed; `written` is told the
 * dump's path the moment it is finished, before any migration runs.
 */
export const run = (
  url: string,
  backups: string,
  written: (file: string) => Effect.Effect<void> = () => Effect.void
) =>
  Effect.gen(function*() {
    const binding = yield* StoreBinding.Service
    const database = yield* databaseOf(url)
    const created = yield* ensureDatabase(url, database)
    return yield* exclusively(Effect.gen(function*() {
      const pending = yield* SchemaState.pendingOf(binding.sets)
      if (pending.length === 0) return { database, created, backup: undefined, applied: [] } satisfies Outcome
      const last = SchemaState.keyOf(pending[pending.length - 1]!)
      const backup = (yield* SchemaState.isEmpty)
        ? undefined
        : yield* dump(url, path.join(backups, `${stampOf(yield* Clock.currentTimeMillis)}-before-${last}.dump`))
      if (backup !== undefined) yield* written(backup)
      const applied = yield* binding.migrate
      return { database, created, backup, applied } satisfies Outcome
    })).pipe(Effect.provide(layerFromUrl(url)))
  })
