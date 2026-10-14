/**
 * Which RFC-violation census run a cut reads: the latest dated report
 * (`<YYYY-MM-DD>[a-z].census.json`) in the census root (`Roots`).
 *
 * The census is working data the host holds, not a file a tree commits, and it
 * decides documents: every census-charged capture's must-fail steps come from
 * it. A cut that read none would state a different document for each of those
 * captures, and the store would book the moves as the code's — so a cut with
 * no run to read is refused, never shrugged. A run that swept only some of the
 * cut's captures is stated in one coverage line, which `--strict-census` turns
 * into a refusal; an empty run is refused by name.
 */
import { Census as CensusMerge } from "@sip/pipeline"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as path from "node:path"
import type { Paths } from "../paths.js"

/** The latest dated run under `dir`, absolute; `undefined` where it holds none. */
export const latest = Effect.fn("Census.latest")(function*(dir: string) {
  const fs = yield* FileSystem.FileSystem
  const names = yield* fs.readDirectory(dir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))
  const newest = CensusMerge.latestCensusReport(names)
  return newest === undefined ? undefined : path.join(dir, newest)
})

/** How many uncovered captures a coverage line names before it counts the rest. */
const NAMED = 10

/** The captures of a cut a census run swept, and the ones it did not, in the cut's order. */
export interface CaptureCoverage {
  readonly covered: number
  readonly total: number
  readonly uncovered: ReadonlyArray<string>
}

/** Which of `captures` the census run covering `swept` covers. */
export const captureCoverage = (
  captures: ReadonlyArray<string>,
  swept: ReadonlyArray<string>
): CaptureCoverage => {
  const known = new Set(swept)
  const uncovered = captures.filter((capture) => !known.has(capture))
  return { covered: captures.length - uncovered.length, total: captures.length, uncovered }
}

/** One line: how many captures the run covers, and the first uncovered ones by name. */
export const coverageLine = (coverage: CaptureCoverage): string => {
  const head = `census covers ${coverage.covered}/${coverage.total} captures`
  if (coverage.uncovered.length === 0) return head
  const named = coverage.uncovered.slice(0, NAMED).join(", ")
  const more = coverage.uncovered.length - NAMED
  return `${head}; uncovered: ${named}${more > 0 ? ` (+${more} more)` : ""}`
}

/** How much of a report's head is read for its capture list before the whole report is parsed. */
const HEAD_BYTES = 16 * 1024 * 1024

/**
 * The end, in `text` from `open` (a `[`), of that JSON array: the index of its
 * closing `]`, brackets inside strings skipped; `undefined` where `text` ends first.
 */
const arrayEnd = (text: string, open: number): number | undefined => {
  let depth = 0
  let inString = false
  for (let at = open; at < text.length; at++) {
    const c = text[at]
    if (inString) {
      if (c === "\\") at++
      else if (c === '"') inString = false
    } else if (c === '"') inString = true
    else if (c === "[") depth++
    else if (c === "]" && --depth === 0) return at
  }
  return undefined
}

/** The `captures` of a parsed report: a list of strings, or none stated. */
const capturesIn = (report: unknown): ReadonlyArray<string> => {
  const captures = (report as { captures?: unknown } | null)?.captures
  return Array.isArray(captures) ? captures.filter((c): c is string => typeof c === "string") : []
}

/** What a cut is told of a report the sweep left empty. */
const emptyReport = (file: string): string =>
  `the census report ${file} is empty — the sweep wrote no report (a sweep over no *.flows.json ` +
  "says so in its sweep.log); re-run it or remove the empty file"

/** The first `bytes` of `file`, decoded. */
const headOf = (fs: FileSystem.FileSystem, file: string, bytes: number) =>
  Effect.scoped(
    Effect.gen(function*() {
      const handle = yield* fs.open(file, { flag: "r" })
      const size = Number((yield* handle.stat).size)
      const chunk = yield* handle.readAlloc(FileSystem.Size(Math.min(bytes, size)))
      return Option.match(chunk, { onNone: () => "", onSome: (bytes) => new TextDecoder().decode(bytes) })
    })
  )

/**
 * The captures a census report states it swept, or why the report cannot be
 * read. The sweep writes the list before its hits, so it is read off the head
 * of a report of hundreds of megabytes; a report laid out otherwise is parsed
 * whole. A report that states no list covers nothing.
 */
export const sweptCaptures = (
  file: string
): Effect.Effect<ReadonlyArray<string> | { readonly unreadable: string }, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const text = yield* headOf(fs, file, HEAD_BYTES)
    if (text.length === 0) return { unreadable: emptyReport(file) }
    const key = text.indexOf('"captures"')
    const hits = text.indexOf('"hits"')
    const open = key < 0 ? -1 : text.indexOf("[", key)
    const end = open < 0 ? undefined : arrayEnd(text, open)
    if (key >= 0 && (hits < 0 || key < hits) && end !== undefined) {
      return capturesIn({ captures: JSON.parse(text.slice(open, end + 1)) as unknown })
    }
    return capturesIn(JSON.parse(yield* fs.readFileString(file)) as unknown)
  }).pipe(
    Effect.catchCause((cause) =>
      Effect.succeed({ unreadable: `the census report ${file} cannot be read: ${String(Cause.squash(cause))}` })
    )
  )

/** Why a cut over these roots cannot run, or `null` when a census run is there to read. */
export const refusal = Effect.fn("Census.refusal")(function*(paths: Paths) {
  const fs = yield* FileSystem.FileSystem
  const file = yield* latest(paths.census)
  if (file === undefined) {
    return `no census run at ${paths.census} — the cut declares its must-fail steps from the latest ` +
      "dated <date>.census.json there, and without one it would state a different document for " +
      "every census-charged capture"
  }
  const size = yield* fs.stat(file).pipe(Effect.map((info) => Number(info.size)), Effect.orElseSucceed(() => 0))
  return size === 0 ? emptyReport(file) : null
})
