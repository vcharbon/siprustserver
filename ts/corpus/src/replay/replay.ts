/**
 * The REPLAY phase: a pinned cut driven against a lane, cells running in this
 * process, each one writing straight into the bundle the store named for it.
 *
 * The phase drives an ordered CELL list with no capture boundary: a batch is a
 * slice of `--batch` cells, so one file of 10 000 calls and 10 000 files of one
 * call give the same batch shape. Every capture opens ONE run row before the
 * first batch; the row stays open across batches and closes the moment its
 * LAST cell lands — never at a batch barrier — so confrontation and recording
 * overlap the replay of everything behind them. On a lane with a batch process
 * a cell's verdict is final only once its batch's gate has read (a call
 * context its call left fails it), so there the row closes at the end of the
 * batch its last cell landed in, with every charge in its index. What bounds
 * the work is a permit gate over every cell in flight — the lane's own cell
 * ceiling under the caller's `--concurrency` — and a start pace under the
 * lane's start ceiling, because how fast calls ARRIVE and how many are up are
 * different questions.
 *
 * `CellContext.runDir` is per call, so pointing it at `<stem>/run<id>` makes the
 * cell write `<stem>/run<id>/<cellDir>/` and `<stem>/run<id>/specs/…` where they
 * belong. A cell's evidence is never moved after it is written, which is what
 * lets the post-run reclassifier re-read the finished cell where it stands.
 *
 * Per batch: its cells are materialised and ONE side-row directory holds the
 * rows of every case in flight. On a lane with a batch process every cell runs
 * inside it (`LaneHosting.standBatch`): it takes the post-run cleanup check at
 * the batch's scope, because a gauge counting every cell in flight cannot
 * answer for one call. The lane's SUT is the batch's too — stood up where the
 * deployment manages it, attached to where it stands (`LaneHosting.standSut`).
 * Elsewhere each cell is its own interpreter process. Either runs inside the
 * deployment's box (`Boxing`), per cell or per batch.
 *
 * The phase writes nothing anybody else reads: the bundles are store-named and
 * the cases content-named, so any number of replays run at once over one cut.
 * What a replay does take is its LANE, where the lane is exclusive, and the
 * box's quiet where its clock is real (`session.ts`).
 *
 * A phase STOPS on request, never by being cut: once its `Stop` is done it
 * admits no cell, gives the cells in flight the grace the stop carries, then
 * interrupts them (the driver's cell SIGTERMs its interpreter, SIGKILL after
 * the toolchain's grace; a batch process is ended with its cells in flight,
 * and takes no cleanup gate), and every run row still open closes
 * `interrupted` over the cells that landed. A cell that never landed stays
 * `planned` on the job, which is what a resume replays.
 */
import { E2e } from "@sip/contracts"
import { runCell } from "@sip/driver"
import { perSecond, unpaced } from "@sip/driver/pace"
import { Bundle, Jobs, Requests, RuleHits, Runs, Sweeps } from "@sip/store"
import { ReplayCli } from "@sip/toolchain"
import * as Console from "effect/Console"
import * as Deferred from "effect/Deferred"
import * as Duration from "effect/Duration"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as Semaphore from "effect/Semaphore"
import * as path from "node:path"
import * as Content from "../content/content.js"
import { Boxing, CaseLayout, CellHost, LaneHosting, Tools } from "../deployment/index.js"
import type { BatchHost, SutHandle } from "./batch-protocol.js"
import type { Options } from "../options.js"
import { stemOf } from "../paths.js"
import * as Provenance from "../provenance.js"
import { stated } from "../universe.js"
import { trimTerminated } from "./cell-trim.js"
import * as HeldContexts from "./held-contexts.js"
import * as ReplayIdentity from "./identity.js"
import { type CapturePlan, type CellRef, cellsOf, materialiseBatch, type PlannedCell } from "./plan.js"
import { countBundle } from "./rule-hits.js"

/** What one capture's replay came to, as the closing table reads it. */
export interface Outcome {
  readonly capture: string
  readonly status: "done" | "no-cases" | "error" | "interrupted"
  readonly detail: string
}

/**
 * How a running phase is asked to stop: once done it admits no cell, and what
 * it carries is the seconds the cells in flight get before their interpreters
 * are interrupted.
 */
export type Stop = Deferred.Deferred<number>

