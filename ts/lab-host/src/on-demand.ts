/**
 * An ON-DEMAND collector run on cells of one lane run (upstream ADR-0060
 * appendix §5, `@sip/corpus` `OnDemand`), as the site's call details and a
 * pack's collect verb ask it: the cells named by their directory under the run's,
 * each landed under its run row and case id; the lane named by its catalog
 * id, as the run row states it. Each ask's directory is its own under the
 * run's `on-demand/<collector>/`, removed once its attachments landed, kept
 * where a step failed.
 */
import { type CollectedCells, Collectors, OnDemand } from "@sip/corpus"
import type { Roots } from "@sip/corpus/deployment/index"
import type { Attachments } from "@sip/store"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as path from "node:path"

/** The directory under a run's where its on-demand runs keep their instances. */
export const DIR = "on-demand"

/** One cell asked of: its directory's name under the run's, and the case result it lands under. */
export interface AskedCell {
  readonly cell: string
  readonly runId: number
  readonly caseId: string
}

/** What an on-demand run is asked. */
export interface Asked {
  readonly collector: string
  /** The cells' lane, by its catalog id. */
  readonly lane: string
  /** The lane run's directory, the cells' parent. */
  readonly runDir: string
  readonly cells: ReadonlyArray<AskedCell>
}

/** The cells of `asked` as the collector reads them. */
export const landedOf = (asked: Asked): ReadonlyArray<CollectedCells.LandedCell> =>
  asked.cells.map((one) => ({
    id: one.cell,
    caseId: one.caseId,
    lane: asked.lane,
    dir: path.join(asked.runDir, one.cell),
    runId: one.runId
  }))

/** The collectors an on-demand run on the lane `lane` (its catalog id) may ask for; none on a lane the deployment does not load. */
export const offered = (lane: string): Effect.Effect<ReadonlyArray<string>, never, Collectors.Service> =>
  Effect.flatMap(Collectors.Service, (seam) => seam.onDemandOn(lane))

/** `asked` run, its attachments landed on its cells; refused on a lane the deployment does not load. */
export const run = (
  asked: Asked
): Effect.Effect<
  OnDemand.Ran,
  OnDemand.Refused,
  Collectors.Service | Attachments.Service | FileSystem.FileSystem | Roots.Service
> =>
  OnDemand.run({
    collector: asked.collector,
    cells: landedOf(asked),
    dir: path.join(asked.runDir, DIR, asked.collector)
  })
