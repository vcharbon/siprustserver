/**
 * The phases of `run.ts` around what a pack fills ({@link Filled}: its
 * provisioning, its compile, what it draws over a lane's kept attempts, the
 * refs its case files port, why its target or a lane does not run a cell):
 * the campaign's setup operations around the rest of the run
 * (`setup-phase.ts`; one that left the lab changed makes each lane the report
 * closed an error), the job's collectors around the lanes' replays
 * (`job-collectors.ts`; one whose close left the lab changed likewise), each
 * lane's replay under its run row's bundle (`replay.ts`), and the report
 * (`report.ts`). A retry's batch (`run.ts`) adds its cells to the cells a
 * campaign job plans.
 *
 * A cell is skipped on a lane, never replayed and never an error, where a tag
 * leaves it out of a campaign on its lane (`excluded: …`, `selection.ts`),
 * forced or not, a case run on its own never; where the pack's lane says it
 * does not run it; where it asks for an operation its lane binds none of
 * (`operation-needs.ts`), forced or not; where its case needs a capability
 * its lane lacks, unless the run is forced (`needs.ts`); where it was compiled
 * with a lane timer its lane states otherwise (`timers.ts`); and where the
 * pack's target does not run it. A cell's claims are answered on a lane by
 * the collectors of their names it wires (`claims.ts` `requestedOn`), a quick
 * run's beside those its level never runs. A lane's batch whose runnable
 * cells claim of a `required` collector nothing on the lane answers is
 * refused before any call (`claims.ts`).
 */
import { Collectors } from "@sip/corpus"
import type { LaneCatalog, LaneView, OperationRun } from "@sip/rig"
import * as Cause from "effect/Cause"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { type Cell, type CellResult, errored, type Lane, skippedOf } from "./cell.js"
import { callRecordsUnwiredOf, requestedOn, requiredUnansweredOf } from "./claims.js"
import * as JobScope from "./job-collectors.js"
import { catalogLane, resolve } from "./lanes.js"
import { unfitOf } from "./needs.js"
import { unboundOperationsOf } from "./operation-needs.js"
import type { CampaignPack } from "./pack.js"
import * as Record from "./record.js"
import type { LaneHosting } from "./replay.js"
import { replayLane } from "./replay.js"
import { report, type ReportPack, type ReportStore } from "./report.js"
import type * as Results from "@sip/store/campaign-results"
import { type CompiledCells, type LaneOutcome, PhaseFailed, type Phases } from "./run.js"
import { exclusionOf, logLines, type Tagging } from "./selection.js"
import { leftChangedOf, setupOf } from "./setup-phase.js"
import { type TimerKeys, unrunnableOf as timerUnrunnableOf } from "./timers.js"

export interface Options<L extends LaneView.View = LaneView.View> {
  /** The lanes the run reads its lanes off. */
  readonly catalog: LaneCatalog.Catalog<L>
  /** The backend the cells' calls are routed by (`host:port`). */
  readonly routing: string
  /** A quick run (`quick.ts`). */
  readonly quick: boolean
  /** Whether the run keeps one case (and sub-case) only: no tag leaves it out. */
  readonly individually: boolean
  /** What the run covers where it is not every case (`scope.ts`); null for a full run. */
  readonly scope: string | null
  /** Whether the campaign's full runs report the refs no case covers. */
  readonly coverage: boolean
  /** Whether a cell whose case needs a capability its lane lacks runs anyway. */
  readonly force: boolean
  /** The tags the deployment declares, and those the run opted into. */
  readonly tagging: Tagging
  /** The run's directory. */
  readonly root: string
  readonly junit: string | undefined
  /** The triage site the report's links point at. */
  readonly site: string
  /** What a campaign job counts: the cells it plans once compiled, each judged one as it lands. */
  readonly progress?: Progress
  /** The run's record: its campaign run row and each lane's run row. */
  readonly record: Record.Record
  /** Its campaign's setup operations, planned over its lanes, and where their events go (`setup-phase.ts`). */
  readonly setup: { readonly instances: ReadonlyArray<OperationRun.Instance>; readonly sink: OperationRun.Sink }
}

/** A campaign job's counts: its planned cells, the judged ones so far. */
export interface Progress {
  readonly planned: (cells: number) => Effect.Effect<void>
  readonly judged: (cells: number) => Effect.Effect<void>
}