/**
 * What a stopped replay exits with: the runtime's own code for an interrupted
 * main fiber, so the exit file and the job row read alike.
 */
export const INTERRUPTED_EXIT = 130

/** One line per capture off the run's own records, so the phase reads as a table. */
export const summarise = (capture: string, name: string, bundle: Bundle.BundleReading): string => {
  const counts = { accepted: 0, known_bug: 0, unlisted: 0, unknown: 0 }
  let failed = 0
  let skipped = 0
  for (const entry of bundle.cases) {
    // A skipped cell never ran, so it is neither a pass nor a failure: the
    // document declared this lane blocked and the driver honoured it.
    if (entry.skipped !== null) skipped += 1
    else if (!entry.passed) failed += 1
    const c = entry.classification as Partial<Record<keyof typeof counts, number>> | null
    if (c === null) continue
    for (const key of Object.keys(counts) as Array<keyof typeof counts>) counts[key] += c[key] ?? 0
  }
  return `${capture}  ${name}: cells=${bundle.cases.length} failed=${failed} ` +
    (skipped === 0 ? "" : `skipped=${skipped} `) +
    `accepted=${counts.accepted} known-bug=${counts.known_bug} ` +
    `unlisted=${counts.unlisted} unknown=${counts.unknown}`
}

/** What a replay invocation produced: its sweep row, its job where it ran one, and one outcome per capture. */
export interface Replay {
  readonly sweep: number
  readonly job: number | undefined
  /** How many batches of cells the phase ran. */
  readonly batches: number
  /** Whether the phase was stopped before its last cell: its open rows closed `interrupted`. */
  readonly interrupted: boolean
  /** The bundle of every run row the phase opened, in capture order. */
  readonly bundles: ReadonlyArray<string>
  readonly outcomes: ReadonlyArray<Outcome>
  /** What each batch's SUT was still holding when it was given back, if any. */
  readonly dirty: ReadonlyArray<string>
  /** How each batch's process died, for every batch whose process did. */
  readonly deadHosts: ReadonlyArray<string>
}

/**
 * What one batch came to: its SUT's cleanup gate, whether its record can be
 * trusted, and the cells it drove.
 */
export interface Batch {
  /** What the SUT was still holding, or `undefined` where it let go of everything. */
  readonly dirty: string | undefined
  /**
   * How the batch process died, or `undefined` where it served the whole batch.
   * A dead process fails every cell after it, so the batch says so beside
   * `dirty` rather than leaving the cause to be inferred.
   */
  readonly deadHost: string | undefined
  /** The cells the batch drove to a result, in the order they landed. */
  readonly driven: ReadonlyArray<CellRef>
  /** How many cells the stop interrupted mid-run: admitted, never landed. */
  readonly interrupted: number
}

/** Where a batch process keeps its scratch, and a managed SUT its log. */
const RUNTIME = "_runtime"

/** Where a sweep's bless audit is appended, one log per sweep. */
const auditLog = (sessions: string, sweep: number): string => path.join(sessions, "_audit", `bless-${sweep}.ndjson`)

/** The run's timestamp label: an ISO instant, seconds resolution. */
const stamp = (): string => new Date().toISOString().replace(/\.\d{3}Z$/, "Z")

/** How many cells at once, and how fast they may start, once the lane has spoken. */
export const ceilings = Effect.fn("Replay.ceilings")(function*(options: Options) {
  const lane = yield* Effect.flatMap(LaneHosting.Service, (hosting) => hosting.lane(options.lane))
  const cells = Math.min(options.concurrency, lane.cellCeiling ?? options.concurrency)
  const paceCeiling = lane.startCeiling
  const starts = paceCeiling === undefined
    ? options.startsPerSecond
    : options.startsPerSecond > 0
    ? Math.min(options.startsPerSecond, paceCeiling)
    : paceCeiling
  return { cells, starts }
})

/** Who stands the batch process of a lane; `undefined` where its cells run on their own. */
export type StandBatch = (
  lane: string,
  request: { readonly under: string; readonly rowsDir?: string; readonly memoryMax: string; readonly cells: number }
) => Effect.Effect<BatchHost | undefined, LaneHosting.LaneUnready | LaneHosting.LaneUnstated>

/** Who stands the SUT of a lane; `undefined` where the cell builds its own. */
export type StandSut = (
  lane: string,
  under: string
) => Effect.Effect<SutHandle | undefined, LaneHosting.LaneUnready | LaneHosting.LaneUnstated>

