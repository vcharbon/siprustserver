/**
 * The ATTACHMENTS of a cell, a batch and a job (`@sip/contracts`
 * `Attachment`, ADR-0060 appendix §9), as the store holds them: a cell's and
 * a batch's in `attachments`, a job's in `job_attachments` (so the result
 * reset, which empties the first, keeps a job's), one per name per holder, in
 * the order listed, each beside its holder's directory so it reads back as a
 * path on this box.
 *
 * A writer lists what it attaches in its holder's directory ({@link attach});
 * that list is what the store reads ({@link listed}). A cell's attachments go
 * with its case result (`runs.ts` writes them beside it), replacing what it
 * held; a batch's and a job's, and a cell's attached after its result landed,
 * are added by name ({@link Interface.add}).
 */
import { Attachment } from "@sip/contracts"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as path from "node:path"
import * as Sessions from "./sessions.js"
import type { Holder } from "./verifications.js"

export type { Holder } from "./verifications.js"

/** What a holder attached: its directory, absolute, and the attachments listed there. */
export interface Held {
  readonly dir: string
  readonly attachments: ReadonlyArray<Attachment.Attachment>
}

/**
 * One attachment as the store holds it: its id, its holder, and where its file
 * is on this box (none for a `link`). The path is joined lexically under the
 * holder's directory; a reader that serves it resolves links itself.
 */
export interface Stored extends Attachment.Attachment {
  readonly id: number
  readonly holder: Holder
  readonly path: string | undefined
}

/** An `attachment_rows` row as a select of its columns reads it. */
interface Row {
  readonly id: string
  readonly scope: Holder["scope"]
  readonly run_id: string | null
  readonly case_id: string | null
  readonly batch_id: string | null
  readonly job_id: string | null
  readonly name: string
  readonly kind: Attachment.Kind
  readonly title: string | null
  readonly dir: string
  readonly file: string
  readonly source: string
}

/** `attachment` appended to the list of the holder whose directory is `dir`, the directory made where it is not. */
export const attach = Effect.fn("Attachments.attach")(function*(dir: string, attachment: Attachment.Attachment) {
  const fs = yield* FileSystem.FileSystem
  yield* fs.makeDirectory(dir, { recursive: true })
  yield* fs.writeFileString(path.join(dir, Attachment.FILE), Attachment.line(attachment), { flag: "a" })
})

/** What the holder whose directory is `dir` attached; nothing where it lists none or its list cannot be read. */
export const listed = (dir: string): Effect.Effect<Held, never, FileSystem.FileSystem> =>
  Effect.flatMap(FileSystem.FileSystem, (fs) => fs.readFileString(path.join(dir, Attachment.FILE))).pipe(
    Effect.map((text) => ({ dir, attachments: Attachment.parse(text) })),
    Effect.orElseSucceed(() => ({ dir, attachments: [] }))
  )

/** The key columns of `holder`'s rows in its table. */
const keyOf = (holder: Holder) =>
  holder.scope === "cell"
    ? { scope: "cell", run_id: holder.runId, case_id: holder.caseId, batch_id: null }
    : holder.scope === "batch"
    ? { scope: "batch", run_id: null, case_id: null, batch_id: holder.batchId }
    : { job_id: holder.jobId }

/** Where `holder`'s rows live: its table, the condition naming them, and the unique index an insert conflicts on. */
const placeOf = (sql: SqlClient, holder: Holder) =>
  holder.scope === "cell"
    ? {
      table: sql("attachments"),
      where: sql`scope = 'cell' AND run_id = ${holder.runId} AND case_id = ${holder.caseId}`,
      conflict: sql`(run_id, case_id, name) WHERE scope = 'cell'`
    }
    : holder.scope === "batch"
    ? {
      table: sql("attachments"),
      where: sql`scope = 'batch' AND batch_id = ${holder.batchId}`,
      conflict: sql`(batch_id, name) WHERE scope = 'batch'`
    }
    : { table: sql("job_attachments"), where: sql`job_id = ${holder.jobId}`, conflict: sql`(job_id, name)` }

