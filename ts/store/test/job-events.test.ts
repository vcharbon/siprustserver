/**
 * Job events against a real postgres: a job's events read back in the order
 * they were appended, each on both clocks; the event of phase
 * `lab-left-changed` marks its job alone; a job's events go with it. The suite
 * builds and drops its own database.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as JobEvents from "../src/job-events.js"
import * as Jobs from "../src/jobs.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_job_events_test")

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(Server.urlFor(database)))))

const withStore = <A, E>(effect: Effect.Effect<A, E, JobEvents.Service | Jobs.Service | SqlClient>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(JobEvents.layer),
      Effect.provide(Jobs.layer),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

beforeAll(async () => {
  if (!Server.served) return
  await on(
    "postgres",
    Effect.gen(function*() {
      const sql = yield* SqlClient
      yield* Server.dropDatabase(SCRATCH)
      yield* sql.unsafe(`CREATE DATABASE ${SCRATCH}`)
    })
  )
  await on(SCRATCH, Server.migrated)
}, 60_000)

afterAll(async () => {
  if (!Server.served) return
  await on("postgres", Server.dropDatabase(SCRATCH))
}, 60_000)

const campaignJob = Effect.flatMap(Jobs.Service, (jobs) =>
  jobs.create({ kind: "campaign", args: { capture: "c", lanes: ["paused"] }, createdBy: "test" }))

const WALL = Date.parse("2026-11-11T12:00:00.250Z")

describe.skipIf(!Server.served)("JobEvents", () => {
  it("reads a job's events back in the order appended, each on the runner's clock and the wall clock", () =>
    withStore(Effect.gen(function*() {
      const events = yield* JobEvents.Service
      const job = yield* campaignJob
      const other = yield* campaignJob
      yield* events.append(job, { subject: "restart-worker", phase: "inject", clockMs: 1000.5, wallMs: WALL })
      yield* events.append(other, { subject: "freeze", phase: "inject", clockMs: 7, wallMs: WALL })
      yield* events.append(job, {
        subject: "restart-worker",
        phase: "failed",
        clockMs: 2000,
        wallMs: WALL + 1000,
        message: "heal exited 1"
      })
      expect((yield* events.ofJob(job)).map((row) => [row.jobId, row.subject, row.phase, row.clockMs, row.at, row.message])).toEqual([
        [job, "restart-worker", "inject", 1000.5, "2026-11-11T12:00:00.250Z", null],
        [job, "restart-worker", "failed", 2000, "2026-11-11T12:00:01.250Z", "heal exited 1"]
      ])
    })))

  it("marks the job whose runner left its lab changed, and no other", () =>
    withStore(Effect.gen(function*() {
      const events = yield* JobEvents.Service
      const job = yield* campaignJob
      const other = yield* campaignJob
      yield* events.append(other, { subject: "freeze", phase: "failed", clockMs: 1, wallMs: WALL })
      expect(yield* events.labLeftChanged(job)).toBe(false)
      yield* events.append(job, { subject: "freeze", phase: JobEvents.LAB_LEFT_CHANGED, clockMs: 2, wallMs: WALL })
      expect([yield* events.labLeftChanged(job), yield* events.labLeftChanged(other)]).toEqual([true, false])
    })))

  it("drops a job's events with the job", () =>
    withStore(Effect.gen(function*() {
      const events = yield* JobEvents.Service
      const sql = yield* SqlClient
      const job = yield* campaignJob
      yield* events.append(job, { subject: "freeze", phase: "inject", clockMs: 1, wallMs: WALL })
      yield* sql`DELETE FROM jobs WHERE id = ${job}`
      expect(yield* events.ofJob(job)).toEqual([])
    })))
})
