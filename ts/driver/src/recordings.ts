/**
 * The per-leg recordings of one finished bundle, as the confrontation reads
 * them: keyed by leg name, legs in name order, so everything that walks the
 * legs (the wire order's tie between two receptions of the same microsecond
 * included) is the same on every file system.
 */
import { Bundle } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import { RECORDING_DIR } from "./layout.js"

/** Every per-leg recording under `<absolute>/recording`, keyed by leg name, legs in name order; none where it has none. */
export const readRecordings = Effect.fn("Driver.readRecordings")(function* (absolute: string) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const dir = path.join(absolute, RECORDING_DIR)
  const out = new Map<string, ReadonlyArray<Bundle.RecordedMessage>>()
  if (!(yield* fs.exists(dir))) return out
  for (const entry of [...(yield* fs.readDirectory(dir))].sort()) {
    if (!entry.endsWith(".jsonl")) continue
    const text = yield* fs.readFileString(path.join(dir, entry))
    out.set(entry.slice(0, -".jsonl".length), yield* Bundle.decodeRecording(text))
  }
  return out
})