/** Who runs a cell and stands the batch process and the SUT: named so a test drives the loop without an interpreter. */
export interface Seams {
  /** How one cell runs. */
  readonly drive?: typeof runCell
  /** Who stands the batch process, so a test can hand in one that spawns nothing. */
  readonly host?: StandBatch
  /** Who stands the SUT, likewise. */
  readonly sut?: StandSut
  /** What brings the interpreter up to date before the first cell, so a test builds nothing. */
  readonly binaries?: (watch: Tools.Watch) => Effect.Effect<unknown, Tools.ToolUnavailable>
  /** Who the run rows say made them, so a test names its environment rather than the checkout's. */
  readonly identity?: typeof ReplayIdentity.current
  /** Which code the run rows say ran, so a test names its trees rather than the checkout's. */
  readonly provenance?: typeof Provenance.current
}

/**
 * One capture's run row, open across batches: what it was planned with, what
 * has landed under it so far, and what closed it.
 */
export interface Tally {
  readonly plan: Extract<CapturePlan, { _tag: "cells" }>
  readonly run: Runs.OpenedRun
  /** The cells the row waits for; the row closes when `landed` reaches it. */
  readonly planned: number
  landed: number
  readonly summaries: Array<E2e.CellSummary>
  /** Why the row closes `error` rather than on its bundle, once a batch of it broke. */
  broke: string | undefined
  outcome: Outcome | undefined
}

/** What one batch is asked to do. */
export interface BatchRequest extends Seams {
  /** The batch's cells, in the order they are driven. */
  readonly cells: ReadonlyArray<CellRef>
  /** Every open run row, by capture; a batch closes the rows its cells complete. */
  readonly tallies: ReadonlyMap<string, Tally>
  readonly options: Options
  /** The stop the batch honours, where its caller can be stopped. */
  readonly stop?: Stop
  /** What each cell that landed is told to, the moment it lands: a job marks it ran. */
  readonly ran?: (cell: CellRef) => Effect.Effect<void, unknown>
}

/**
 * One capture, closed on what its own cells wrote across every batch it sat
 * in: the bundle's index over ALL its cells, the row, the rule counts.
 * Closed `interrupted` where the phase was stopped before its last cell: the
 * index holds the cells that landed, and the row says how many were planned.
 */
const close = Effect.fn("Replay.close")(function*(tally: Tally, options: Options, interrupted = false) {
  const runs = yield* Runs.Service
  const ruleHits = yield* RuleHits.Service
  const host = yield* CellHost.Service
  const fs = yield* FileSystem.FileSystem
  const { capture } = tally.plan
  const { run } = tally
  if (tally.broke !== undefined) {
    const detail = `${capture}  ERROR ${tally.broke}`
    yield* runs.finish({ id: run.id, status: "error", detail: tally.broke, cases: [] })
    yield* Console.error(detail)
    tally.outcome = { capture, status: "error", detail }
    return
  }
  // The bundle's own index, written where `Bundle.read` and the viewer already
  // look for it; in cell order, whatever order the cells landed in.
  const cells = [...tally.summaries].sort((a, b) => (a.cell < b.cell ? -1 : a.cell > b.cell ? 1 : 0))
  yield* fs.makeDirectory(run.bundlePath, { recursive: true })
  yield* fs.writeFileString(
    path.join(run.bundlePath, "campaign.json"),
    E2e.emitCampaignIndex({ campaign: `triage-${run.name}`, ts: stamp(), cells })
  )
  const bundle = yield* Bundle.read(run.bundlePath, { charges: host.charges })
  const stopped = `interrupted after ${tally.landed} of ${tally.planned} cell(s)`
  yield* runs.finish({
    id: run.id,
    status: interrupted ? "interrupted" : bundle.status,
    ...(interrupted ? { detail: stopped } : bundle.detail === undefined ? {} : { detail: bundle.detail }),
    cases: bundle.cases
  })
  // The post-run rule counts are taken while the bundle is still on disk:
  // retention keeps only a (capture, lane)'s latest and last-green one, so a
  // reading deferred to a viewer would answer for the survivors alone.
  yield* ruleHits.record(run.id, (yield* countBundle(run.bundlePath, options.lane)).hits)
  const line = interrupted ? `${capture}  ${run.name}: INTERRUPTED ${stopped}` : summarise(capture, run.name, bundle)
  yield* Console.log(line)
  tally.outcome = { capture, status: interrupted ? "interrupted" : bundle.status, detail: line }
})

