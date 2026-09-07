/**
 * The CAMPAIGN: a matrix of cells, one run directory, one `campaign.json`, and
 * an exit code that IS the verdict.
 *
 * Cells run one at a time by default. A replay cell binds real sockets and a
 * rust-test cell compiles a crate, so overlapping them makes a campaign's result
 * depend on the host it ran on — and a flaky matrix is worse than a slow one.
 * `concurrency` is there for a deployment that has measured otherwise, and
 * `startsPerSecond` beside it for one whose SUT cares how fast calls ARRIVE and
 * not only how many are up (`./pace.ts`).
 *
 * The index is written even when a cell crashed, and especially then: a campaign
 * that dies without an index leaves a run directory nobody can read.
 */
import { Campaign, type Confrontation, E2e } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import { cargoTest, runCell, type CellRun, type TestInvocation } from "./cells.js"
import { CAMPAIGN_INDEX } from "./layout.js"
import { perSecond, unpaced } from "./pace.js"

export interface CampaignOptions {
  /** Where this run writes. Created if it does not exist; existing cells are overwritten. */
  readonly runDir: string
  /** The run's timestamp label — the caller's, so a re-run can reuse one. */
  readonly ts: string
  /** How a crate test is invoked. */
  readonly tests?: TestInvocation
  /** How many cells may run at once. One unless a deployment has measured otherwise. */
  readonly concurrency?: number
  /**
   * A ceiling on how fast cells START, in cells per second. Absent — the
   * default — a cell starts the moment a concurrency slot frees.
   */
  readonly startsPerSecond?: number
  /**
   * Whether classification decides a replay cell's outcome. `record` (the
   * default) writes every confrontation and fails nothing because of one;
   * `enforce` fails a cell on any `unlisted` or `unknown` record.
   */
  readonly mode?: Confrontation.ConfrontMode
}

/** One finished campaign. */
export interface CampaignRun {
  readonly index: E2e.CampaignIndex
  /** Every cell, in campaign order, with what a reader is told about it. */
  readonly cells: ReadonlyArray<CellRun>
  /** Where the index was written. */
  readonly indexPath: string
}

export const runCampaign = Effect.fn("Driver.runCampaign")(function* (
  input: Campaign.CampaignInput,
  options: CampaignOptions
) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const runDir = path.resolve(options.runDir)
  yield* fs.makeDirectory(runDir, { recursive: true })

  const context = { runDir, tests: options.tests ?? cargoTest, mode: options.mode ?? "record" as const }
  const pace = options.startsPerSecond === undefined
    ? unpaced
    : yield* perSecond(options.startsPerSecond)
  const cells = yield* Effect.forEach(
    input.cells,
    (cell) => Effect.andThen(pace.admit, runCell(cell, context)),
    { concurrency: options.concurrency ?? 1 }
  )

  const index: E2e.CampaignIndex = {
    campaign: input.campaign,
    ts: options.ts,
    cells: cells.map((c) => c.summary)
  }
  const indexPath = path.join(runDir, CAMPAIGN_INDEX)
  yield* fs.writeFileString(indexPath, E2e.emitCampaignIndex(index))
  return { index, cells, indexPath } satisfies CampaignRun
})

/** The exit code a finished campaign is: 0 where every cell that RAN passed, 1 otherwise. */
export const exitCodeOf = (run: CampaignRun): number => (E2e.campaignPassed(run.index) ? 0 : 1)

/** One line per cell, in the order the campaign listed them. */
export const summarize = (run: CampaignRun): string =>
  run.cells
    .map((c) => {
      const mark = c.summary.error !== undefined
        ? "ERROR"
        : c.summary.skipped !== undefined
        ? "SKIP"
        : c.summary.passed
        ? "ok"
        : "FAILED"
      const why = c.summary.error ?? c.summary.skipped
      return `${mark.padEnd(6)} ${c.summary.dir}${why === undefined ? "" : ` — ${why}`}`
    })
    .join("\n")
