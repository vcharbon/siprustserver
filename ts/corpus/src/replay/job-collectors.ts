/**
 * The rig's JOB-scope collectors around a job's work (`@sip/rig`
 * `CollectorRun`, ADR-0060 appendix §3), whoever runs the job (a capture
 * replay, a campaign): the ones the deployment wires over the job's lanes
 * (`Collectors.job`) open before the work, collect over every cell it ran to a
 * bundle once it ended, and close after it, whatever ended it: a failure, an
 * interruption, a signal to the runner. What they answered of a cell is
 * merged into the landed cell (`collected-cells.ts`), or handed to a caller
 * that lands its cells after them; of the job itself are the job's lines,
 * after those its pre-checks answered (`pre-checks.ts`, which run them), and
 * attachments. Their events are the job's, where a `lab-left-changed` one
 * marks it, and the caller is told who left the lab changed.
 *
 * A cell the work ran twice (a repeated request's passes) is read once per
 * run, its later ids suffixed by the order it ran in.
 */
import { CollectorRun, type Context, PreCheck } from "@sip/rig"
import { Attachments, JobEvents } from "@sip/store"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import { identity } from "effect/Function"
import * as path from "node:path"
import { Collectors } from "../deployment/index.js"
import * as Locks from "../locks.js"
import * as CollectedCells from "./collected-cells.js"
import * as CollectorIo from "./collector-io.js"
import * as PreChecks from "./pre-checks.js"

/** What a job's collectors run around. */
export interface Request<A> {
  /** The job's row, where it is one: its events, lines and attachments are its own. */
  readonly job: number | undefined
  /**
   * The directory of work no job row files: its collectors' directories under
   * `collectors/`. A job's are under its own directory in the session tree,
   * with its attachments (`CollectorIo.jobHolderOf`).
   */
  readonly dir: string
  readonly lanes: ReadonlyArray<string>
  /** The cells the work ran to a bundle, once it ended. */
  readonly cells: (value: A) => ReadonlyArray<CollectedCells.LandedCell>
  /** The cells the work is to run, which every collector step's context states (`planned`); none stated where absent. */
  readonly planned?: ReadonlyArray<Context.Planned>
  /** Makes the opening (each lock wait, each `open`) interruptible where the caller runs `around` masked. */
  readonly restore?: <X, EX, RX>(effect: Effect.Effect<X, EX, RX>) => Effect.Effect<X, EX, RX>
  /** Told whom a collector's lock waits for, and `undefined` once it is taken. */
  readonly watch?: Locks.Watch
  /**
   * Where what the collectors answered of the cells goes, `cells` as they read
   * them (ids made distinct), in the order `cells` gave them; merged into each
   * landed cell where absent. A caller that lands its cells after `around`
   * folds them in itself.
   */
  readonly answered?: (cells: ReadonlyArray<CollectedCells.LandedCell>, collected: ReadonlyArray<CollectorRun.Collected>) => Effect.Effect<void>
  /** Told, once every collector closed however the work ended, the labels of those whose `close` left the lab changed, where any did. */
  readonly leftChanged?: (labels: ReadonlyArray<string>) => Effect.Effect<void>
}

/** `cells` with each id unique among them: a repeat's id suffixed `~<n>`, its n-th run. */
export const distinct = (cells: ReadonlyArray<CollectedCells.LandedCell>): ReadonlyArray<CollectedCells.LandedCell> => {
  const seen = new Map<string, number>()
  return cells.map((cell) => {
    const n = (seen.get(cell.id) ?? 0) + 1
    seen.set(cell.id, n)
    return n === 1 ? cell : { ...cell, id: `${cell.id}~${n}` }
  })
}

/** `work` with the job's collectors around it; the work itself where the deployment wires none. */
export const around = <A, E, R>(request: Request<A>, work: Effect.Effect<A, E, R>) =>
  Effect.gen(function*() {
    const seam = yield* Collectors.Service
    const dir = yield* CollectorIo.jobHolderOf(request.job, request.dir)
    const instances = (yield* seam.job(request.lanes, path.join(dir, "collectors"), dir)).filter((one) => !PreCheck.isPreCheck(one))
    if (instances.length === 0) return yield* work
    const events = yield* JobEvents.Service
    const options: CollectorRun.Options = {
      sink: CollectorIo.sinkOf(request.job === undefined ? undefined : { id: request.job, events }),
      locker: yield* CollectorIo.locker(Locks.DEFAULT_WAIT_MS, request.watch),
      planned: request.planned
    }
    const restore = request.restore ?? identity
    let opened: CollectorRun.Opened | undefined
    const leftChanged = Effect.suspend(() => {
      const labels = opened?.leftChanged() ?? []
      return labels.length === 0 || request.leftChanged === undefined ? Effect.void : request.leftChanged(labels)
    })
    return yield* Effect.scoped(Effect.gen(function*() {
      const scope = yield* restore(CollectorRun.opened(instances, options))
      opened = scope
      const value = yield* work
      const cells = distinct(request.cells(value))
      const over = yield* CollectedCells.describe(cells, path.basename(dir))
      const collected = yield* scope.collect(over)
      yield* request.answered === undefined
        ? CollectedCells.merge(CollectedCells.pendingOf(collected, cells))
        : request.answered(cells, collected)
      yield* answerJob(request.job, dir, collected)
      return value
    })).pipe(Effect.ensuring(leftChanged))
  })

/**
 * What `collected` answered of job `job` itself, written after its lines and
 * added to its attachments, `dir` holding their files; nothing where it is no
 * job. A store refusing them is said with why, never the job's failure.
 */
const answerJob = (job: number | undefined, dir: string, collected: ReadonlyArray<CollectorRun.Collected>) =>
  Effect.gen(function*() {
    if (job === undefined) return
    const verifications = collected.flatMap((one) => one.scope.verifications)
    const attachments = collected.flatMap((one) => one.scope.attachments)
    yield* PreChecks.appendJobLines(job, verifications)
    if (attachments.length > 0) {
      yield* Effect.flatMap(Attachments.Service, (store) => store.add({ scope: "job", jobId: job }, { dir, attachments }))
    }
  }).pipe(Effect.catchCause((cause) => Effect.logError(`job ${job}: the store refused its collectors' answers: ${Cause.pretty(cause)}`)))
