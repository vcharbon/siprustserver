/**
 * The replay phase of one batch of a lane: every cell's run-spec written under
 * the batch's directory's `specs/`, its bundle at `<cell id>/` under the lane's
 * run row's bundle (`record.ts`), the cells run under the lane's pacing and
 * their locks (`schedule.ts`), each judged once its lane ended (`judge.ts`)
 * and handed to `landed`.
 *
 * Every batch runs through the one batch driver (`@sip/corpus`
 * `replay/lane-batch`), its row filed under the run's campaign job, its
 * attempt its own (`batch.ts`); a quick run files none. The simulated lane
 * stands nothing: each cell runs as its own boxed replay process, its SUT
 * in-process. A hosted lane holds the lane (a box-wide singleton) and the
 * box's quiet for the batch, and the driver stands the lane's batch process
 * with the provisioned block the pack states, then a managed SIP target's
 * worker (with the variables the pack states), feeds it every cell, and judges
 * the cells once the batch's own end gate has charged them; a batch whose end
 * did not run to its end line (its bound, its death) leaves every cell an
 * error, not judged. A lane the box refuses (the pack's {@link LaneHosting}
 * `refusal`), or one lacking a capability a campaign needs, is the phase's
 * failure.
 *
 * The rig's batch-scope collectors the deployment wires on the lane
 * (`@sip/corpus` `Collectors`) bracket every batch, the simulated lane's too,
 * and read its cells described off their bundles, each with the claims the
 * pack reads of it; what they answer of a cell joins its judged result before
 * it is handed to `landed`, their attachments listed in its bundle. A close
 * that left the lab changed is the batch's dirty end. On a hosted lane each
 * cell's call runs through the batch's cell-scope collectors, its locks kept
 * while one of them holds them (`schedule.ts`); the simulated lane's clock is
 * simulated, so none runs there and its batch says so of each one wired.
 *
 * Each attempt of the lane's cells (`run.ts`) is a batch of its own, under
 * its own directory, each cell's bundle its own (`attempts.ts`).
 */
import type { LaneCatalog, LaneView } from "@sip/rig"
import { CollectedCells, CollectorIo, Collectors } from "@sip/corpus"
import * as LaneBatch from "@sip/corpus/replay/lane-batch"
import * as Session from "@sip/corpus/session"
import { JobEvents, Sessions } from "@sip/store"
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { batchDirOf, batchHolderOf, batchNameOf, bundleNameOf } from "./attempts.js"
import { inBatch, unjudgedOf } from "./batch.js"
import { type Cell, type CellResult, type Lane, withCollected } from "./cell.js"
import { exec } from "./exec.js"
import { judge, unjudgedAtEnd } from "./judge.js"
import { catalogLane, endpointAddresses, pacingOf, refusalOf, timingToleranceOf } from "./lanes.js"
import type { CampaignPack } from "./pack.js"
import { plannedOf } from "./planned.js"
import * as Results from "@sip/store/campaign-results"
import { type LaneOutcome, PhaseFailed } from "./run.js"
import { type Alone, schedule } from "./schedule.js"
import { aloneOn } from "./selection.js"

/** The memory box of one cell's replay process. */
export const CELL_MEMORY_MAX = "1G"

/** How long a run waits for a lane another run holds. */
const LANE_WAIT_MS = 900_000

type HostedOf<L extends LaneView.View> = Extract<L, { readonly kind: "hosted" }>

/**
 * How a pack hosts its lanes for a batch (`L` its lane view, `R` what its
 * replay tool's build needs): its lane ids' prefix, the capabilities a
 * campaign needs of a lane, what a cell's run-spec and a hosted batch state of
 * the lane, what it adds to a cell's bundle before the cell is judged, and the
 * box's replay tool and refusals.
 */
