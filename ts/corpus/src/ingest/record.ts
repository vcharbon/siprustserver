/**
 * What the artifact store records about one extraction (`ingest.json` in the
 * capture's directory): which pcaps, by path, size, mtime and content hash,
 * which extractor, when, under which kept seed, and which generation of the
 * name it landed under.
 *
 * The record is what says whether a capture must be extracted again, and it
 * is written last: an extraction that dies short of writing its own reads as
 * never extracted, not as the artifacts it was replacing.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as Schema from "effect/Schema"
import * as Stream from "effect/Stream"
import * as crypto from "node:crypto"
import * as path from "node:path"
import { Extraction } from "../deployment/index.js"
import type { Source } from "./discover.js"

/** The record's file name, in the capture's directory. */
export const INGEST = "ingest.json"

const PcapRecord = Schema.Struct({
  path: Schema.String,
  size: Schema.Number,
  mtime: Schema.String,
  sha256: Schema.String
})
export interface PcapRecord extends Schema.Schema.Type<typeof PcapRecord> {}

export const Ingested = Schema.Struct({
  capture: Schema.String,
  /** The set's files, in the order they were read. */
  pcaps: Schema.Array(PcapRecord),
  /** The extractor that produced these artifacts (`fingerprint.ts`). */
  extractor: Schema.String,
  ingestedAt: Schema.String,
  /** Where the anonymisation's seed is kept: a path, never the seed. */
  seedFile: Schema.optional(Schema.String),
  /** The generation of the capture name (`generation.ts`), and the one it follows. */
  generation: Schema.optional(Schema.Number),
  follows: Schema.optional(Schema.String)
})
export interface Ingested extends Schema.Schema.Type<typeof Ingested> {}

const decodeIngested = Schema.decodeUnknownSync(Ingested)

/** Where one capture's artifacts live. */
export const captureDir = (artifacts: string, capture: string): string => path.join(artifacts, capture)

/** The sha-256 of `file`, read as a stream: a pcap is gigabytes. */
export const sha256 = Effect.fn("IngestRecord.sha256")(function*(file: string) {
  const fs = yield* FileSystem.FileSystem
  const hash = crypto.createHash("sha256")
  yield* fs.stream(file).pipe(Stream.runForEach((chunk) => Effect.sync(() => void hash.update(chunk))))
  return hash.digest("hex")
})

/** The record at `file`, or `null` where none reads. */
const readAt = (fs: FileSystem.FileSystem, file: string) =>
  fs.readFileString(file).pipe(
    Effect.flatMap((text) => Effect.try(() => decodeIngested(JSON.parse(text) as unknown))),
    Effect.orElseSucceed(() => null)
  )

/** The capture's ingest record as the artifact store holds it, or `null` where it holds none it can read. */
export const read = Effect.fn("IngestRecord.read")(function*(artifacts: string, capture: string) {
  const fs = yield* FileSystem.FileSystem
  return yield* readAt(fs, path.join(captureDir(artifacts, capture), INGEST))
})

/** When the artifact store last extracted this capture, or `null`. */
export const ingestedAt = Effect.fn("IngestRecord.ingestedAt")(function*(artifacts: string, capture: string) {
  return (yield* read(artifacts, capture))?.ingestedAt ?? null
})

/** Whether this file is the one the record read, by size and mtime, then by bytes. */
const unchanged = (fs: FileSystem.FileSystem, file: string, record: PcapRecord) =>
  Effect.gen(function*() {
    const info = yield* fs.stat(file)
    const mtime = Option.getOrUndefined(Option.map(info.mtime, (at) => at.toISOString()))
    if (record.size === Number(info.size) && record.mtime === mtime) return true
    return (yield* sha256(file)) === record.sha256
  })

/**
 * Why this capture has to be extracted again, or `undefined` where its
 * artifacts are current: the extractor, the list of files, then each file.
 *
 * Size and mtime are asked first and a pcap is hashed only when they moved:
 * a campaign over ten thousand captures must not read every pcap to find out
 * that nothing changed. A touched-but-identical file therefore costs one hash
 * and stays current — the bytes decide, the timestamp only decides whether to
 * look. A file that can no longer be read is a pcap that changed.
 */
export const staleness = Effect.fn("IngestRecord.staleness")(function*(
  artifacts: string,
  source: Source,
  extractor: string
) {
  const fs = yield* FileSystem.FileSystem
  const record = yield* read(artifacts, source.capture)
  if (record === null) return "never extracted"
  if (record.extractor !== extractor) return "the extractor changed"
  if (record.pcaps.length !== source.pcaps.length || record.pcaps.some((one, i) => one.path !== source.pcaps[i])) {
    return "the set of files changed"
  }
  for (const [i, file] of source.pcaps.entries()) {
    const same = yield* unchanged(fs, file, record.pcaps[i]!).pipe(Effect.orElseSucceed(() => false))
    if (!same) return "the pcap changed"
  }
  return undefined
})

/** The record of an extraction of `source`'s files, as written now. */
export const recordOf = Effect.fn("IngestRecord.recordOf")(function*(
  source: Source,
  extractor: string,
  landing: { readonly seedFile: string | undefined; readonly generation: number; readonly follows: string | undefined }
) {
  const fs = yield* FileSystem.FileSystem
  const pcaps: Array<PcapRecord> = []
  for (const file of source.pcaps) {
    const info = yield* fs.stat(file)
    pcaps.push({
      path: file,
      size: Number(info.size),
      mtime: Option.getOrElse(info.mtime, () => new Date(0)).toISOString(),
      sha256: yield* sha256(file)
    })
  }
  return {
    capture: source.capture,
    pcaps,
    extractor,
    ingestedAt: new Date().toISOString(),
    ...(landing.seedFile === undefined ? {} : { seedFile: landing.seedFile }),
    generation: landing.generation,
    ...(landing.follows === undefined ? {} : { follows: landing.follows })
  } satisfies Ingested
})

/** `record` written as the capture's ingest record. */
export const write = Effect.fn("IngestRecord.write")(function*(artifacts: string, record: Ingested) {
  const fs = yield* FileSystem.FileSystem
  yield* fs.writeFileString(
    path.join(captureDir(artifacts, record.capture), INGEST),
    `${JSON.stringify(record, null, 2)}\n`
  )
})

/**
 * The failure the ingest recorded for this capture, or `undefined` where it
 * recorded none — an extraction that succeeded, or a capture never ingested.
 * The size is the ingest record's, every file of the set summed, where one
 * was written.
 */
export const failureOf = Effect.fn("IngestRecord.failureOf")(function*(artifacts: string, capture: string) {
  const extraction = yield* Extraction.Service
  const error = yield* extraction.failureOf(captureDir(artifacts, capture), capture)
  if (error === undefined) return undefined
  const size = (yield* read(artifacts, capture))?.pcaps.reduce((sum, one) => sum + one.size, 0)
  return { capture, error, ...(size === undefined ? {} : { size }) } satisfies Extraction.Failure
})
