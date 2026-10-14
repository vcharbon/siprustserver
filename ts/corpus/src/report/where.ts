/**
 * WHERE one capture's artifacts are, by absolute path: what it was cut from,
 * what the store holds of its standing cut, what the cut refused, and what
 * each lane's standing run left on disk.
 *
 * The cases are counted unless they are asked for: a capture of thousands of
 * calls is one line, not thousands. Asked for, each case document is
 * materialised on the way past, as the flows document always is, because a
 * path into a lazily filled cache is only an answer once the file is there.
 *
 * A capture neither the artifact store, the cut store nor a run names is
 * said to be unknown: the name is what is wrong, whatever its spelling.
 */
import { Cuts, Runs } from "@sip/store"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import * as Content from "../content/content.js"
import { excludedFile } from "../cut/excluded.js"
import { type Paths, prepDir } from "../paths.js"

/** One labelled line of the answer. */
export interface Entry {
  readonly label: string
  readonly text: string
}

/** What one capture holds, entry by entry. */
export interface Where {
  readonly capture: string
  /** Whether the artifact store, the cut store or a run names the capture. */
  readonly known: boolean
  /** How many cases the standing cut holds; 0 where it holds none. */
  readonly cases: number
  readonly entries: ReadonlyArray<Entry>
}

export interface Options {
  /** The cut to read the capture as of; the standing one where absent. */
  readonly prep?: number | undefined
  /** Whether every case document is listed, materialised, rather than counted. */
  readonly cases: boolean
  /** The command line that records a cut of `capture`, named where none stands. */
  readonly cutCommand: (capture: string) => string
}

/** A materialisation, or the one-line reason the store could not answer it. */
const orSaid = <R>(effect: Effect.Effect<string, unknown, R>) =>
  effect.pipe(
    Effect.catchCause((cause) => Effect.succeed(`unavailable — ${Cause.pretty(cause).split("\n")[0]}`))
  )

/** Every artifact of `capture` under `paths`. */
export const read = Effect.fn("Where.read")(function*(paths: Paths, capture: string, options: Options) {
  const cuts = yield* Cuts.Service
  const runs = yield* Runs.Service
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const exists = (file: string) => fs.exists(file).pipe(Effect.orElseSucceed(() => false))
  const entries: Array<Entry> = []
  const said = (label: string, text: string) => void entries.push({ label, text })

  const corpus = path.join(paths.corpus, capture)
  const inCorpus = yield* exists(corpus)
  said("corpus", inCorpus ? corpus : `${corpus} (not there)`)

  const standing = yield* cuts.standing(capture, options.prep)
  if (standing === null) {
    said(
      "cut",
      (options.prep === undefined ? "none" : `none as of prep${options.prep}`) +
        ` — \`${options.cutCommand(capture)}\` records one`
    )
  } else {
    said(
      "cut",
      `prep${standing.prepId}  ${standing.cutAt}  ` +
        (standing.broke !== null ? `BROKE: ${standing.broke}` : `${standing.cases.length} case(s)`)
    )
    said("cut log", prepDir(paths.sessions, standing.prepId))
    if (standing.flowsHash !== null) {
      said("flows", yield* orSaid(Content.materialiseFlows(paths.sessions, standing.flowsHash)))
    }
    if (options.cases) {
      for (const entry of standing.cases) said("case", yield* orSaid(Content.materialise(paths.sessions, entry.caseHash)))
    }
  }

  const excluded = excludedFile(paths.sessions, capture)
  if (yield* exists(excluded)) said("excluded", excluded)

  let ran = 0
  for (const lane of yield* runs.lanes) {
    const run = yield* runs.standing(capture, lane.lane)
    if (run === null) continue
    ran += 1
    said(
      `run${run.runId}`,
      `${run.bundlePath}  ${lane.lane}  ${run.status}  ` +
        `${run.failed}/${run.cases} failed` +
        (run.rfcGating === 0 ? "" : `  rfc: ${run.rfcGating} gating`) +
        (run.lastOkRunId === null ? "  (never green)" : `  last-ok run${run.lastOkRunId}`)
    )
  }
  return {
    capture,
    known: inCorpus || standing !== null || ran > 0,
    cases: standing?.cases.length ?? 0,
    entries
  } satisfies Where
})
