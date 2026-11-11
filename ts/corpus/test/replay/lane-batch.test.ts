/**
 * One batch on a lane, against a real postgres and a lane that stands a fake
 * batch process and SUT: the lane stands before the collectors open, the
 * cells run on what it stood, the end gate reads before the collectors close
 * and both are given back last; a dead process takes no gate and leaves the
 * batch unjudged; a lane that stands nothing still brackets its cells; a
 * batch that fails still closes its row; a retry is a row of its own.
 *
 * The suite builds and drops its OWN database (`@sip/store/test-server`).
 */
import { Batches, Client, Jobs } from "@sip/store"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { LaneHosting } from "../../src/deployment/index.js"
import type { BatchHost, SutHandle } from "../../src/replay/batch-protocol.js"
import * as LaneBatch from "../../src/replay/lane-batch.js"

const SCRATCH = await scratch("corpus_lane_batch_test")

const STANDS = "stands"
const PAUSED = "paused"

let events: Array<string> = []
let hostDied: string | undefined

const host: BatchHost = {
  run: () => Effect.die("unused"),
  died: () => hostDied,
  end: Effect.sync(() => {
    events.push("gate")
    return undefined
  }),
  contexts: () => undefined,
  kill: Effect.sync(() => void events.push("killed")),
  judged: () => true
}

const sut: SutHandle = {
  metrics: undefined,
  died: () => undefined,
  stop: Effect.sync(() => void events.push("sut stopped"))
}

const hosting = LaneHosting.layerWith({
  lanes: [STANDS, PAUSED],
  lane: () => Effect.succeed(LaneHosting.NEUTRAL_LANE),
  standBatch: (lane, request) =>
    Effect.sync(() => {
      events.push(`batch stood for ${request.cells} cell(s)${request.provisioned === undefined ? "" : ", provisioned"}`)
      return lane === STANDS ? host : undefined
    }),
  standSut: (lane, _under, variables) =>
    Effect.sync(() => {
      events.push(`sut stood${variables === undefined ? "" : ` with ${Object.keys(variables).join(",")}`}`)
      return lane === STANDS ? sut : undefined
    }),
  preflight: () => Effect.succeed(null)
})

const withStore = <A, E>(effect: Effect.Effect<A, E, Batches.Service | Jobs.Service | LaneHosting.Service>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Layer.mergeAll(Batches.layer, Jobs.layer, hosting)),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

const jobOf = Effect.flatMap(Jobs.Service, (jobs) => jobs.create({ kind: "campaign", args: {}, createdBy: "test" }))

const collect = Effect.sync(() => {
  events.push("collectors opened")
  return Effect.sync(() => void events.push("collectors closed"))
})

const stand: LaneBatch.Stand = { under: "/tmp/unused", memoryMax: "1G", cells: 2, provisioned: { routing: "x" }, sutVariables: { A: "1" } }

const driveOn = (lane: string) => (stood: LaneBatch.Stood) =>
  Effect.sync(() => {
    events.push(`cells ran${stood.host === undefined ? "" : " in the batch process"}`)
    return { value: lane, interrupted: 0 }
  })

describe.skipIf(!served)("one batch on a lane", () => {
  it("stands the lane, opens the collectors, runs the cells, reads the gate, closes the collectors, then gives back", async () => {
    events = []
    hostDied = undefined
    const { ended, rows } = await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      const ended = yield* LaneBatch.run({
        lane: STANDS,
        row: { jobId: job, lane: STANDS, cells: ["a", "b"] },
        stand,
        collect,
        drive: driveOn(STANDS)
      })
      return { ended, rows: yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job)) }
    }))
    expect(events).toEqual([
      "batch stood for 2 cell(s), provisioned",
      "sut stood with A",
      "collectors opened",
      "cells ran in the batch process",
      "gate",
      "collectors closed",
      "sut stopped",
      "killed"
    ])
    expect(ended).toMatchObject({ value: STANDS, dirty: undefined, deadHost: undefined, judged: true, hosted: true })
    expect(rows.map((row) => [row.cells, row.detail])).toEqual([[["a", "b"], null]])
    expect(rows[0]!.finishedAt).not.toBeNull()
  })

  it("reads no gate of a dead process, leaving the batch unjudged and its row saying why", async () => {
    events = []
    hostDied = "the batch process exited 9"
    const { ended, rows } = await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      const ended = yield* LaneBatch.run({ lane: STANDS, row: { jobId: job, lane: STANDS, cells: ["a"] }, stand, drive: driveOn(STANDS) })
      return { ended, rows: yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job)) }
    }))
    expect(events).not.toContain("gate")
    expect(ended).toMatchObject({ deadHost: "the batch process exited 9", judged: false })
    expect(rows.map((row) => row.detail)).toEqual(["the batch process exited 9"])
  })

  it("brackets the cells of a lane that stands nothing, and writes a retry as a row of its own", async () => {
    events = []
    const rows = await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      yield* LaneBatch.run({ lane: PAUSED, row: { jobId: job, lane: PAUSED, cells: ["a", "b"] }, stand, collect, drive: driveOn(PAUSED) })
      yield* LaneBatch.run({ lane: PAUSED, row: { jobId: job, lane: PAUSED, attempt: 2, cells: ["b"] }, stand, collect, drive: driveOn(PAUSED) })
      return yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job))
    }))
    expect(events.filter((event) => event.startsWith("collectors") || event.startsWith("cells"))).toEqual([
      "collectors opened",
      "cells ran",
      "collectors closed",
      "collectors opened",
      "cells ran",
      "collectors closed"
    ])
    expect(rows.map((row) => [row.attempt, row.cells])).toEqual([[1, ["a", "b"]], [2, ["b"]]])
    expect(rows.every((row) => row.finishedAt !== null && row.detail === null)).toBe(true)
  })

  it("closes the row of a batch whose cells failed, the failure as its detail, and gives the lane back", async () => {
    events = []
    hostDied = undefined
    const { failed, rows } = await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      const failed = yield* Effect.flip(LaneBatch.run({
        lane: STANDS,
        row: { jobId: job, lane: STANDS, cells: ["a"] },
        stand,
        drive: () => Effect.fail(new Error("no cell could start"))
      }))
      return { failed, rows: yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job)) }
    }))
    expect(String(failed)).toContain("no cell could start")
    expect(events.slice(-2)).toEqual(["sut stopped", "killed"])
    expect(rows[0]!.finishedAt).not.toBeNull()
    expect(rows[0]!.detail).toContain("no cell could start")
  })
})
