/**
 * An ON-DEMAND run of one collector (ADR-0060 appendix §5) over landed cells
 * the site or the CLI names, long after their job ended: one scope per lane,
 * the instance the deployment plans for it there (`Collectors.onDemand`),
 * opened, collected over the lane's cells described off their bundles, and
 * closed (`@sip/rig` `CollectorOnDemand`).
 *
 * What it attaches to a cell lands on the cell (`@sip/store` `Attachments`),
 * an attachment of the same name replaced. Its verification lines are said to
 * the caller and never merged into the cell: a question asked after the job
 * ended does not change the cell's verdict.
 */
import { CollectorOnDemand, type CollectorRun } from "@sip/rig"
import { Attachments } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import { randomUUID } from "node:crypto"
import * as path from "node:path"
import { Collectors, type Roots } from "../deployment/index.js"
import * as CollectedCells from "./collected-cells.js"
import * as CollectorIo from "./collector-io.js"

/** Why an on-demand run did not start: no cell named, or a lane the collector cannot run on demand on. */
export class Refused extends Schema.TaggedError<Refused>()("OnDemand.Refused", { reason: Schema.String }) {
  override get message(): string {
    return this.reason
  }
}

/** What an on-demand run is asked. */
export interface Request {
  /** The collector's name. */
  readonly collector: string
  /** The cells it reads, each landed under its run row. */
  readonly cells: ReadonlyArray<CollectedCells.LandedCell>
  /**
   * Where it makes its own directory, one per ask (`ask-<unique>`), each
   * lane's instance under its `<lane>/`, removed once the attachments were
   * copied onto their cells: two asks at once never share one. It is kept
   * where a step failed or a `close` left the lab changed, so the log its
   * message names stays readable.
   */
  readonly dir: string
}

/** The prefix of an ask's own directory under {@link Request.dir}. */
export const ASK_PREFIX = "ask-"

/** What it answered of one cell. */
export interface CellAnswered {
  readonly cell: CollectedCells.LandedCell
  readonly answers: CollectorRun.Answers
}

/** What an on-demand run came to. */
export interface Ran {
  /** Per cell named, in their order, what it answered of it. */
  readonly cells: ReadonlyArray<CellAnswered>
  /** The labels of the instances whose `close` left the lab changed. */
  readonly leftChanged: ReadonlyArray<string>
}

const NO_ANSWERS: CollectorRun.Answers = { verifications: [], attachments: [] }

/** `cells` grouped by lane, the lanes in the order their first cell is named. */
const byLane = (cells: ReadonlyArray<CollectedCells.LandedCell>): ReadonlyMap<string, ReadonlyArray<CollectedCells.LandedCell>> => {
  const lanes = new Map<string, Array<CollectedCells.LandedCell>>()
  for (const cell of cells) lanes.set(cell.lane, [...(lanes.get(cell.lane) ?? []), cell])
  return lanes
}

/** `answered`'s attachments landed on their cells, a refusal of the store said. */
const land = (answered: ReadonlyArray<CellAnswered>) =>
  Effect.flatMap(Attachments.Service, (store) =>
    Effect.forEach(answered, ({ cell, answers }) =>
      answers.attachments.length === 0
        ? Effect.void
        : store.add({ scope: "cell", runId: cell.runId, caseId: cell.caseId }, { dir: cell.dir, attachments: answers.attachments }).pipe(
          Effect.catchCause(() => Console.error(`[collector] run ${cell.runId}: the store refused the attachments of ${cell.caseId}`))
        ), { discard: true }))

/** `request` run: its refusal where a lane cannot run the collector on demand, every lane checked before any runs. */
export const run = (
  request: Request
): Effect.Effect<Ran, Refused, Collectors.Service | Attachments.Service | FileSystem.FileSystem | Roots.Service> =>
  Effect.gen(function*() {
    if (request.cells.length === 0) return yield* new Refused({ reason: `no cell named for ${request.collector}` })
    const seam = yield* Collectors.Service
    const fs = yield* FileSystem.FileSystem
    const asked = path.join(request.dir, `${ASK_PREFIX}${randomUUID()}`)
    const planned: Array<readonly [CollectorRun.Instance, ReadonlyArray<CollectedCells.LandedCell>]> = []
    for (const [lane, cells] of byLane(request.cells)) {
      const dir = path.join(asked, lane)
      const instance = yield* seam.onDemand(request.collector, lane, path.join(dir, "collectors"), dir)
      if (Result.isFailure(instance)) return yield* new Refused({ reason: instance.failure })
      planned.push([instance.success, cells])
    }
    yield* fs.makeDirectory(request.dir, { recursive: true }).pipe(Effect.orDie)
    yield* fs.makeDirectory(asked).pipe(Effect.orDie)
    const failing = { seen: false }
    const sink = CollectorIo.sinkOf(undefined)
    const watched: CollectorRun.Sink = (event) =>
      Effect.andThen(Effect.sync(() => void (failing.seen ||= FAILING.has(event.phase))), sink(event))
    return yield* ranOver(request, planned, watched).pipe(
      Effect.ensuring(Effect.suspend(() => failing.seen ? Effect.void : Effect.ignore(fs.remove(asked, { recursive: true }))))
    )
  })

/** The phases that say a step failed, its log kept under the ask's directory. */
const FAILING: ReadonlySet<CollectorRun.Phase> = new Set(["failed", "lab-left-changed"])

/** `planned`, each lane's instance and its cells, run one after the other under `sink`, their attachments landed on their cells. */
const ranOver = (
  request: Request,
  planned: ReadonlyArray<readonly [CollectorRun.Instance, ReadonlyArray<CollectedCells.LandedCell>]>,
  sink: CollectorRun.Sink
) =>
  Effect.gen(function*() {
    const options: CollectorRun.Options = { sink, locker: yield* CollectorIo.locker() }
    const answered: Array<CellAnswered> = []
    const leftChanged: Array<string> = []
    for (const [instance, cells] of planned) {
      const over = yield* CollectedCells.describe(cells, `${request.collector}-on-demand`)
      const ran = yield* CollectorOnDemand.run([instance], over, options)
      for (const cell of cells) {
        const answers = ran.collected.map((one) => one.cells.get(cell.id) ?? NO_ANSWERS)
        answered.push({
          cell,
          answers: {
            verifications: answers.flatMap((one) => one.verifications),
            attachments: answers.flatMap((one) => one.attachments)
          }
        })
      }
      leftChanged.push(...ran.leftChanged)
    }
    yield* land(answered)
    return { cells: answered, leftChanged }
  })