export interface LaneHosting<C extends Cell, L extends LaneView.View, R> {
  readonly prefix: string
  /** The capabilities a lane needs to run a campaign at all. */
  readonly needs: ReadonlyArray<string>
  /** The lane block of `cell`'s run-spec on `found`, its calls routed by the backend at `routing`. */
  readonly laneBlock: (found: L, routing: string, cell: C) => object
  /** The provisioned block a hosted lane's batch process stands with, toward `routing`. */
  readonly provisioned: (found: HostedOf<L>, routing: string) => unknown
  /** The variables a hosted lane's managed SUT is started with; none where it needs none. */
  readonly sutVariables: (found: HostedOf<L>) => Readonly<Record<string, string>> | undefined
  /** What the pack writes into a cell's bundle once its lane's cells ended, before it is judged; it reports its own failure. */
  readonly bundled: (cell: C, bundle: string) => Effect.Effect<void, never, FileSystem.FileSystem>
  /** The replay tool made ready. */
  readonly ready: Effect.Effect<unknown, Error, R>
  /** The command one simulated-lane cell's replay runs under (`<command> <run-spec>`). */
  readonly command: string
  /** The variables of one cell's replay box sized `memoryMax`. */
  readonly box: (memoryMax: string) => Effect.Effect<Readonly<Record<string, string>>>
  /** Why the box cannot drive `found` now, or null. */
  readonly refusal: (found: L) => Effect.Effect<string | null, never, FileSystem.FileSystem>
}

export interface Context<C extends Cell = Cell, L extends LaneView.View = LaneView.View> {
  /** The lanes the run reads its lanes off. */
  readonly catalog: LaneCatalog.Catalog<L>
  /** The run's directory. */
  readonly root: string
  /** The lane's run row's bundle: where every cell's bundle is written. */
  readonly bundles: string
  /** The backend the cells' calls are routed by (`host:port`). */
  readonly routing: string
  /** Each cell's result, once judged. */
  readonly landed: (result: CellResult<C>, startedAt: Date) => Effect.Effect<void>
  /** A quick run: the simulated lane only. */
  readonly quick: boolean
  /** The batch's attempt on the lane: 1 for its first, n for its cells' n-th replay. */
  readonly attempt: number
  /** The campaign job the run is, the owner of its batches' rows; none for a quick run. */
  readonly job: number | undefined
}

/** A cell placed for its batch: its run-spec, its bundle, its locks. */
export interface Placed<C extends Cell = Cell> {
  readonly cell: C
  readonly spec: string
  readonly bundle: string
  readonly locks: ReadonlyArray<string>
  readonly alone?: Alone
}

const failed = (lane: Lane, reason: string) => new PhaseFailed({ phase: `replay ${lane}`, reason })

const message = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause))

/** The run-spec of `cell` on `found`, its bundle written to `bundle`, its lane block the pack's toward `routing`. */
export const runSpecOf = <C extends Cell, L extends LaneView.View>(
  hosting: Pick<LaneHosting<C, L, unknown>, "laneBlock">,
  found: L,
  routing: string,
  cell: C,
  bundle: string
) => {
  const addresses = endpointAddresses(found, cell.endpoints)
  return {
    case: cell.dir,
    out_dir: bundle,
    lane: hosting.laneBlock(found, routing, cell),
    run: {
      timing_tolerance_ms: timingToleranceOf(found),
      ...(Object.keys(addresses).length === 0 ? {} : { endpoint_addresses: addresses })
    }
  }
}

/** Each cell's run-spec on `lane` (`found` in the catalog), written. */
const place = <C extends Cell, L extends LaneView.View>(
  hosting: Pick<LaneHosting<C, L, unknown>, "laneBlock">,
  lane: Lane,
  found: L,
  cells: ReadonlyArray<C>,
  context: Context<C, L>
) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const specs = path.join(batchDirOf(context.root, lane, context.attempt), "specs")
    yield* fs.makeDirectory(specs, { recursive: true })
    const placed: Array<Placed<C>> = []
    for (const cell of cells) {
      const spec = path.join(specs, `${cell.id}.json`)
      const bundle = path.join(context.bundles, bundleNameOf(cell, context.attempt))
      const runSpec = runSpecOf(hosting, found, context.routing, cell, bundle)
      yield* fs.writeFileString(spec, `${JSON.stringify(runSpec, null, 2)}\n`)
      const alone = aloneOn(found, cell)
      placed.push({ cell, spec, bundle, locks: cell.locks, ...(alone === undefined ? {} : { alone }) })
    }
    return placed as ReadonlyArray<Placed<C>>
  })

/** What a batch process reports of one cell's replay. */
interface HostReport {
  readonly exitCode: number
  readonly stdout: string
  readonly stderr: string
}