/** A cell landed under its capture's row; the row closes on its last one. */
const landed = (tally: Tally, options: Options, count = 1) =>
  Effect.suspend(() => {
    tally.landed += count
    return tally.landed >= tally.planned ? close(tally, options) : Effect.void
  })

/** The variables every interpreter of a batch runs under: its box, and the batch's side rows. */
const interpreterEnv = (memoryMax: string, rowsDir: string | undefined) =>
  Effect.gen(function*() {
    const boxing = yield* Boxing.Service
    const layout = yield* CaseLayout.Service
    return {
      ...boxing.interpreterVariables(memoryMax),
      ...(rowsDir === undefined || layout.interpreterVariable === undefined
        ? {}
        : { [layout.interpreterVariable]: rowsDir })
    } as Readonly<Record<string, string>>
  })

/**
 * One batch: its cells materialised, then every cell run under one side-row
 * directory, one batch process (where the lane has one) and one SUT, each
 * capture's row closed as its own last cell lands — or, under a batch
 * process, once the batch's gate has read and charged its cells.
 * A capture a batch cannot materialise breaks: its cells here and in every
 * batch after it are landed unrun, and its row closes `error`. A stop asked
 * for mid-batch admits no further cell, waits the stop's grace for the cells
 * in flight, then interrupts them; the batch's end runs as on any other.
 */
