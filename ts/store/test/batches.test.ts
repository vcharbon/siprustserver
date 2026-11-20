/**
 * Batches against a real postgres: each batch a job drives is a row of its
 * own, its window open until its end and closed with what the end said; a
 * retry is a row of its own; a sweep that is no job owns its batches; a batch
 * names its owner, and goes with it. The suite builds and drops its own
 * database.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Batches from "../src/batches.js"
import * as Client from "../src/client.js"
import * as Jobs from "../src/jobs.js"
import * as Sweeps from "../src/sweeps.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_batches_test")

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(Server.urlFor(database)))))

type Store = Batches.Service | Jobs.Service | Sweeps.Service | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Store>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Batches.layer),
      Effect.provide(Jobs.layer),
      Effect.provide(Sweeps.layer),
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

describe.skipIf(!Server.served)("Batches", () => {
  it("writes one row per batch of a job, its window open until its end, a retry as a row of its own", () =>
    withStore(Effect.gen(function*() {
      const batches = yield* Batches.Service
      const job = yield* campaignJob
      const first = yield* batches.open({ jobId: job, lane: "paused", cells: ["a", "b", "c"] })
      const open = yield* batches.ofJob(job)
      expect(open).toHaveLength(1)
      expect(open[0]).toMatchObject({ id: first, jobId: job, sweepId: null, lane: "paused", attempt: 1, cells: ["a", "b", "c"] })
      expect(open[0]!.finishedAt).toBeNull()
      yield* batches.close(first, null)
      const retry = yield* batches.open({ jobId: job, lane: "paused", attempt: 2, cells: ["b"] })
      yield* batches.close(retry, "the SUT still held a call")
      const rows = yield* batches.ofJob(job)
      expect(rows.map((row) => [row.id, row.attempt, row.cells, row.detail])).toEqual([
        [first, 1, ["a", "b", "c"], null],
        [retry, 2, ["b"], "the SUT still held a call"]
      ])
      for (const row of rows) {
        expect(row.finishedAt).not.toBeNull()
        expect(Date.parse(row.finishedAt!)).toBeGreaterThanOrEqual(Date.parse(row.startedAt))
      }
      expect(Date.parse(rows[1]!.startedAt)).toBeGreaterThanOrEqual(Date.parse(rows[0]!.finishedAt!))
    })))

  it("files a replay's batches under its sweep where it is no job", () =>
    withStore(Effect.gen(function*() {
      const batches = yield* Batches.Service
      const sweeps = yield* Sweeps.Service
      const sweep = yield* sweeps.open({
        lane: "paused",
        universe: "named",
        selection: "x",
        captures: 1,
        concurrency: 1,
        batchSize: 2,
        memoryMax: "1G"
      })
      yield* batches.open({ sweepId: sweep, lane: "paused", cells: ["x#1", "x#2"] })
      yield* batches.open({ sweepId: sweep, lane: "paused", cells: ["x#3"] })
      expect((yield* batches.ofSweep(sweep)).map((row) => [row.jobId, row.cells])).toEqual([
        [null, ["x#1", "x#2"]],
        [null, ["x#3"]]
      ])
    })))

  it("closes a gone owner's open batches as abandoned, leaving its ended ones and other owners' as they were", () =>
    withStore(Effect.gen(function*() {
      const batches = yield* Batches.Service
      const sweeps = yield* Sweeps.Service
      const job = yield* campaignJob
      const other = yield* campaignJob
      const ended = yield* batches.open({ jobId: job, lane: "paused", cells: ["a"] })
      yield* batches.close(ended, null)
      const cut = yield* batches.open({ jobId: job, lane: "paused", attempt: 2, cells: ["a"] })
      const running = yield* batches.open({ jobId: other, lane: "paused", cells: ["z"] })
      expect(yield* batches.abandon({ jobId: job }, "abandoned: gone")).toEqual([cut])
      expect((yield* batches.ofJob(job)).map((row) => [row.id, row.detail, row.finishedAt !== null])).toEqual([
        [ended, null, true],
        [cut, "abandoned: gone", true]
      ])
      expect((yield* batches.ofJob(other)).map((row) => [row.id, row.finishedAt])).toEqual([[running, null]])
      const sweep = yield* sweeps.open({
        lane: "paused",
        universe: "named",
        selection: "x",
        captures: 1,
        concurrency: 1,
        batchSize: 2,
        memoryMax: "1G"
      })
      const swept = yield* batches.open({ sweepId: sweep, lane: "paused", cells: ["x#1"] })
      expect(yield* batches.abandon({ sweepId: sweep }, "abandoned: gone")).toEqual([swept])
      expect((yield* batches.ofJob(other)).map((row) => row.finishedAt)).toEqual([null])
    })))

  it("refuses a batch with no owner, and drops a job's batches with the job", () =>
    withStore(Effect.gen(function*() {
      const batches = yield* Batches.Service
      const sql = yield* SqlClient
      const orphan = yield* Effect.result(sql`INSERT INTO batches (lane, cells) VALUES ('paused', '{}')`)
      expect(orphan._tag).toBe("Failure")
      const job = yield* campaignJob
      yield* batches.open({ jobId: job, lane: "paused", cells: ["a"] })
      yield* sql`DELETE FROM jobs WHERE id = ${job}`
      expect(yield* batches.ofJob(job)).toEqual([])
    })))
})
