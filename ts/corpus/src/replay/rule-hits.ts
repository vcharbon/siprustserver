/**
 * COUNTING ONE FINISHED BUNDLE against the post-run rule families, cell by
 * cell, into the rows the store records (`RuleHits`).
 *
 * Each cell's counts were taken by the reclassifier off the one reading it
 * made after the run, and the driver wrote them beside the cell as
 * `rule-hits.json` (`CellHits`). What is done HERE is the fold across cells —
 * how loud, how wide, and one cell by name — which is the store's vocabulary
 * and not the pipeline's.
 *
 * A cell without that file is read off its confrontation sheet and verdict,
 * through the deployment's own predicates (`CellHost.hitsOf`), so no second
 * copy of a predicate is measured.
 *
 * A file that is not there takes its own branch; one that will not read or
 * decode is logged and leaves its cell uncounted.
 */
import { Bundle, CellHits, Confrontation, E2e } from "@sip/contracts"
import { Layout } from "@sip/driver"
import type { Reclassifier } from "@sip/pipeline"
import type { RuleHits } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as path from "node:path"
import { CellHost } from "../deployment/index.js"

/** One key of the tally: the family, the rule and the subdivision counted. */
const keyOf = (family: string, rule: string, bucket: string): string => `${family} ${rule} ${bucket}`

/** A tally that counts records, the cells they landed on, and one cell by name. */
class Tally {
  private readonly at = new Map<string, { hits: number; cells: Set<string>; sample: string }>()

  add(family: string, rule: string, bucket: string, cell: string, hits = 1): void {
    const key = keyOf(family, rule, bucket)
    const held = this.at.get(key) ?? { hits: 0, cells: new Set<string>(), sample: cell }
    held.hits += hits
    held.cells.add(cell)
    this.at.set(key, held)
  }

  rows(): ReadonlyArray<RuleHits.Hit> {
    return [...this.at.entries()].map(([key, held]) => {
      const [family = "", rule = "", bucket = ""] = key.split(" ")
      return { family, rule, bucket, hits: held.hits, cells: held.cells.size, sample: held.sample } satisfies RuleHits.Hit
    })
  }
}

/** A file read whole: `None` where it is not there, its text where it is. */
const textOf = (fs: FileSystem.FileSystem, file: string) =>
  Effect.gen(function*() {
    if (!(yield* fs.exists(file).pipe(Effect.orElseSucceed(() => false)))) return Option.none<string>()
    return Option.some(yield* fs.readFileString(file))
  })

/** The cell directories of one run bundle: those that carry a confrontation sheet. */
export const cellsOf = Effect.fn("RuleHits.cellsOf")(function*(runDir: string) {
  const fs = yield* FileSystem.FileSystem
  const entries = yield* fs.readDirectory(runDir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))
  const cells: Array<string> = []
  for (const entry of entries) {
    const sheet = path.join(runDir, entry, Layout.CONFRONTATION_FILE)
    if (yield* fs.exists(sheet).pipe(Effect.orElseSucceed(() => false))) cells.push(entry)
  }
  return cells.sort()
})

/**
 * Each cell directory's pass as the run's index states it, where the bundle
 * carries an index: the driver's gate, which a panicked run fails whatever
 * verdict it left.
 */
const indexedPasses = (runDir: string): Effect.Effect<ReadonlyMap<string, boolean>, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const text = yield* textOf(fs, path.join(runDir, Layout.CAMPAIGN_INDEX))
    if (Option.isNone(text)) return new Map<string, boolean>()
    const index = E2e.decodeCampaignIndexSync(JSON.parse(text.value) as unknown)
    return new Map(index.cells.map((cell) => [cell.dir, cell.passed]))
  }).pipe(
    Effect.catchCause((cause) =>
      Effect.logWarning(`run index ${runDir} not read`, cause).pipe(Effect.as(new Map<string, boolean>()))
    )
  )

/** No verdict on disk: only the sheet counts — a verdict with nothing in it blesses nothing and is not audited. */
const unverdicted = (cell: string, lane: string): Reclassifier.RunOutcome => ({
  caseId: cell,
  lane,
  verdict: { case: cell, lane, status: "failed" } satisfies Bundle.RunVerdict,
  dir: "",
  passed: false
})

/**
 * One cell's counts: the file the reclassifier wrote, else — where it wrote
 * none — the sheet re-read. A cell that cannot be read counts as nothing and
 * is not silently zeroed: it is left out of the cells the caller reports as
 * read.
 */
const countCell = (
  host: CellHost.Interface,
  runDir: string,
  cell: string,
  lane: string,
  indexed: boolean | undefined
): Effect.Effect<Option.Option<CellHits.Hits>, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const dir = path.join(runDir, cell)
    const written = yield* textOf(fs, path.join(dir, Layout.RULE_HITS_FILE))
    if (Option.isSome(written)) return Option.some(CellHits.decodeHitsSync(JSON.parse(written.value) as unknown))
    const sheet = yield* textOf(fs, path.join(dir, Layout.CONFRONTATION_FILE))
    if (Option.isNone(sheet)) return Option.none<CellHits.Hits>()
    const records = Confrontation.parseConfrontationLines(sheet.value)
    const verdictText = yield* textOf(fs, path.join(dir, Layout.VERDICT_FILE))
    const outcome = Option.match(verdictText, {
      onNone: () => unverdicted(cell, lane),
      onSome: (text): Reclassifier.RunOutcome => {
        const verdict = Bundle.decodeRunVerdictSync(JSON.parse(text) as unknown)
        return { caseId: cell, lane, verdict, dir, passed: Bundle.verdictPassed(verdict) && indexed !== false }
      }
    })
    return Option.some(host.hitsOf(outcome, records))
  }).pipe(
    Effect.catchCause((cause) =>
      Effect.logWarning(`${cell} rule hits not read`, cause).pipe(Effect.as(Option.none<CellHits.Hits>()))
    )
  )

/** What one bundle came to, and how much of it could be read. */
export interface Counted {
  readonly hits: ReadonlyArray<RuleHits.Hit>
  /** Cells whose counts could be read; 0 means the bundle proved nothing. */
  readonly cells: number
}

/** Every post-run rule count one run bundle carries. */
export const countBundle = Effect.fn("RuleHits.countBundle")(function*(runDir: string, lane: string) {
  const host = yield* CellHost.Service
  const tally = new Tally()
  let cells = 0
  const passes = yield* indexedPasses(runDir)
  for (const cell of yield* cellsOf(runDir)) {
    const hits = yield* countCell(host, runDir, cell, lane, passes.get(cell))
    if (Option.isNone(hits)) continue
    for (const hit of hits.value) tally.add(hit.family, hit.rule, hit.bucket, cell, hit.hits)
    cells += 1
  }
  return { hits: tally.rows(), cells } satisfies Counted
})
