/**
 * The attachments of a cell, a batch and a job as the pages read them
 * (`@sip/store` `Attachments`, ADR-0060 appendix §9): a holder's list, and one
 * attachment read for its kind's component. Every attachment reads by its
 * kind; none is named here.
 *
 * A `log` reads bounded, its two ends where it is long (`log-text.ts`); a
 * `records`, `table`, `diff` or `json` file reads whole up to
 * {@link MAX_VIEW_BYTES}, past which it is downloaded, never shown; a `file`
 * states its size and a `link` its URL. A file is read where its links
 * resolve, and only inside the session tree. An id the store holds none of, a
 * file gone from its directory, one that resolves outside the session tree,
 * and a file that does not read as its kind are a 404 saying so.
 */
import { Attachment } from "@sip/contracts"
import { Attachments, Sessions } from "@sip/store"
import { Effect, Layer } from "effect"
import * as Context from "effect/Context"
import { FileSystem } from "effect/FileSystem"
import * as path from "node:path"
import type { AttachmentListed, AttachmentView } from "./api.ts"
import { NotFound } from "./catalog.ts"
import * as LogText from "./log-text.ts"

/** The largest file a page reads as a records, table, diff or JSON view. */
export const MAX_VIEW_BYTES = 8 * 1024 * 1024

export type Listed = typeof AttachmentListed.Type
export type View = typeof AttachmentView.Type

/** The route an attachment's file is served at, as it is. */
export const fileHref = (id: number): string => `/api/attachments/${id}/file`

export interface Interface {
  /** What `holder` holds, in its order. */
  readonly list: (holder: Attachments.Holder) => Effect.Effect<ReadonlyArray<Listed>>
  /** The attachment `id` read for its kind's component. */
  readonly view: (id: number) => Effect.Effect<View, NotFound>
  /** The attachment `id`, where its file is with its links resolved, and its size; a `link` has no file. */
  readonly stored: (id: number) => Effect.Effect<Attachments.Stored & { readonly path: string; readonly bytes: number }, NotFound>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/AttachmentReads") {}

const listedOf = (stored: Attachments.Stored): Listed => ({
  id: stored.id,
  name: stored.name,
  kind: stored.kind,
  title: stored.title ?? stored.name,
  source: stored.source
})

/** A file's `text` read as `kind`, or why it does not read as one. */
export const viewOf = (kind: Exclude<Attachment.Kind, "log" | "file" | "link">, text: string): View | string => {
  if (kind === "records") return { kind, records: Attachment.recordsOf(text) }
  if (kind === "table") return { kind, ...Attachment.tableOf(text) }
  let parsed: unknown
  try {
    parsed = JSON.parse(text)
  } catch (e) {
    return `not JSON: ${e instanceof Error ? e.message : String(e)}`
  }
  if (kind === "json") return { kind, value: parsed }
  const diff = Attachment.diffOf(parsed)
  return diff === undefined ? "not a diff document" : { kind, diff }
}

/** Whether `file` lies under `dir`, both absolute and with their links resolved. */
const isWithin = (dir: string, file: string): boolean => {
  const relative = path.relative(dir, file)
  return relative !== "" && !path.isAbsolute(relative) && relative.split(path.sep)[0] !== ".."
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const store = yield* Attachments.Service
    const sessions = yield* Sessions.Service
    const fs = yield* FileSystem

    const list = (holder: Attachments.Holder) =>
      store.of(holder).pipe(Effect.map((all) => all.map(listedOf)), Effect.orDie)

    const byId = (id: number) =>
      Effect.gen(function*() {
        const stored = yield* store.byId(id).pipe(Effect.orDie)
        if (stored === undefined) return yield* new NotFound({ id: String(id), note: "no such attachment" })
        return stored
      })

    const stored = (id: number) =>
      Effect.gen(function*() {
        const one = yield* byId(id)
        if (one.path === undefined) return yield* new NotFound({ id: String(id), note: `a ${one.kind} has no file` })
        const real = yield* fs.realPath(one.path).pipe(Effect.option)
        const tree = yield* fs.realPath(sessions.dir).pipe(Effect.orElseSucceed(() => sessions.dir))
        if (real._tag === "None") return yield* new NotFound({ id: String(id), note: `${one.file} is not in its directory` })
        if (!isWithin(tree, real.value)) {
          return yield* new NotFound({ id: String(id), note: `${one.file} resolves outside the session tree` })
        }
        const size = yield* fs.stat(real.value).pipe(Effect.map((info) => (info.type === "File" ? info.size : undefined)), Effect.orElseSucceed(() => undefined))
        if (size === undefined) return yield* new NotFound({ id: String(id), note: `${one.file} is not in its directory` })
        return { ...one, path: real.value, bytes: Number(size) }
      })

    const view = (id: number) =>
      Effect.gen(function*() {
        const one = yield* byId(id)
        if (one.kind === "link") return { kind: "link", url: one.file } satisfies View
        const held = yield* stored(id)
        const bytes = held.bytes
        if (one.kind === "file") return { kind: "file", bytes } satisfies View
        if (one.kind === "log") {
          const read = yield* LogText.bounded(held.path, fileHref(id))
          if (read === undefined) return yield* new NotFound({ id: String(id), note: `${one.file} does not read` })
          return { kind: "log", text: read.text, bytes: read.bytes } satisfies View
        }
        if (bytes > MAX_VIEW_BYTES) {
          return yield* new NotFound({ id: String(id), note: `${one.file} is ${bytes} bytes, past ${MAX_VIEW_BYTES}: download it` })
        }
        const text = yield* fs.readFileString(held.path).pipe(Effect.orDie)
        const read = viewOf(one.kind, text)
        if (typeof read === "string") return yield* new NotFound({ id: String(id), note: `${one.file}: ${read}` })
        return read
      })

    return Service.of({ list, view, stored })
  })
)

export * as AttachmentReads from "./attachments.ts"