/** How one cell's replay process ended: its exit code and what it said. */
interface Exited {
  readonly code: number
  readonly summary: string
}

/** `p`'s cell as the rig's collectors of its batch on `found` read it, `failed` where its own outcome is a failure or an error. */
const describedOf = <C extends Cell>(
  pack: Pick<CampaignPack<C>, "claimsOf">,
  found: LaneView.View,
  p: Placed<C>,
  attempt: number,
  failed: boolean
): CollectedCells.Described => ({
  id: p.cell.id,
  caseId: Results.caseIdOf(p.cell.ref, p.cell.subcase),
  lane: found.id,
  dir: p.bundle,
  attempt,
  failed,
  claims: pack.claimsOf(p.cell)
})

/**
 * Whether `p`'s cell, its replay ended `exited`, failed or errored as its
 * batch's collectors see it: its judgement as it stands, the rig's
 * collectors still to come (`judge.ts`).
 */
const failedOwn = <C extends Cell>(pack: Pick<CampaignPack<C>, "typer">, lane: Lane, p: Placed<C>, exited: Exited) =>
  Effect.map(
    judge(pack.typer, lane, p.cell, exited.code, exited.summary, p.bundle),
    (result) => result.status === "failure" || result.status === "error"
  )

/**
 * The rig's collectors of the batch (`@sip/corpus` `Collectors`): those the
 * deployment wires on `found`, their directories under the `collectors/` of
 * the batch's holder in the session tree, which holds what they attach to it
 * (`attempts.ts` `batchHolderOf`), their events the run's job's, over
 * `placed`'s cells, each stated failed or not off how its replay ended
 * (`exitedOf`, in `placed`'s order), every one where the batch's end did not
 * judge them (`LaneBatch.failedAtEnd`), with the judges of the batch's
 * exports alone; none where it wires neither.
 */
export const rigCollectorsOf = <A, C extends Cell>(
  pack: Pick<CampaignPack<C>, "typer" | "claimsOf">,
  lane: Lane,
  found: LaneView.View,
  placed: ReadonlyArray<Placed<C>>,
  context: Pick<Context, "root" | "attempt" | "job">,
  exitedOf: (value: A) => ReadonlyArray<Exited>
) =>
  Effect.gen(function*() {
    const sessions = yield* Sessions.Service
    const holder = batchHolderOf({ root: context.root, sessions: sessions.dir, job: context.job }, lane, context.attempt)
    const batch = yield* Effect.flatMap(Collectors.Service, (seam) =>
      Collectors.batchOf(seam, found.id, path.join(holder, "collectors"), holder))
    if (!Collectors.wiresAny(batch)) return {}
    const events = yield* JobEvents.Service
    const collectors: LaneBatch.Collectors<A, FileSystem.FileSystem> = {
      instances: batch.instances,
      holder,
      options: {
        sink: CollectorIo.sinkOf(context.job === undefined ? undefined : { id: context.job, events }),
        locker: yield* CollectorIo.locker(),
        exportJudges: batch.exportJudges,
        planned: placed.map((p) => plannedOf(p.cell, found.id, pack.claimsOf(p.cell)))
      },
      cells: (value, end) =>
        Effect.flatMap(
          Effect.forEach(placed, (p, at) => (end.judged ? failedOwn(pack, lane, p, exitedOf(value)[at]!) : Effect.succeed(true))),
          (failed) =>
            CollectedCells.describe(
              placed.map((p, at) => describedOf(pack, found, p, context.attempt, failed[at]!)),
              batchNameOf(lane, context.attempt)
            )
        ),
      cell: batch.cell
    }
    return { collectors }
  })

/** `p`'s cell, its replay ended `exited`, as its cell-scope collectors read it once its call ended. */
const describedAlone = <C extends Cell>(
  pack: Pick<CampaignPack<C>, "typer" | "claimsOf">,
  lane: Lane,
  found: LaneView.View,
  p: Placed<C>,
  attempt: number,
  exited: Exited
) =>
  Effect.flatMap(
    failedOwn(pack, lane, p, exited),
    (failed) => CollectedCells.describe([describedOf(pack, found, p, attempt, failed)], batchNameOf(lane, attempt))
  )

/** What the batch driver refused or failed with, as the lane's phase failure. */
const driverFailed = (lane: Lane) => (e: unknown): PhaseFailed => (e instanceof PhaseFailed ? e : failed(lane, message(e)))

