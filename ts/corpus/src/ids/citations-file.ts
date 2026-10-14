/**
 * A CITATIONS FILE: a JSON list of citations (kind, ref, citedBy, citedOn),
 * the form a command that reads a tracker writes once and the commands that
 * honour citations read back without one.
 */
import type { Citations } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"

const KINDS: ReadonlyArray<string> = ["run", "job", "sweep", "prep", "case"]

/** The citations `file` holds; an entry that is no citation dies, naming its index. */
export const read = Effect.fn("CitationsFile.read")(function*(file: string) {
  const text = yield* Effect.flatMap(FileSystem.FileSystem, (fs) => fs.readFileString(file)).pipe(Effect.orDie)
  const parsed = JSON.parse(text) as unknown
  if (!Array.isArray(parsed)) throw new Error(`${file}: a citations file is a JSON list`)
  return parsed.map((entry: Citations.Citation, at): Citations.Citation => {
    if (!KINDS.includes(entry?.kind) || typeof entry.ref !== "string" || typeof entry.citedBy !== "string") {
      throw new Error(`${file}: entry ${at} is no citation (kind, ref, citedBy, citedOn)`)
    }
    return { kind: entry.kind, ref: entry.ref, citedBy: entry.citedBy, citedOn: entry.citedOn ?? null }
  })
})

/** Writes `citations` to `file`, as {@link read} reads it back. */
export const write = (file: string, citations: ReadonlyArray<Citations.Citation>) =>
  Effect.flatMap(FileSystem.FileSystem, (fs) => fs.writeFileString(file, `${JSON.stringify(citations, null, 1)}\n`))