/** What a pack fills of a run's phases over its cells `C` on its lanes `L`. */
export interface Filled<C extends Cell, L extends LaneView.View, R> {
  readonly provision: Effect.Effect<void, PhaseFailed, R>
  readonly compile: Effect.Effect<CompiledCells<C>, PhaseFailed, R>
  readonly laneKept: (lane: Lane, kept: ReadonlyArray<CellResult<C>>) => Effect.Effect<void, never, R>
  /** The refs the run's case files state: the ones it ports. */
  readonly ported: Effect.Effect<ReadonlyArray<string>, never, R>
  /** Why the pack's lane `found` does not run `cell`, forced or not, or `undefined`; asked right after the tags. */
  readonly laneUnrunnable: (found: L, cell: C) => string | undefined
  /** Why the pack's target does not run `cell` on any lane, or `undefined`; asked last. */
  readonly targetUnrunnable: (cell: C) => string | undefined
}

/** The packs a run's phases read: its cells, its lane hosting, its report and its store (as the service `RS` holding it). */
export interface Packs<C extends Cell, L extends LaneView.View, X extends Results.CampaignRun, RH, RS> {
  readonly campaign: CampaignPack<C>
  readonly hosting: LaneHosting<C, L, RH>
  readonly report: ReportPack<X>
  readonly store: Effect.Effect<ReportStore<X>, never, RS>
}

/** What a run's phases run on: the pack's filled phases' `R`, its hosting's `RH`, its store's `RS`, and the runner's. */
export type Needs<C extends Cell, L extends LaneView.View, RH, RS, R> =
  | R
  | RS
  | Effect.Services<ReturnType<typeof replayLane<C, L, RH>>>
  | Effect.Services<ReturnType<typeof wiredOn>>
  | Effect.Services<ReturnType<typeof JobScope.preChecks>>
  | Effect.Services<ReturnType<typeof report<Results.CampaignRun>>>
  | Effect.Services<ReturnType<typeof JobScope.around<C, never, never>>>
  | Effect.Services<ReturnType<typeof setupOf<never>>>

/** Why `cell` is skipped on `found`, or `undefined` where it runs: a tag first, forced or not, then what the lane or the target lacks. */
export const skipReasonOf = <C extends Cell, L extends LaneView.View>(
  pack: Pick<Filled<C, L, never>, "laneUnrunnable" | "targetUnrunnable">,
  timerKeys: TimerKeys,
  options: Pick<Options, "force" | "individually" | "tagging">,
  found: L | undefined,
  cell: C
): string | undefined =>
  (found === undefined
    ? undefined
    : exclusionOf(options.tagging, found, cell.tags, options.individually) ??
      pack.laneUnrunnable(found, cell) ??
      unboundOperationsOf(found, cell) ??
      unfitOf(found, cell, options.force) ??
      timerUnrunnableOf(timerKeys, found, cell)) ??
  pack.targetUnrunnable(cell)

/**
 * The collectors the deployment wires on the catalog lane `lane`, at batch or
 * job scope, under the run's level, that collect every cell of their scope,
 * by name: an `on-failure` one collects no passing cell, so it answers none
 * of a passing cell's claims.
 */
const wiredOn = (lane: string) =>
  Effect.flatMap(Collectors.Service, (seam) =>
    Effect.map(
      Effect.all([seam.batch(lane, "", ""), seam.job([lane], "", "")]),
      ([batch, job]) => new Set([...batch, ...job].filter((one) => one.onFailure !== true).map((one) => one.collector.name))
    ))

