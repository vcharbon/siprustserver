/**
 * The TAKE MARKER: what an inbox take states of the capture it landed in a
 * workspace's pcap root, written beside its first file as
 * `.<file>.take.json`. It names the level the capture was taken at, the
 * digest of that file as landed, and, for an anonymised level, the flows
 * document the take anonymised from the raw read (`.<file>.flows.json`
 * beside it) and the seed it ran under. The ingest reads the marker, never
 * the file name, to know that a capture was taken already: it then takes that
 * document as the capture's own, or for `none` the capture as read, and
 * anonymises nothing again. A dotfile, so no listing of the pcap root and no
 * capture discovery takes it for a capture.
 */
import { AnonymiseLevel } from "@sip/workspace"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as crypto from "node:crypto"
import * as nodeFs from "node:fs"
import * as path from "node:path"

/** A marker or the file it speaks for does not read. */
export class Unreadable extends Schema.TaggedError<Unreadable>()("InboxMarker.Unreadable", { reason: Schema.String }) {
  override get message(): string {
    return this.reason
  }
}

/** The marker as written: its paths relative to the pcap root. */
const Written = Schema.Struct({
  level: AnonymiseLevel.Level,
  /** The SHA-256 of the file the marker stands beside, as landed. */
  sha256: Schema.String,
  /** The anonymised flows document; absent for `none`. */
  document: Schema.optionalKey(Schema.String),
  /** The seed the anonymisation ran under; absent for `none`. */
  seed: Schema.optionalKey(Schema.String)
})
const decode = Schema.decodeUnknownResult(Written)

/** A marker as read: its paths absolute. */
export interface Marker {
  readonly level: AnonymiseLevel.Level
  readonly sha256: string
  readonly document?: string
  readonly seed?: string
}

/** The marker file of the landed capture file `file`. */
export const markerOf = (file: string): string => path.join(path.dirname(file), `.${path.basename(file)}.take.json`)

/** Where the take keeps the anonymised flows document of the landed capture file `file`. */
export const documentOf = (file: string): string => path.join(path.dirname(file), `.${path.basename(file)}.flows.json`)

/** The SHA-256 of a file, streamed: a capture may be hundreds of megabytes. */
export const digest = (file: string) =>
  Effect.callback<string, Unreadable>((resume) => {
    const hash = crypto.createHash("sha256")
    const stream = nodeFs.createReadStream(file)
    stream.on("data", (chunk) => hash.update(chunk))
    stream.on("error", (error) => resume(Effect.fail(new Unreadable({ reason: `${file} does not read: ${error.message}` }))))
    stream.on("end", () => resume(Effect.succeed(hash.digest("hex"))))
    return Effect.sync(() => stream.destroy())
  })

/** The marker of `file` written, its paths stored relative to the file's directory. */
export const write = Effect.fn("InboxMarker.write")(function*(file: string, marker: Marker) {
  const fs = yield* FileSystem.FileSystem
  const dir = path.dirname(file)
  const relative = (to: string) => path.relative(dir, to)
  const written = {
    level: marker.level,
    sha256: marker.sha256,
    ...(marker.document === undefined ? {} : { document: relative(marker.document) }),
    ...(marker.seed === undefined ? {} : { seed: relative(marker.seed) })
  }
  yield* fs.writeFileString(markerOf(file), `${JSON.stringify(written, null, 2)}\n`)
})

/**
 * The marker `file` stands beside, its paths absolute; `undefined` where it
 * stands beside none. A marker that does not decode, or whose digest is not
 * the file's, is refused: the file is no longer the one its take landed.
 */
export const read = Effect.fn("InboxMarker.read")(function*(file: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(markerOf(file)).pipe(Effect.option)
  if (text._tag === "None") return undefined
  const parsed = Result.try(() => JSON.parse(text.value) as unknown)
  const decoded = Result.isSuccess(parsed) ? decode(parsed.success) : undefined
  if (decoded === undefined || Result.isFailure(decoded)) {
    return yield* new Unreadable({ reason: `${markerOf(file)} is no take marker` })
  }
  const written = decoded.success
  if ((yield* digest(file)) !== written.sha256) {
    return yield* new Unreadable({
      reason: `${path.basename(file)} changed since its inbox take landed it: drop the capture into the inbox again`
    })
  }
  const dir = path.dirname(file)
  return {
    level: written.level,
    sha256: written.sha256,
    ...(written.document === undefined ? {} : { document: path.resolve(dir, written.document) }),
    ...(written.seed === undefined ? {} : { seed: path.resolve(dir, written.seed) })
  } satisfies Marker
})