export const batch = Effect.fn("Replay.batch")(function*(request: BatchRequest) {
  const { options, stop, tallies } = request
  const hosting = yield* LaneHosting.Service
  const cellHost = yield* CellHost.Service
  const drive = request.drive ?? runCell
  const standHost: StandBatch = request.host ??
    ((lane, ask) =>
      hosting.standBatch(lane, {
        under: ask.under,
        ...(ask.rowsDir === undefined ? {} : { rowsDir: ask.rowsDir }),
        memoryMax: ask.memoryMax,
        cells: ask.cells
      }))
  const standSut: StandSut = request.sut ?? ((lane, under) => hosting.standSut(lane, under))
  const tests = yield* cellHost.tests
  const tallyOf = (capture: string): Tally => tallies.get(capture)!

  const broken = (ref: CellRef): boolean => tallyOf(ref.capture).broke !== undefined
  // A capture broken by an earlier batch is never materialised again.
  const materialised = yield* materialiseBatch({
    cells: request.cells.filter((ref) => !broken(ref)),
    lane: options.lane,
    sessions: options.paths.sessions,
    flowsHash: (capture) => tallyOf(capture).plan.flowsHash,
    concurrency: options.concurrency
  })
  for (const [capture, why] of materialised.broke) tallyOf(capture).broke = why
  const cells = materialised.cells
  // Every cell of a broken capture lands unrun, so its row still closes on its last.
  const unrun = new Map<string, number>()
  for (const ref of request.cells) {
    if (broken(ref)) unrun.set(ref.capture, (unrun.get(ref.capture) ?? 0) + 1)
  }
  for (const [capture, count] of unrun) yield* landed(tallyOf(capture), options, count)

  const runtime = path.join(options.paths.sessions, RUNTIME)
  // The batch's one side-row directory, which every cell's peers read their rows from.
  const merged = cells.length === 0
    ? undefined
    : yield* Content.materialiseLedger(options.paths.sessions, [...new Set(cells.map((entry) => entry.caseHash))])
  const env = yield* interpreterEnv(options.memoryMax, merged)
  const limits = yield* ceilings(options)
  // The lane's batch process, for as long as the batch: it serves what the
  // SUT asks of its peers, so it stands first. Killed with the batch's scope
  // whichever way it ends; ended by hand on the way that ends well.
  const host: BatchHost | undefined = merged === undefined
    ? undefined
    : yield* Effect.acquireRelease(
      standHost(options.lane, { under: runtime, rowsDir: merged, memoryMax: options.memoryMax, cells: limits.cells }),
      (held) => held?.kill ?? Effect.void
    )

  // The lane's SUT, for as long as the batch: started here where the
  // deployment manages it, attached to where it already stands — after the
  // batch process. Given back whichever way the batch ends.
  const sut = yield* Effect.acquireRelease(
    cells.length === 0 ? Effect.succeed(undefined) : standSut(options.lane, runtime),
    (held) => held?.stop ?? Effect.void
  )

  const gate = yield* Semaphore.make(limits.cells)
  const pace = limits.starts > 0 ? yield* perSecond(limits.starts) : unpaced
  const stopping = stop === undefined ? Effect.succeed(false) : Deferred.isDone(stop)

  /** The cells handed to the driver, and the ones that came back with a result. */
  const admitted = new Set<PlannedCell>()
  const driven: Array<CellRef> = []

  /**
   * One cell, run and landed under its row — which closes here on its last,
   * or once the batch's gate has read where a batch process runs the cells.
   * Not admitted once a stop is in: it stays planned. The landing is
   * uninterruptible, so a cell whose result is in hand is never half-recorded.
   */
  const one = (planned: PlannedCell) => {
    const tally = tallyOf(planned.capture)
    return gate.withPermits(1)(Effect.gen(function*() {
      yield* pace.admit
      if (yield* stopping) return
      admitted.add(planned)
      const finished = yield* drive(planned.cell, { runDir: tally.run.bundlePath, tests, mode: "record" })
      yield* trimTerminated(path.join(tally.run.bundlePath, finished.summary.dir), finished.summary.passed)
      yield* Effect.uninterruptible(Effect.suspend(() => {
        tally.summaries.push(finished.summary)
        const { cell: _cell, ...ref } = planned
        driven.push(ref)
        // Marked as it lands, not at the batch's end: a real-clock batch is a
        // quarter of an hour, and the job's progress reads these marks.
        return Effect.andThen(
          request.ran === undefined ? Effect.void : Effect.ignore(request.ran(ref)),
          host === undefined ? landed(tally, options) : Effect.sync(() => void (tally.landed += 1))
        )
      }))
    }))
  }

  // Every cell in flight at once, bounded by the permit gate — on a fiber of
  // its own, so a stop can wait for it and then cut it short. A batch
  // process's cells run in it: the interpreter seam's `run` is the process's,
  // everything else about a cell is `runCell`'s as ever.
  const inHost = <A, E, R>(self: Effect.Effect<A, E, R>) =>
    host === undefined ? self : Effect.updateService(self, ReplayCli.Service, (cli) => ({ ...cli, run: host.run }))
  const running = yield* Effect.forkChild(
    Effect.forEach(cells, one, { concurrency: "unbounded", discard: true }).pipe(
      Effect.provideService(ReplayCli.Environment, env),
      inHost
    )
  )
  const stopped = stop === undefined
    ? false
    : yield* Effect.raceFirst(Effect.as(Fiber.await(running), false), Effect.as(Deferred.await(stop), true))
  if (stopped) {
    const grace = yield* Deferred.await(stop!)
    const inFlight = () => admitted.size - driven.length
    yield* Console.log(
      `[replay] stop requested: admitting no new cell; ${inFlight()} cell(s) in flight, waiting up to ${grace} s`
    )
    const finished = yield* Fiber.await(running).pipe(Effect.timeoutOption(Duration.seconds(grace)))
    if (Option.isNone(finished)) {
      yield* Console.log(`[replay] grace over: interrupting ${inFlight()} cell(s)`)
      yield* Fiber.interrupt(running)
    }
  }
  const ran = yield* Fiber.await(running)
  if (!stopped && Exit.isFailure(ran)) return yield* Effect.failCause(ran.cause)
  const interrupted = admitted.size - driven.length
  if (interrupted > 0) yield* Console.log(`[replay] ${interrupted} cell(s) interrupted, ${driven.length} landed`)
  // Whether the process died is read BEFORE it is ended: one that died
  // mid-batch is a cause, one that ended here is not.
  const deadHost = host?.died()
  if (deadHost !== undefined) yield* Console.error(`[replay] ${deadHost}`)
  // The batch's own gate, in the process once every cell ended. A batch whose
  // stop interrupted cells has its process killed with them in flight, and
  // takes no gate. A managed SUT that DIED is reported as itself, never as a
  // leak.
  const reapedBy = host === undefined || interrupted > 0 ? Effect.succeed(undefined) : host.end
  const hostDirty = deadHost === undefined ? yield* reapedBy : undefined
  const dirty = sut?.died() ?? hostDirty
  if (dirty !== undefined) yield* Console.error(`[replay] ${dirty}`)
  // What the gate read of a standing SUT's call contexts, on every run the
  // batch's cells ran under; a context one cell's call left is that cell's
  // run's cleanup finding, never the batch's.
  const contexts = host?.contexts()
  if (contexts !== undefined) {
    const runs = yield* Runs.Service
    const runOf = new Map(driven.map((ref) => [tallyOf(ref.capture).run.bundlePath, tallyOf(ref.capture).run.id]))
    yield* HeldContexts.keep(contexts, [...runOf.keys()], new Date().toISOString())
    for (const charge of HeldContexts.charges(contexts, [...runOf.keys()])) {
      yield* Console.error(`[replay] ${charge.finding}`)
      yield* runs.dirty([runOf.get(charge.runDir)!], charge.finding)
      // The charged cell's verdict is already failed on disk: its index entry says so too.
      const tally = [...tallies.values()].find((one) => one.run.bundlePath === charge.runDir)!
      const at = tally.summaries.findIndex((summary) => summary.dir === charge.cell)
      if (at >= 0) tally.summaries[at] = { ...tally.summaries[at]!, passed: false }
    }
  }
  // A batch process's rows close once its gate has read, each on its last cell.
  if (host !== undefined) {
    for (const tally of new Set(driven.map((ref) => tallyOf(ref.capture)))) {
      if (tally.outcome === undefined && tally.landed >= tally.planned) yield* close(tally, options)
    }
  }
  return { dirty, deadHost, driven, interrupted } satisfies Batch
  // Scoped: the batch process is released with the batch, whichever way it
  // ends, so nothing is left holding the lane's addresses.
}, Effect.scoped)

