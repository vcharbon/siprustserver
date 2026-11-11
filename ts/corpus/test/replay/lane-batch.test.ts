/**
 * One batch on a lane, against a real postgres and a lane that stands a fake
 * batch process and SUT: the lane stands before the collectors open, the
 * cells run on what it stood, the end gate reads before the collectors close
 * and both are given back last; a dead process takes no gate and leaves the
 * batch unjudged; an end gate that wrote no end line leaves it unjudged, its
 * row saying so; a gate's leak and a dead SUT are both said; each gate the
 * end read is a line of the batch, its end's own line beside them only where
 * it says what no gate does; a lane that
 * stands nothing still brackets its cells; a batch that fails still closes
 * its row; a batch filed under no owner writes no row.
 *
 * The suite builds and drops its OWN database (`@sip/store/test-server`).
 */
import { Batches, Client, Jobs } from "@sip/store"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as Layer from "effect/Layer"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { LaneHosting } from "../../src/deployment/index.js"
import type { BatchHost, Gate, SutHandle } from "../../src/replay/batch-protocol.js"
import * as LaneBatch from "../../src/replay/lane-batch.js"

const SCRATCH = await scratch("corpus_lane_batch_test")

const STANDS = "stands"
const PAUSED = "paused"

let events: Array<string> = []
let hostDied: string | undefined
let gateRead: string | undefined
let hostJudged = true
let sutDied: string | undefined
let gatesRead: ReadonlyArray<Gate> = []

const host: BatchHost = {
  run: () => Effect.die("unused"),
  died: () => hostDied,
  end: Effect.sync(() => {
    events.push("gate")
    return gateRead
  }),
  contexts: () => undefined,
  gates: () => gatesRead,
  kill: Effect.sync(() => void events.push("killed")),
  judged: () => hostJudged
}