const holderOf = (row: Row): Holder =>
  row.scope === "cell"
    ? { scope: "cell", runId: Number(row.run_id), caseId: row.case_id! }
    : row.scope === "batch"
    ? { scope: "batch", batchId: Number(row.batch_id) }
    : { scope: "job", jobId: Number(row.job_id) }

const storedOf = (root: Sessions.Root, row: Row): Stored => ({
  id: Number(row.id),
  holder: holderOf(row),
  name: row.name,
  kind: row.kind,
  ...(row.title === null ? {} : { title: row.title }),
  file: row.file,
  source: row.source,
  path: row.kind === "link" ? undefined : path.join(Sessions.fromStored(root, row.dir), row.file)
})

const rowsOf = (root: Sessions.Root, holder: Holder, held: Held, from: number) => {
  const files = held.attachments.filter(Attachment.isHeld)
  const dir = files.some((attachment) => attachment.kind !== "link") ? Sessions.toStored(root, held.dir) : ""
  return files.map((attachment, at) => ({
    ...keyOf(holder),
    ordinal: from + at,
    name: attachment.name,
    kind: attachment.kind,
    title: attachment.title ?? null,
    dir: attachment.kind === "link" ? "" : dir,
    file: attachment.file,
    source: attachment.source
  }))
}

/**
 * Replaces every attachment `holder` holds with `held`'s, in their order. Runs
 * in the caller's transaction, so a case result and its attachments land
 * together.
 */
export const replace = Effect.fn("Attachments.replace")(function*(holder: Holder, held: Held) {
  const sql = yield* SqlClient
  const root = yield* Sessions.Service
  const place = placeOf(sql, holder)
  yield* sql`DELETE FROM ${place.table} WHERE ${place.where}`
  const rows = rowsOf(root, holder, held, 0)
  if (rows.length > 0) yield* sql`INSERT INTO ${place.table} ${sql.insert(rows)}`
})

/**
 * Adds `held`'s attachments to what `holder` holds: one of a name it already
 * holds replaces it where it stands, a new name comes after the others. Runs
 * in the caller's transaction.
 */
export const addTo = Effect.fn("Attachments.addTo")(function*(holder: Holder, held: Held) {
  const sql = yield* SqlClient
  const root = yield* Sessions.Service
  const place = placeOf(sql, holder)
  const [next] = yield* sql<{ next: number }>`
    SELECT coalesce(max(ordinal) + 1, 0)::int AS next FROM ${place.table} WHERE ${place.where}
  `
  for (const row of rowsOf(root, holder, held, next?.next ?? 0)) {
    yield* sql`
      INSERT INTO ${place.table} ${sql.insert(row)}
      ON CONFLICT ${place.conflict} DO UPDATE SET
        kind = EXCLUDED.kind, title = EXCLUDED.title, dir = EXCLUDED.dir,
        file = EXCLUDED.file, source = EXCLUDED.source
    `
  }
})

export interface Interface {
  /** {@link addTo} in a transaction of its own. */
  readonly add: (holder: Holder, held: Held) => Effect.Effect<void, SqlError>
  /** What `holder` holds, in its order. */
  readonly of: (holder: Holder) => Effect.Effect<ReadonlyArray<Stored>, SqlError>
  /** The attachment `id`; undefined where there is none. */
  readonly byId: (id: number) => Effect.Effect<Stored | undefined, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Attachments") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    const add = (holder: Holder, held: Held) =>
      sql.withTransaction(addTo(holder, held)).pipe(
        Effect.provideService(SqlClient, sql),
        Effect.provideService(Sessions.Service, root)
      )

    const of = Effect.fn("Attachments.of")(function*(holder: Holder) {
      const rows = yield* sql<Row>`SELECT * FROM attachment_rows WHERE ${placeOf(sql, holder).where} ORDER BY ordinal`
      return rows.map((row) => storedOf(root, row))
    })

    const byId = Effect.fn("Attachments.byId")(function*(id: number) {
      const [row] = yield* sql<Row>`SELECT * FROM attachment_rows WHERE id = ${id}`
      return row === undefined ? undefined : storedOf(root, row)
    })

    return Service.of({ add, of, byId })
  })
)