/** What the phase is asked to replay. */
export interface PhaseRequest extends Seams {
  /** One plan per capture, in the order their rows open. */
  readonly captures: ReadonlyArray<CapturePlan>
  readonly options: Options
  /** The prep every capture is read AS OF. */
  readonly prepId: number
  /** The job this replay is, where it is one: a request's replay always is. */
  readonly job?: JobRequest
  /** How the phase is asked to stop, where its caller can ask. */
  readonly stop?: Stop
}

/**
 * The job a request's replay runs as: its row, already `running`, and the
 * request resolved against the pin. The phase records the job cells against
 * the row, marks them `ran` as each batch lands, and writes its sweep and its
 * batch on the row so the job reads them.
 */
export interface JobRequest {
  readonly id: number
  readonly resolution: Requests.Resolution
  /** Whether an earlier pass of this job recorded its cells: a repeated request's later passes record none. */
  readonly recorded?: boolean
}

/**
 * Every run row, opened in capture order as one insert: id order is what
 * `standings` reads as recency, and the bundle path is what the cells about to
 * run write into. Each row names its own set: one replay holds full runs
 * beside cases runs. Every row records who made it and under what load: the
 * ceilings' concurrency and pace, the batch, the memory box, the span bound.
 * One statement for N captures, so a request over 10 000 captures opens
 * 10 000 rows in one round trip.
 */
const openRows = Effect.fn("Replay.openRows")(function*(
  plans: ReadonlyArray<CapturePlan>,
  options: Options,
  prepId: number,
  sweepId: number,
  jobId: number | undefined,
  whoMade: typeof ReplayIdentity.current,
  codeOf: typeof Provenance.current
) {
  const runs = yield* Runs.Service
  const started = Date.now()
  const code = yield* codeOf
  const limits = yield* ceilings(options)
  const who = yield* whoMade(options.lane, {
    concurrency: limits.cells,
    batchSize: options.batchSize,
    startsPerSecond: limits.starts,
    maxSpanSeconds: options.selection.maxSpanSeconds ?? null,
    memoryMax: options.memoryMax
  })
  const opening = plans.map((plan) => ({
    capture: plan.capture,
    lane: options.lane,
    mode: "record",
    sessionDir: path.join(options.paths.sessions, stemOf(plan.capture)),
    provenance: code,
    identity: who,
    sweepId,
    ...(jobId === undefined ? {} : { jobId }),
    prepId,
    setKind: plan.kind
  }))
  const rows = yield* runs.openMany(opening)
  const opened = new Map(rows.map((run, at) => [opening[at]!.capture, run]))
  if (opened.size > 0) yield* Console.log(`[replay] opened ${opened.size} run row(s) in ${Date.now() - started} ms`)
  return opened
})