/** The run's phases: the pack's filled ones, the setup, the job's collectors, each lane's replay and the report. */
export const phases = <C extends Cell, L extends LaneView.View, X extends Results.CampaignRun, RH, RS, R>(
  packs: Packs<C, L, X, RH, RS>,
  options: Options<L>,
  filled: Filled<C, L, R>
): Phases<Needs<C, L, RH, RS, R>, C> => {
  let judged = 0
  let planned = 0
  const { campaign, hosting } = packs
  const leftOnRecord = (reason: string) =>
    Record.leftChanged(options.record, reason).pipe(
      Effect.catchCause((cause) => Console.error(`${campaign.verb}: the lab left changed not recorded on the run: ${Cause.pretty(cause)}`))
    )
  const idOf = (lane: string) => resolve(hosting.prefix, options.catalog, lane)
  const job = { record: options.record, root: options.root, idOf }
  const preChecked = Effect.flatMap(JobScope.preChecks(campaign, job), (refused) =>
    refused === undefined ? Effect.void : Effect.fail(new PhaseFailed({ phase: "pre-checks", reason: refused })))
  const replay = (lane: Lane, stated: ReadonlyArray<C>, attempt: number) =>
    Effect.flatMap(wiredOn(idOf(lane)), (wired) => {
      const asked = options.quick ? new Set([...wired, ...campaign.quickUnrun]) : wired
      const cells = stated.map((cell) => requestedOn(cell, asked))
      const statedOf = new Map(cells.map((cell, at) => [cell, stated[at]!]))
      const asStated = (result: CellResult<C>): CellResult<C> => ({ ...result, cell: statedOf.get(result.cell) ?? result.cell })
      const row = options.record.lanes.find((r) => r.lane === lane)
      if (row === undefined) return Effect.fail(new PhaseFailed({ phase: `replay ${lane}`, reason: "no run row" }))
      const found = catalogLane(hosting.prefix, options.catalog, lane)
      const unrunnable = cells.flatMap((cell) => {
        const why = skipReasonOf(filled, campaign.timerKeys, options, found, cell)
        return why === undefined ? [] : [skippedOf(errored(lane, cell, why), why)]
      })
      const runnable = cells.filter((cell) => !unrunnable.some((result) => result.cell === cell))
      const unanswered = requiredUnansweredOf(options.catalog.rig, lane, runnable) ??
        (found === undefined || runnable.length === 0 ? undefined : callRecordsUnwiredOf(options.catalog.rig, found))
      if (unanswered !== undefined) return Effect.fail(new PhaseFailed({ phase: `replay ${lane}`, reason: unanswered }))
      const said = found === undefined || attempt > 1
        ? Effect.void
        : Effect.forEach(logLines(options.tagging, found, cells, options.individually), (line) => Console.log(line), { discard: true })
      const retry = attempt === 1 || options.progress === undefined
        ? Effect.void
        : Effect.suspend(() => options.progress!.planned(planned += cells.length))
      const replayed: Effect.Effect<LaneOutcome<C>, PhaseFailed, Effect.Services<ReturnType<typeof replayLane<C, L, RH>>>> =
        runnable.length === 0
        ? Effect.succeed({ results: [] })
        : replayLane(campaign, hosting, lane, runnable, {
          catalog: options.catalog,
          root: options.root,
          bundles: row.bundlePath,
          routing: options.routing,
          quick: options.quick,
          attempt,
          job: options.record.jobId,
          landed: () => options.progress === undefined ? Effect.void : Effect.suspend(() => options.progress!.judged(++judged))
        })
      const counted = Effect.suspend(() =>
        options.progress === undefined || unrunnable.length === 0
          ? Effect.void
          : options.progress.judged(judged += unrunnable.length)
      )
      return Effect.andThen(
        Effect.andThen(Effect.andThen(said, retry), counted),
        Effect.map(replayed, (outcome) => ({ ...outcome, results: [...unrunnable, ...outcome.results].map(asStated) }))
      )
    })
  return {
    provision: Effect.andThen(preChecked, filled.provision),
    setup: (body) => setupOf(options.setup.instances, options.setup.sink, (left) => leftOnRecord(leftChangedOf(left)))(body),
    collectors: (cells, replays) =>
      Effect.tap(
        JobScope.around(campaign, job, cells, replays),
        ({ leftChanged }) => leftChanged.length === 0 ? Effect.void : leftOnRecord(JobScope.leftChangedOf(leftChanged))
      ),
    compile: Effect.tap(filled.compile, ({ cells }) =>
      Effect.suspend(() => options.progress?.planned(planned = cells.length * options.record.lanes.length) ?? Effect.void)),
    replay,
    laneKept: filled.laneKept,
    report: (outcome) =>
      Effect.flatMap(Effect.all([filled.ported, packs.store]), ([ported, store]) =>
        report(store, campaign, packs.report, outcome, {
          root: options.root,
          junit: options.junit,
          site: options.site,
          record: options.record,
          partial: options.scope !== null,
          coverage: options.coverage,
          ported,
          quick: options.quick
        }))
  }
}