/** The time `effect` started at, beside what it came to. */
const timed = <A, E, R>(effect: Effect.Effect<A, E, R>) =>
  Effect.flatMap(Clock.currentTimeMillis, (at) => Effect.map(effect, (value) => ({ value, startedAt: new Date(at) })))

/** One cell's replay as it ended: its exit code, its summary, when it started. */
export interface Ended {
  readonly code: number
  readonly summary: string
  readonly startedAt: Date
}

/**
 * Each cell of `batch` judged once its lane's cells ended, the pack's
 * additions to its bundle written first (`LaneHosting.bundled`), what the
 * rig's collectors answered of it joined (`cell.ts` `withCollected`), and
 * handed to `landed`; every one an error where the batch's end did not run to
 * its end (`batch.ts` `unjudgedOf`).
 */
export const judgeAll = <C extends Cell>(
  pack: Pick<CampaignPack<C>, "typer">,
  hosting: Pick<LaneHosting<C, never, unknown>, "bundled">,
  lane: Lane,
  placed: ReadonlyArray<Placed<C>>,
  batch: LaneBatch.Ended<ReadonlyArray<Ended>>,
  context: Pick<Context<C>, "landed">
) => {
  const unjudged = unjudgedOf(batch)
  return Effect.forEach(placed, (p, i) => {
    const { code, summary, startedAt } = batch.value[i]!
    const judged = Effect.map(judge(pack.typer, lane, p.cell, code, summary, p.bundle), (result) =>
      withCollected(
        unjudged !== undefined ? unjudgedAtEnd(result, unjudged) : result,
        batch.collected.flatMap((one) => one.cells.get(p.cell.id)?.verifications ?? [])
      ))
    return Effect.tap(Effect.andThen(hosting.bundled(p.cell, p.bundle), judged), (result) => context.landed(result, startedAt))
  })
}

/**
 * What a batch left dirty: how its batch process died, what its end gate read
 * the SUT holding, each rig collector whose close left the lab changed.
 */
const dirtyOf = (batch: LaneBatch.Ended<unknown>): { readonly dirty?: string } => {
  const end = [batch.deadHost, batch.dirty, LaneBatch.leftChangedOf(batch)].filter((x): x is string => x !== undefined).join("; ")
  return end === "" ? {} : { dirty: end }
}

const simulated = <C extends Cell, L extends LaneView.View, R>(
  pack: Pick<CampaignPack<C>, "typer" | "claimsOf">,
  hosting: LaneHosting<C, L, R>,
  lane: Lane,
  found: L,
  placed: ReadonlyArray<Placed<C>>,
  context: Context<C, L>
) =>
  Effect.gen(function*() {
    const box = yield* hosting.box(CELL_MEMORY_MAX)
    const batch = yield* inBatch(context, placed.map((p) => p.cell), {
      lane: found.id,
      stand: undefined,
      ...(yield* rigCollectorsOf(pack, lane, found, placed, context, (ended: ReadonlyArray<Ended>) => ended)),
      drive: () =>
        Effect.map(
          schedule(placed, pacingOf(found), (p) =>
            Effect.map(
              timed(exec(hosting.command, [p.spec], box).pipe(
                Effect.catch((e) => Effect.succeed({ code: 3, stdout: "", stderr: `environment: ${e.message}` }))
              )),
              ({ value: ran, startedAt }): Ended => ({ code: ran.code, summary: `${ran.stdout}\n${ran.stderr}`, startedAt })
            )),
          (ended) => ({ value: ended, interrupted: 0 })
        )
    }).pipe(Effect.mapError(driverFailed(lane)))
    const results = yield* judgeAll(pack, hosting, lane, placed, batch, context)
    return { results, ...dirtyOf(batch) } satisfies LaneOutcome<C>
  })

