/**
 * The finished cell's files as the bless reads them: its confrontation sheet,
 * its RFC audit, its document's step order and its per-leg recordings. A file
 * that is not there is an absence. One that will not read or decode is the
 * writer's defect, logged and read as absent: no reading is taken off a file
 * nobody could read.
 */
import { Bundle, Confrontation, StepOrder } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import type { PlatformError } from "effect/PlatformError"
import * as Schema from "effect/Schema"
import * as path from "node:path"

const CONFRONTATION_FILE = "confrontation.ndjson"
const PIVOT_FILE = "pivot.json"
const RFC_FILE = "rfc.json"
const RECORDING_DIR = "recording"

/** A cell file that is not there. */
class CellFileMissing extends Schema.TaggedError<CellFileMissing>()("Bless.CellFileMissing", {
  file: Schema.String
}) {}

/** A cell file the platform will not hand over, or whose content does not decode. */
class CellFileUnreadable extends Schema.TaggedError<CellFileUnreadable>()("Bless.CellFileUnreadable", {
  file: Schema.String,
  cause: Schema.Defect()
}) {}

const readFailure = (file: string) => (error: PlatformError): CellFileMissing | CellFileUnreadable =>
  error.reason._tag === "NotFound" ? new CellFileMissing({ file }) : new CellFileUnreadable({ file, cause: error })

/** `decode` over the file's text, a throw being the file's unreadability. */
const readDecoded = <A>(
  file: string,
  decode: (text: string) => A
): Effect.Effect<A, CellFileMissing | CellFileUnreadable, FileSystem.FileSystem> =>
  Effect.gen(function* () {
    const fs = yield* FileSystem.FileSystem
    const text = yield* fs.readFileString(file).pipe(Effect.mapError(readFailure(file)))
    return yield* Effect.try({ try: () => decode(text), catch: (cause) => new CellFileUnreadable({ file, cause }) })
  })

/** `read` as an option: absent where the file is not there, logged under `what` and absent where it will not read. */
const orAbsent = <A, R>(what: string) => (
  read: Effect.Effect<A, CellFileMissing | CellFileUnreadable, R>
): Effect.Effect<Option.Option<A>, never, R> =>
  read.pipe(
    Effect.map(Option.some),
    Effect.catchTag("Bless.CellFileMissing", () => Effect.succeed(Option.none<A>())),
    Effect.catch((error) => Effect.logWarning(`${what} not read`, error).pipe(Effect.as(Option.none<A>())))
  )

/** The cell's confrontation records; `None` where the cell has no readable sheet. */
export const recordsOf = (
  dir: string
): Effect.Effect<Option.Option<ReadonlyArray<Confrontation.ConfrontationRecord>>, never, FileSystem.FileSystem> =>
  readDecoded(path.join(dir, CONFRONTATION_FILE), Confrontation.parseConfrontationLines).pipe(
    orAbsent("confrontation sheet")
  )

/** The cell's RFC audit, decoded; `None` where the cell carries no readable audit. */
export const rfcAuditOf = (dir: string): Effect.Effect<Option.Option<Bundle.RunRfcAudit>, never, FileSystem.FileSystem> =>
  readDecoded(path.join(dir, RFC_FILE), (text) => Bundle.decodeRunRfcAuditSync(JSON.parse(text))).pipe(
    orAbsent("rfc audit")
  )

/** The cell's document step ids in flow order; none where the cell carries no readable document. */
export const stepsOf = (dir: string): Effect.Effect<ReadonlyArray<string>, never, FileSystem.FileSystem> =>
  readDecoded(path.join(dir, PIVOT_FILE), (text) => StepOrder.stepOrderOf(JSON.parse(text) as unknown)).pipe(
    orAbsent("document"),
    Effect.map(Option.getOrElse((): ReadonlyArray<string> => []))
  )

/** One leg's recorded messages, the leg being the file name. */
const legOf = (file: string) =>
  readDecoded(file, (text) =>
    text
      .split("\n")
      .filter((line) => line.trim() !== "")
      .map((line) => Bundle.decodeRecordedMessageSync(JSON.parse(line)))
  )

/** The recording directory's leg files; `None` where it is not there or will not list. */
const legFilesOf = (at: string): Effect.Effect<Option.Option<ReadonlyArray<string>>, never, FileSystem.FileSystem> =>
  Effect.gen(function* () {
    const fs = yield* FileSystem.FileSystem
    return yield* fs.readDirectory(at).pipe(Effect.mapError(readFailure(at)))
  }).pipe(orAbsent("recording directory"))

/** The cell's per-leg recordings as one list; none where any leg will not read, or the cell recorded nothing. */
export const recordingOf = (
  dir: string
): Effect.Effect<ReadonlyArray<Bundle.RecordedMessage>, never, FileSystem.FileSystem> =>
  Effect.gen(function* () {
    const at = path.join(dir, RECORDING_DIR)
    const legs = yield* legFilesOf(at)
    if (Option.isNone(legs)) return []
    const messages: Array<Bundle.RecordedMessage> = []
    for (const leg of legs.value.filter((f) => f.endsWith(".jsonl"))) {
      const read = yield* legOf(path.join(at, leg)).pipe(orAbsent(`recording ${leg}`))
      if (Option.isNone(read)) return []
      messages.push(...read.value)
    }
    return messages
  })
