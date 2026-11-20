/**
 * The platform whose file response opens its file only when the server reads
 * the body, and closes it when the body ends or the client leaves. A response
 * built and never sent (`HttpStaticServer` reads a full response's validators
 * to answer a 304) holds no descriptor. A read that fails resets the connection.
 */
import { Effect, Layer, Option } from "effect"
import { FileSystem } from "effect/FileSystem"
import { Etag, Headers, HttpPlatform, HttpServerResponse } from "effect/unstable/http"
import { createReadStream } from "node:fs"
import { Readable } from "node:stream"

const fileBody = (path: string, start: number, length: number): Readable =>
  Readable.from(
    (async function*() {
      if (length > 0) yield* createReadStream(path, { start, end: start + length - 1 })
    })(),
    { objectMode: false }
  )

export const LazyFilePlatform = Layer.effect(
  HttpPlatform.HttpPlatform,
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const etags = yield* Etag.Generator
    const platform = yield* HttpPlatform.HttpPlatform
    return HttpPlatform.HttpPlatform.of({
      ...platform,
      fileResponse: (path, options) =>
        Effect.gen(function*() {
          const info = yield* fs.stat(path)
          const etag = yield* etags.fromFileInfo(info)
          const start = Number(options?.offset ?? 0)
          const length = options?.bytesToRead === undefined ? Number(info.size) - start : Number(options.bytesToRead)
          const validators = Headers.set(
            options?.headers === undefined ? Headers.empty : Headers.fromInput(options.headers),
            "etag",
            Etag.toString(etag)
          )
          return HttpServerResponse.raw(fileBody(path, start, length), {
            status: options?.status ?? 200,
            statusText: options?.statusText,
            headers: {
              ...validators,
              "content-length": length.toString(),
              ...Option.match(info.mtime, {
                onNone: () => ({}),
                onSome: (mtime) => ({ "last-modified": mtime.toUTCString() })
              })
            }
          })
        })
    })
  })
).pipe(Layer.provide(Etag.layer))