/** The lane's cells replayed by one batch, its lane and the box held for the caller's scope. */
const replayed = <C extends Cell, L extends LaneView.View, R>(
  pack: Pick<CampaignPack<C>, "typer" | "claimsOf" | "verb">,
  hosting: LaneHosting<C, L, R>,
  lane: Lane,
  found: HostedOf<L>,
  placed: ReadonlyArray<Placed<C>>,
  context: Context<C, L>
) =>
  Effect.gen(function*() {
    const name = found.id
    const refused = yield* hosting.refusal(found)
    if (refused !== null) return yield* Effect.fail(failed(lane, refused))
    yield* Session.forReplay({ lane: name, patience: { waitMs: LANE_WAIT_MS } }, `${pack.verb} ${context.root}`).pipe(
      Effect.mapError((e) => failed(lane, e.message))
    )
    const pacing = pacingOf(found)
    const sutVariables = hosting.sutVariables(found)
    const batch = yield* inBatch(context, placed.map((p) => p.cell), {
      lane: name,
      stand: {
        under: batchDirOf(context.root, lane, context.attempt),
        memoryMax: CELL_MEMORY_MAX,
        cells: Math.min(placed.length, pacing.concurrency),
        provisioned: hosting.provisioned(found, context.routing),
        ...(sutVariables === undefined ? {} : { sutVariables })
      },
      ...(yield* rigCollectorsOf(
        pack,
        lane,
        found,
        placed,
        context,
        (reports: ReadonlyArray<{ readonly value: HostReport }>): ReadonlyArray<Exited> =>
          reports.map(({ value: report }) => ({ code: report.exitCode, summary: `${report.stdout}\n${report.stderr}` }))
      )),
      drive: ({ host, cells }) =>
        host === undefined
          ? Effect.fail(failed(lane, `${name} has no batch process`))
          : Effect.map(
            schedule(
              placed,
              pacing,
              (p) =>
                cells.around(p.cell.id, timed(host.run(p.spec)), ({ value: report }) =>
                  describedAlone(pack, lane, found, p, context.attempt, {
                    code: report.exitCode,
                    summary: `${report.stdout}\n${report.stderr}`
                  })),
              (ran) => ran.held
            ),
            (ran) => ({ value: ran.map((one) => one.value), interrupted: 0 })
          )
    }).pipe(Effect.mapError(driverFailed(lane)))
    return {
      ...batch,
      value: batch.value.map(({ value: report, startedAt }): Ended => ({
        code: report.exitCode,
        summary: `${report.stdout}\n${report.stderr}`,
        startedAt
      }))
    }
  })

/** The lane's batch replayed in a scope of its own, so its lane, the box and its processes are given back before its cells are judged. */
const hosted = <C extends Cell, L extends LaneView.View, R>(
  pack: Pick<CampaignPack<C>, "typer" | "claimsOf" | "verb">,
  hosting: LaneHosting<C, L, R>,
  lane: Lane,
  found: HostedOf<L>,
  placed: ReadonlyArray<Placed<C>>,
  context: Context<C, L>
) =>
  Effect.flatMap(Effect.scoped(replayed(pack, hosting, lane, found, placed, context)), (batch) =>
    Effect.map(
      judgeAll(pack, hosting, lane, placed, batch, context),
      (results) => ({ results, ...dirtyOf(batch) } satisfies LaneOutcome<C>)
    ))

/** Replay `cells` on `lane` under the pack's lane hosting. */
export const replayLane = <C extends Cell, L extends LaneView.View, R>(
  pack: Pick<CampaignPack<C>, "typer" | "claimsOf" | "verb">,
  hosting: LaneHosting<C, L, R>,
  lane: Lane,
  cells: ReadonlyArray<C>,
  context: Context<C, L>
) =>
  Effect.gen(function*() {
    const found = catalogLane(hosting.prefix, context.catalog, lane)
    if (found === undefined) return yield* Effect.fail(failed(lane, "the lane catalog holds no such lane"))
    if (context.quick && found.kind !== "simulated") return yield* Effect.fail(failed(lane, "--quick runs the simulated lane only"))
    const unfit = refusalOf(hosting.needs, found)
    if (unfit !== undefined) return yield* Effect.fail(failed(lane, unfit))
    yield* hosting.ready.pipe(Effect.mapError((e) => failed(lane, `the replay tool: ${message(e)}`)))
    const placed = yield* place(hosting, lane, found, cells, context).pipe(Effect.mapError((e) => failed(lane, message(e))))
    if (found.kind === "simulated") return yield* simulated(pack, hosting, lane, found, placed, context)
    return yield* hosted(pack, hosting, lane, found as HostedOf<L>, placed, context)
  })