const sut: SutHandle = {
  metrics: undefined,
  died: () => sutDied,
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

const withStore = <A, E>(effect: Effect.Effect<A, E, Batches.Service | Jobs.Service | LaneHosting.Service | SqlClient>): Promise<A> =>
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

/** The lane's batch ends as `said`, everything else clean. */
const endingAs = (
  said: {
    readonly gate?: string
    readonly gates?: ReadonlyArray<Gate>
    readonly judged?: boolean
    readonly sut?: string
    readonly died?: string
  }
) => {
  events = []
  hostDied = said.died
  gateRead = said.gate
  gatesRead = said.gates ?? []
  hostJudged = said.judged ?? true
  sutDied = said.sut
}

const driveOn = (lane: string) => (stood: LaneBatch.Stood) =>
  Effect.sync(() => {
    events.push(`cells ran${stood.host === undefined ? "" : " in the batch process"}`)
    return { value: lane, interrupted: 0 }
  })

describe.skipIf(!served)("one batch on a lane", () => {
  it("stands the lane, opens the collectors, runs the cells, reads the gate, closes the collectors, then gives back", async () => {
    endingAs({})
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
    endingAs({ died: "the batch process exited 9" })
    const { ended, rows } = await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      const ended = yield* LaneBatch.run({ lane: STANDS, row: { jobId: job, lane: STANDS, cells: ["a"] }, stand, drive: driveOn(STANDS) })
      return { ended, rows: yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job)) }
    }))
    expect(events).not.toContain("gate")
    expect(ended).toMatchObject({ deadHost: "the batch process exited 9", judged: false })
    expect(rows.map((row) => row.detail)).toEqual(["the batch process exited 9"])
  })

  it("leaves a batch whose end gate wrote no end line unjudged, its row saying so", async () => {
    endingAs({ judged: false })
    const { ended, rows } = await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      const ended = yield* LaneBatch.run({ lane: STANDS, row: { jobId: job, lane: STANDS, cells: ["a"] }, stand, drive: driveOn(STANDS) })
      return { ended, rows: yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job)) }
    }))
    expect(events).toContain("gate")
    expect(ended).toMatchObject({ dirty: undefined, deadHost: undefined, judged: false, hosted: true })
    expect(rows.map((row) => row.detail)).toEqual([LaneBatch.NO_END_LINE])
  })

  it("says both what the gate read the SUT holding and how the managed SUT died", async () => {
    endingAs({ gate: "the SUT still held a call", sut: "the SUT exited 137" })
    const { ended, rows } = await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      const ended = yield* LaneBatch.run({ lane: STANDS, row: { jobId: job, lane: STANDS, cells: ["a"] }, stand, drive: driveOn(STANDS) })
      return { ended, rows: yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job)) }
    }))
    expect(ended).toMatchObject({ dirty: "the SUT still held a call; the SUT exited 137", judged: true })
    expect(rows.map((row) => row.detail)).toEqual(["the SUT still held a call; the SUT exited 137"])
  })

  it("holds its end gate's line: passing on a clean end, failing on a dirty one, not checkable unjudged, none where nothing stood", async () => {
    const endOn = (lane: string, said: Parameters<typeof endingAs>[0]) =>
      Effect.gen(function*() {
        endingAs(said)
        const job = yield* jobOf
        yield* LaneBatch.run({ lane, row: { jobId: job, lane, cells: ["a"] }, stand, drive: driveOn(lane) })
        const [row] = yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job))
        const sql = yield* SqlClient
        return yield* sql<{ name: string; answer: string }>`
          SELECT name, answer FROM verifications WHERE scope = 'batch' AND batch_id = ${row!.id} ORDER BY ordinal
        `.pipe(Effect.map((lines) => lines.map((line) => `${line.name} ${line.answer}`)))
      })
    const leak = "the SUT never reaped"
    const read: ReadonlyArray<Gate> = [
      { name: "reap", answer: "fail", message: leak },
      { name: "contexts", answer: "pass" },
      { name: "limiter-drain", answer: "pass" }
    ]
    const lines = await withStore(Effect.all([
      endOn(STANDS, {}),
      endOn(STANDS, { gate: "the SUT still held a call" }),
      endOn(STANDS, { sut: "the SUT exited 137" }),
      endOn(STANDS, { judged: false }),
      endOn(PAUSED, {}),
      endOn(STANDS, { gate: leak, gates: read }),
      endOn(STANDS, { gates: read.slice(1) }),
      endOn(STANDS, { gate: leak, gates: read, sut: "the SUT exited 137" })
    ]))
    expect(lines).toEqual([
      [`${LaneBatch.END_GATE} pass`],
      [`${LaneBatch.END_GATE} fail`],
      [`${LaneBatch.END_GATE} fail`],
      [`${LaneBatch.END_GATE} not-checkable`],
      [],
      ["reap fail", "contexts pass", "limiter-drain pass"],
      ["contexts pass", "limiter-drain pass"],
      ["reap fail", "contexts pass", "limiter-drain pass", `${LaneBatch.END_GATE} fail`]
    ])
  })

  it("states a failing gate's finding as its line's message, typed to the SUT", () => {
    const lines = LaneBatch.linesOf({
      dirty: "the SUT never reaped",
      gates: [{ name: "reap", answer: "fail", message: "the SUT never reaped" }],
      sutDied: undefined,
      deadHost: undefined,
      interrupted: 0,
      judged: true,
      hosted: true
    })
    expect(lines).toEqual([{
      name: "reap",
      source: "batch process",
      answer: "fail",
      message: "the SUT never reaped",
      detail: { seen: "the SUT never reaped", side: "sut" }
    }])
  })

  it("brackets the cells of a lane that stands nothing", async () => {
    endingAs({})
    const { ended, rows } = await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      const ended = yield* LaneBatch.run({ lane: PAUSED, row: { jobId: job, lane: PAUSED, cells: ["a", "b"] }, stand, collect, drive: driveOn(PAUSED) })
      return { ended, rows: yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job)) }
    }))
    expect(events.filter((event) => event.startsWith("collectors") || event.startsWith("cells"))).toEqual([
      "collectors opened",
      "cells ran",
      "collectors closed"
    ])
    expect(ended).toMatchObject({ value: PAUSED, judged: true, hosted: false })
    expect(rows.map((row) => [row.cells, row.detail, row.finishedAt !== null])).toEqual([[["a", "b"], null, true]])
  })

  it("writes no row for a batch filed under no owner, and still runs it", async () => {
    endingAs({})
    const { ended, before, after } = await withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      const count = Effect.map(sql<{ n: string }>`SELECT count(*) AS n FROM batches`, (rows) => Number(rows[0]!.n))
      const before = yield* count
      const ended = yield* LaneBatch.run({ lane: STANDS, stand, collect, drive: driveOn(STANDS) })
      return { ended, before, after: yield* count }
    }))
    expect(ended).toMatchObject({ value: STANDS, judged: true, hosted: true })
    expect(events).toContain("cells ran in the batch process")
    expect(after).toBe(before)
  })

  it("closes the row of a batch whose cells failed, the failure as its detail, and gives the lane back", async () => {
    endingAs({})
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
