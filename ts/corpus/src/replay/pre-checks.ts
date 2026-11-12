/**
 * A job's pre-checks (`@sip/rig` `PreCheck`, ADR-0060 appendix §3), whoever
 * runs the job (a capture replay, a campaign): the job-scope collectors `at =
 * "before"` the deployment wires over the job's lanes, run before the job's
 * provisioning and its first call, and the `on-demand` ones the job asks for
 * by name (a capture replay asks for none). What they answer of the job are its lines
 * and attachments, after any it holds; their events are the job's. A refusal
 * of theirs (`PreCheck.run`) refuses the job with its message, as does a
 * required pre-check, of either scope, that a real-clock lane of the job
 * names no part of, and one the job asks for that a lane excludes.
 *
 * A capture replay runs them once its lane is held ({@link forReplay}, from
 * `build.ts` `session`), so nothing it checked changes before its first call.
 * A batch's own pre-checks run in the batch driver (`lane-batch.ts`).
 */
import type { Attachment, Verification } from "@sip/contracts"
import { PreCheck } from "@sip/rig"
import { Attachments, JobEvents, Verifications } from "@sip/store"
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { Collectors } from "../deployment/index.js"
import * as Locks from "../locks.js"
import * as CollectedCells from "./collected-cells.js"
import * as CollectorIo from "./collector-io.js"

/** What a job's pre-checks run for. */
export interface Request {
  /** The job's row, where it is one: its events, lines and attachments are its own. */
  readonly job: number | undefined
  /** The job's directory: its pre-checks' directories under `collectors/`, its own attachments. */
  readonly dir: string
  readonly lanes: ReadonlyArray<string>
  /** The `on-demand` pre-checks the job asks for by name, guarding what it alone does; none where absent. */
  readonly asked?: ReadonlyArray<string>
  /** Told whom a pre-check's lock waits for, and `undefined` once it is taken. */
  readonly watch?: Locks.Watch
}

/** A job's pre-checks refused it: `reason` says why. */
export class Refused extends Schema.TaggedError<Refused>()("PreChecks.Refused", { reason: Schema.String }) {
  override get message(): string {
    return this.reason
  }
}

/** `lines` written after those job `job` holds; nothing where none is given. */
export const appendJobLines = (job: number, lines: ReadonlyArray<Verification.Verification>) =>
  lines.length === 0 ? Effect.void : Effect.flatMap(Verifications.Service, (store) =>
    Effect.flatMap(store.of({ scope: "job", jobId: job }), (held) => store.writeJob(job, [...held, ...lines])))

/** `answers` as job `job`'s lines and attachments, `dir` holding the attached files; a store refusing them is said, never the job's failure. */
const answerJob = (
  job: number,
  dir: string,
  answers: { readonly verifications: ReadonlyArray<Verification.Verification>; readonly attachments: ReadonlyArray<Attachment.Attachment> }
) =>
  Effect.gen(function*() {
    yield* appendJobLines(job, answers.verifications)
    if (answers.attachments.length > 0) {
      yield* Effect.flatMap(Attachments.Service, (store) => store.add({ scope: "job", jobId: job }, { dir, attachments: answers.attachments }))
    }
  }).pipe(Effect.catchCause(() => Effect.logError(`job ${job}: the store refused its pre-checks' answers`)))

/** Runs the job's pre-checks: why the job is refused, or `undefined` where it is not or none is wired. */
export const job = (request: Request) =>
  Effect.gen(function*() {
    const seam = yield* Collectors.Service
    const instances = (yield* seam.job(request.lanes, path.join(request.dir, "collectors"), request.dir, request.asked))
      .filter(PreCheck.isPreCheck)
    const unwired = yield* seam.unwired(request.lanes, request.asked)
    if (instances.length === 0 && unwired.length === 0) return undefined
    const events = yield* JobEvents.Service
    const checked = yield* PreCheck.run(instances, unwired, {
      sink: CollectorIo.sinkOf(request.job === undefined ? undefined : { id: request.job, events }),
      locker: yield* CollectorIo.locker(Locks.DEFAULT_WAIT_MS, request.watch)
    })
    if (request.job !== undefined) yield* answerJob(request.job, request.dir, checked.answers)
    return checked.refusal
  })

/** The job row a replay files its pre-checks under, and the job's directory. */
export interface Filed {
  readonly id: number
  readonly dir: string
}

/** The directory, under `sessions`' collectors, of one run of the pre-checks of work no job row files. */
const unfiledDir = (sessions: string) =>
  Effect.map(Clock.currentTimeMillis, (ms) => path.join(sessions, CollectedCells.COLLECTORS_DIR, `pre-checks-${ms}-${process.pid}`))

/**
 * The pre-checks of a capture replay on `lane`, run once its lane is held:
 * those of job `filed`, its directory holding theirs; of work no job row files
 * (a sweep, a campaign's or an evidence run's replay), under `sessions`'
 * collectors, in a directory removed once they let the work through. Fails
 * {@link Refused} with why they refused it.
 */
export const forReplay = (sessions: string, lane: string, filed: Filed | undefined, watch?: Locks.Watch) =>
  Effect.gen(function*() {
    const dir = filed?.dir ?? (yield* unfiledDir(sessions))
    const refused = yield* job({ job: filed?.id, dir, lanes: [lane], ...(watch === undefined ? {} : { watch }) })
    if (refused !== undefined) return yield* new Refused({ reason: refused })
    if (filed === undefined) yield* Effect.ignore(Effect.flatMap(FileSystem.FileSystem, (files) => files.remove(dir, { recursive: true })))
  })
