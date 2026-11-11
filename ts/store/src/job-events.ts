/**
 * A job's EVENTS: what its runner did to the lab while the job ran, one row
 * per phase of one subject (an operation injected, healed, proved recovered,
 * or failed), at its instant on the runner's clock and on the wall clock, in
 * the order the runner wrote them. A job whose lab was left changed (a heal
 * that failed twice) holds an event of phase {@link LAB_LEFT_CHANGED}: that
 * event is the job's mark.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

/** The phase of the event that marks a job whose lab its runner left changed. */
export const LAB_LEFT_CHANGED = "lab-left-changed"

export interface Event {
  /** What the event is of: an operation, and what it ran for. */
  readonly subject: string
  readonly phase: string
  /** Milliseconds on the runner's clock. */
  readonly clockMs: number
  /** Unix milliseconds. */
  readonly wallMs: number
  /** What failed, where something did. */
  readonly message?: string | undefined
}

export interface Row {
  readonly id: number
  readonly jobId: number
  readonly subject: string
  readonly phase: string
  readonly clockMs: number
  /** The wall clock instant, ISO. */
  readonly at: string
  readonly message: string | null
}

export interface Interface {
  /** `event` appended to the job `jobId`'s events. */
  readonly append: (jobId: number, event: Event) => Effect.Effect<void, SqlError>
  /** The job's events, in the order they were appended. */
  readonly ofJob: (jobId: number) => Effect.Effect<ReadonlyArray<Row>, SqlError>
  /** Whether the job's runner left its lab changed: an event of {@link LAB_LEFT_CHANGED} is on it. */
  readonly labLeftChanged: (jobId: number) => Effect.Effect<boolean, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/JobEvents") {}

interface EventRow {
  readonly id: string
  readonly job_id: string
  readonly subject: string
  readonly phase: string
  readonly clock_ms: number
  readonly at: Date
  readonly message: string | null
}

const rowOf = (row: EventRow): Row => ({
  id: Number(row.id),
  jobId: Number(row.job_id),
  subject: row.subject,
  phase: row.phase,
  clockMs: Number(row.clock_ms),
  at: row.at.toISOString(),
  message: row.message
})

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const append = Effect.fn("JobEvents.append")(function*(jobId: number, event: Event) {
      yield* sql`
        INSERT INTO job_events (job_id, subject, phase, clock_ms, at, message)
        VALUES (${jobId}, ${event.subject}, ${event.phase}, ${event.clockMs},
                to_timestamp(${event.wallMs}::double precision / 1000), ${event.message ?? null})
      `
    })

    const ofJob = Effect.fn("JobEvents.ofJob")(function*(jobId: number) {
      const rows = yield* sql<EventRow>`SELECT * FROM job_events WHERE job_id = ${jobId} ORDER BY id`
      return rows.map(rowOf)
    })

    const labLeftChanged = Effect.fn("JobEvents.labLeftChanged")(function*(jobId: number) {
      const rows = yield* sql<{ marked: boolean }>`
        SELECT EXISTS (SELECT 1 FROM job_events WHERE job_id = ${jobId} AND phase = ${LAB_LEFT_CHANGED}) AS marked
      `
      return rows[0]?.marked === true
    })

    return Service.of({ append, ofJob, labLeftChanged })
  })
)