/** The interpreter a sweep's cells exec: the deployment's boxed wrapper, else the interpreter itself. */
const interpreterOf = Effect.gen(function*() {
  const boxed = yield* Effect.flatMap(Boxing.Service, (boxing) => boxing.interpreter)
  if (boxed !== undefined) return boxed
  return yield* Effect.flatMap(Tools.Service, (tools) => tools.ready("interpreter"))
})

/**
 * The whole replay phase. The interpreter is brought up to date first, then
 * every capture's row is opened under one sweep row, every one of them read
 * as of ONE pinned prep, and the cells run in batches.
 *
 * The cells run under the deployment's cell services built HERE, once the
 * sweep has a number (`CellHost.services`): the bless audit is the sweep's
 * own log, so the reclassifier inside those services files into it and the
 * closing lines read it back; the interpreter is the boxed wrapper, so an
 * over-large cell costs one cell.
 *
 * The sweep row records the invocation for every caller; a request's replay
 * is a JOB besides, and its cells and progress go on the job row.
 */
export const phase = Effect.fn("Replay.phase")(function*(request: PhaseRequest) {
  const { captures, job, options, prepId, stop } = request
  const sweeps = yield* Sweeps.Service
  const runs = yield* Runs.Service
  const requests = yield* Requests.Service
  const jobs = yield* Jobs.Service
  const cellHost = yield* CellHost.Service
  const cells = cellsOf(captures)
  const sweep = yield* sweeps.open({
    lane: options.lane,
    universe: job === undefined ? options.selection.universe : "request",
    selection: job === undefined ? stated(options.selection) : `request ${job.resolution.requestId}`,
    captures: captures.length,
    concurrency: options.concurrency,
    batchSize: options.batchSize,
    memoryMax: options.memoryMax
  })
  if (job !== undefined) {
    yield* jobs.sweep(job.id, sweep)
    if (job.recorded !== true) yield* requests.recordJob(job.id, job.resolution)
  }
  // The BLESS AUDIT's log, named before any cell runs: every passing cell puts
  // the bless premise to itself and files what it finds against the RULE,
  // never against the cell.
  const audit = auditLog(options.paths.sessions, sweep)
  const fsys = yield* FileSystem.FileSystem
  yield* fsys.remove(audit, { force: true })
  const limits = yield* ceilings(options)
  const batches = Math.ceil(cells.length / options.batchSize)
  yield* Console.log(
    `[replay] sweep ${sweep}${job === undefined ? "" : ` (job ${job.id}, request ${job.resolution.requestId})`}: ` +
      `${captures.length} capture(s), ${cells.length} cell(s) on lane ${options.lane} from ` +
      `prep${prepId} in ${batches} batch(es) of ${options.batchSize} cell(s), ` +
      `${limits.cells} at once, each capped at ${options.memoryMax}`
  )
  // The build's lock is the third resource a job can wait on: the row says
  // whose build, as it says whose lane.
  const watch = (holder: string | undefined) =>
    job === undefined ? Effect.void : jobs.waiting(job.id, holder ?? null).pipe(Effect.ignore)
  yield* (request.binaries ?? ((w: Tools.Watch) =>
    Effect.flatMap(Tools.Service, (tools) => tools.ready("interpreter", w))))(watch)
  const interpreter = yield* interpreterOf

  const opened = yield* openRows(
    captures,
    options,
    prepId,
    sweep,
    job?.id,
    request.identity ?? ReplayIdentity.current,
    request.provenance ?? Provenance.current
  )
  // What has no cell to wait for closes now: a plan the store could not make,
  // a cut with nothing in it.
  const settled = new Map<string, Outcome>()
  const tallies = new Map<string, Tally>()
  for (const plan of captures) {
    const { capture } = plan
    const run = opened.get(capture)!
    if (plan._tag === "broke") {
      const detail = `${capture}  ERROR ${plan.broke}`
      yield* runs.finish({ id: run.id, status: "error", detail: plan.broke, cases: [] })
      yield* Console.error(detail)
      settled.set(capture, { capture, status: "error", detail })
      continue
    }
    if (plan._tag === "no-cases" || plan.cells.length === 0) {
      const why = plan._tag === "no-cases" ? plan.detail : "the set names no case"
      const detail = `${capture}  0 cases (${why})`
      yield* runs.finish({ id: run.id, status: "no-cases", detail: why, cases: [] })
      yield* Console.log(detail)
      settled.set(capture, { capture, status: "no-cases", detail })
      continue
    }
    tallies.set(capture, {
      plan,
      run,
      planned: plan.cells.length,
      landed: 0,
      summaries: [],
      broke: undefined,
      outcome: undefined
    })
  }

  const stopping = stop === undefined ? Effect.succeed(false) : Deferred.isDone(stop)
  const swept = yield* Effect.gen(function*() {
    const dirty: Array<string> = []
    const deadHosts: Array<string> = []
    for (let at = 0; at < cells.length; at += options.batchSize) {
      // A stop in before a batch starts: no batch starts.
      if (yield* stopping) break
      if (job !== undefined) yield* jobs.batch(job.id, Math.floor(at / options.batchSize) + 1, batches)
      const done = yield* batch({
        cells: cells.slice(at, at + options.batchSize),
        tallies,
        options,
        ...(stop === undefined ? {} : { stop }),
        ...(job === undefined ? {} : { ran: (cell: CellRef) => requests.ran(job.id, [cell]) }),
        ...(request.drive === undefined ? {} : { drive: request.drive }),
        ...(request.host === undefined ? {} : { host: request.host }),
        ...(request.sut === undefined ? {} : { sut: request.sut })
      })
      if (done.dirty !== undefined) {
        dirty.push(done.dirty)
        // The batch's gate answers for every run its cells ran under: each run row says so.
        const touched = [...new Set(done.driven.map((cell) => tallies.get(cell.capture)!.run.id))]
        yield* runs.dirty(touched, done.dirty)
      }
      if (done.deadHost !== undefined) deadHosts.push(done.deadHost)
    }
    return { dirty, deadHosts }
  }).pipe(Effect.provide(cellHost.services({ interpreter, audit })))
  // Stopped: every row still open closes on what landed under it.
  const interrupted = yield* stopping
  if (interrupted) {
    for (const tally of tallies.values()) {
      if (tally.outcome === undefined) yield* close(tally, options, true)
    }
  }
  yield* sweeps.finish(sweep)
  // Read back rather than accumulated: the cells append from wherever they run,
  // and a rule cited by one cell reads the same as one cited by four hundred.
  // The sweep is finished either way: a log that cannot be read is said so,
  // after every cell has already landed, not turned into the sweep's failure.
  for (const line of yield* cellHost.auditLines(audit)) yield* Console.log(line)
  return {
    sweep,
    job: job?.id,
    batches,
    interrupted,
    bundles: [...opened.values()].map((run) => run.bundlePath),
    outcomes: captures.map((plan) => settled.get(plan.capture) ?? tallies.get(plan.capture)!.outcome!),
    dirty: swept.dirty,
    deadHosts: swept.deadHosts
  } satisfies Replay
})

