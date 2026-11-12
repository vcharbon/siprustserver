/**
 * An attachment's file, whole, streamed off disk:
 * `GET /api/attachments/<id>/file` — what a page's download and a bounded
 * log's "whole log" link to. A `file` is offered for download under its own
 * name, every other kind is served as text or JSON to read in place, from
 * where its links resolve. An id the store holds no file of, or whose file
 * resolves outside the session tree (`attachments.ts`), is a 404.
 */
import { Effect, Layer } from "effect"
import { FileSystem } from "effect/FileSystem"
import { HttpRouter, HttpServerResponse } from "effect/unstable/http"
import * as path from "node:path"
import { AttachmentReads } from "./attachments.ts"

const notFound = HttpServerResponse.text("no such attachment file", { status: 404 })

const TYPES = {
  log: "text/plain; charset=utf-8",
  records: "text/plain; charset=utf-8",
  table: "text/plain; charset=utf-8",
  diff: "application/json",
  json: "application/json",
  file: "application/octet-stream",
  link: "text/plain; charset=utf-8"
} as const

export const layer = Layer.unwrap(
  Effect.gen(function*() {
    const reads = yield* AttachmentReads.Service
    const fs = yield* FileSystem
    const serve = Effect.gen(function*() {
      const { id } = yield* HttpRouter.params
      if (id === undefined || !/^\d+$/.test(id)) return notFound
      const held = yield* reads.stored(Number(id)).pipe(Effect.option)
      if (held._tag === "None") return notFound
      const one = held.value
      const served = yield* HttpServerResponse.file(one.path, { contentType: TYPES[one.kind] }).pipe(
        Effect.provideService(FileSystem, fs),
        Effect.option
      )
      if (served._tag === "None") return notFound
      return one.kind === "file"
        ? HttpServerResponse.setHeader(served.value, "content-disposition", `attachment; filename="${path.basename(one.file).replace(/["\\\r\n]/g, "_")}"`)
        : served.value
    })
    return HttpRouter.add("GET", "/api/attachments/:id/file", serve)
  })
)

export * as AttachmentFile from "./attachment-file.ts"
