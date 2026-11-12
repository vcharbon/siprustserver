/**
 * What a RUN BUNDLE says about itself on disk, and how the session tree is
 * walked for the ones a pass wants.
 *
 * A run bundle is everything one cell of one run left behind: the document it
 * ran (`pivot.json`), what went over the wire, and what the confrontation made
 * of it (`verdict.json`). It lives at `<sessions>/<stem>/run<id>/<cell>`, and
 * the cell directory NAMES its case and its lane — `<caseId>__<kind>__<lane>` —
 * so which case a bundle belongs to is read without opening anything.
 *
 * Nothing here interprets a bundle. It states where the files are and which
 * cell passed, and every reading of the contents belongs to the pass that wants
 * it.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { stemOf } from "@sip/corpus/paths"

/** The document one cell ran, as the run bundle keeps it. */
export const PIVOT_FILE = "pivot.json"

/** What the confrontation made of the run. */
export const VERDICT_FILE = "verdict.json"

/** One cell of one run: which case ran on which lane, and how it came out. */
export interface RunBundle {
  /** The cell directory, holding the document, the wire and the verdict. */
  readonly dir: string
  /** The capture's session directory, holding its enriched flows under `src/`. */
  readonly session: string
  /** The capture, as the corpus and the run database key it: a pcap BASENAME. */
  readonly capture: string
  readonly caseId: string
  readonly lane: string
  /** The run the bundle belongs to, as the store assigned it. */
  readonly run: number
  /** Whether the interpreter passed the cell. */
  readonly passed: boolean
}

/** Where a capture's own artifacts live under its session: the enriched flows a re-cut reads. */
export const srcDirOf = (bundle: RunBundle): string =>
  path.join(bundle.session, "src", bundle.capture)

/** The enriched flows document of a capture, as its session tree holds it. */
export const flowsFile = (bundle: RunBundle): string =>
  path.join(srcDirOf(bundle), `${bundle.capture}.anon.flows.json`)

/** The case and lane a cell directory names: `<caseId>__<kind>__<lane>`. */
const nameOf = (cell: string): { caseId: string; lane: string } | undefined => {
  const parts = cell.split("__")
  const lane = parts.length < 3 ? undefined : parts[parts.length - 1]
  return lane === undefined || parts[0] === undefined
    ? undefined
    : { caseId: parts.slice(0, parts.length - 2).join("__"), lane }
}

/** Whether the interpreter passed, or `undefined` where it left no verdict. */
const passedIn = (dir: string) =>
  Effect.flatMap(FileSystem.FileSystem, (fs) => fs.readFileString(path.join(dir, VERDICT_FILE))).pipe(
    Effect.map((text) => {
      const doc = JSON.parse(text) as { readonly status?: string }
      return doc.status === undefined ? undefined : doc.status === "ok"
    }),
    Effect.catchCause(() => Effect.succeed(undefined))
  )

/** The directories under `dir`, in directory order; none where it does not read. */
const directoriesOf = Effect.fn("RunBundle.directoriesOf")(function*(dir: string) {
  const fs = yield* FileSystem.FileSystem
  const entries = yield* fs.readDirectory(dir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))
  const dirs: Array<string> = []
  for (const entry of entries) {
    const info = yield* fs.stat(path.join(dir, entry)).pipe(Effect.option)
    if (info._tag === "Some" && info.value.type === "Directory") dirs.push(entry)
  }
  return dirs as ReadonlyArray<string>
})

/** The session stems a triage tree holds, in directory order. */
export const stemsUnder = (sessions: string) =>
  Effect.map(directoriesOf(sessions), (dirs) => dirs.filter((name) => !name.startsWith("_")))

/**
 * Every run bundle of one capture's session, newest run last. A cell that left
 * no verdict is not a bundle — nothing about it can be read — and contributes
 * nothing rather than a row that reads as a failure.
 */
export const bundlesOf = Effect.fn("RunBundle.bundlesOf")(function*(sessions: string, capture: string) {
  const session = path.join(sessions, stemOf(capture))
  const out: Array<RunBundle> = []
  const runs = (yield* directoriesOf(session))
    .filter((name) => /^run\d+$/.test(name))
    .map((name) => Number(name.slice(3)))
    .sort((a, b) => a - b)
  for (const run of runs) {
    const runDir = path.join(session, `run${run}`)
    for (const cell of yield* directoriesOf(runDir)) {
      if (cell === "specs") continue
      const named = nameOf(cell)
      if (named === undefined) continue
      const dir = path.join(runDir, cell)
      const passed = yield* passedIn(dir)
      if (passed === undefined) continue
      out.push({ dir, session, capture, caseId: named.caseId, lane: named.lane, run, passed })
    }
  }
  return out as ReadonlyArray<RunBundle>
})

/**
 * The capture each session directory stands for: the one enriched capture under
 * its `src/`. Read rather than derived, because a stem is a capture with its
 * pcap suffix off and putting the suffix back is a guess.
 */
export const captureOf = (sessions: string, stem: string) =>
  Effect.flatMap(FileSystem.FileSystem, (fs) => fs.readDirectory(path.join(sessions, stem, "src"))).pipe(
    Effect.map((held) => held.find((name) => name.startsWith(stem))),
    Effect.orElseSucceed(() => undefined)
  )