/**
 * The closing table, and the exit code that says whether anything broke.
 *
 * A batch whose SUT was still holding a call when it was given back fails the
 * invocation like an error does: the post-call cleanup check is never
 * advisory, only relocated to the scope that can ask it (the batch process).
 * So does a batch whose process died: its cells after that point failed on a
 * cause the batch, not the cases, is answerable for. A stopped replay exits
 * {@link INTERRUPTED_EXIT}, whatever else it found. A caller replaying what is
 * already known to fail (`--known` without `--strict`) states `errorsFail:
 * false`: its errored captures are the queue's own, and the summary says them.
 */
export const report = (replay: Replay, options: { readonly errorsFail?: boolean } = {}): Effect.Effect<number> =>
  Effect.gen(function*() {
    const by = (status: Outcome["status"]): number =>
      replay.outcomes.filter((outcome) => outcome.status === status).length
    yield* Console.log(
      `\n[replay] sweep ${replay.sweep}: ${replay.batches} batch(es), done=${by("done")} ` +
        `no-cases=${by("no-cases")} errors=${by("error")}` +
        (replay.interrupted ? ` interrupted=${by("interrupted")} — STOPPED before its last cell` : "") +
        (replay.dirty.length === 0 ? "" : ` dirty-batches=${replay.dirty.length}`) +
        (replay.deadHosts.length === 0 ? "" : ` dead-batch-processes=${replay.deadHosts.length}`)
    )
    if (replay.interrupted) return INTERRUPTED_EXIT
    const errors = (options.errorsFail ?? true) && by("error") > 0
    return errors || replay.dirty.length > 0 || replay.deadHosts.length > 0 ? 1 : 0
  })
