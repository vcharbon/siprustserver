/**
 * The rig's JOB-scope collectors around a job's work (`@sip/rig`
 * `CollectorRun`, ADR-0060 appendix §3), whoever runs the job (a capture
 * replay, a campaign): the ones the deployment wires over the job's lanes
 * (`Collectors.job`) open before the work, collect over every cell it ran to a
 * bundle once it ended, and close after it, whatever ended it: a failure, an
 * interruption, a signal to the runner. What they answered of a cell is
 * merged into the landed cell (`collected-cells.ts`), of the job itself are
 * the job's lines and attachments. Their events are the job's, where a
 * `lab-left-changed` one marks it.
 *
 * A cell the work ran twice (a repeated request's passes) is read once per
 * run, its later ids suffixed by the order it ran in.
 */
import { CollectorRun } from "@sip/rig"
import { Attachments, JobEvents, Verifications } from "@sip/store"
import * as Effect from "effect/Effect"
import * as path from "node:path"
import { Collectors } from "../deployment/index.js"
import * as CollectedCells from "./collected-cells.js"
import * as CollectorIo from "./collector-io.js"

/** What a job's collectors run around. */
export interface Request<A> {
  /** The job's row, where it is one: its events, lines and attachments are its own. */
  readonly job: number | undefined
  /** The job's directory: its collectors' directories under `collectors/`, its own attachments. */
  readonly dir: string
  readonly lanes: ReadonlyArray<string>
  /** The cells the work ran to a bundle, once it ended. */
  readonly cells: (value: A) => ReadonlyArray<CollectedCells.LandedCell>
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
    const instances = yield* seam.job(request.lanes, path.join(request.dir, "collectors"), request.dir)
    if (instances.length === 0) return yield* work
    const events = yield* JobEvents.Service
    const options: CollectorRun.Options = {
      sink: CollectorIo.sinkOf(request.job === undefined ? undefined : { id: request.job, events }),
      locker: yield* CollectorIo.locker()
    }
    return yield* Effect.scoped(Effect.gen(function*() {
      const opened = yield* CollectorRun.opened(instances, options)
      const value = yield* work
      const cells = distinct(request.cells(value))
      const collected = yield* opened.collect(yield* CollectedCells.describe(cells, path.basename(request.dir)))
      yield* CollectedCells.merge(CollectedCells.pendingOf(collected, cells))
      yield* answerJob(request, collected)
      return value
    }))
  })

/** What `collected` answered of the job itself, written as its lines and added to its attachments; nothing where it is no job. */
const answerJob = (request: Pick<Request<never>, "job" | "dir">, collected: ReadonlyArray<CollectorRun.Collected>) =>
  Effect.gen(function*() {
    if (request.job === undefined) return
    const verifications = collected.flatMap((one) => one.scope.verifications)
    const attachments = collected.flatMap((one) => one.scope.attachments)
    if (verifications.length > 0) yield* Effect.flatMap(Verifications.Service, (store) => store.writeJob(request.job!, verifications))
    if (attachments.length > 0) {
      yield* Effect.flatMap(Attachments.Service, (store) => store.add({ scope: "job", jobId: request.job! }, { dir: request.dir, attachments }))
    }
  }).pipe(Effect.catchCause(() => Effect.logError(`job ${request.job}: the store refused its collectors' answers`)))
