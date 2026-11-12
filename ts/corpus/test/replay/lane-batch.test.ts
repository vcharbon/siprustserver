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
 * The rig's collectors, real executables: opened once the lane stood,
 * collected once the gate read and the caller's collectors closed, closed
 * before the SUT is given back; their line of the batch is the row's, their
 * attachment of it the batch's; one whose collect exits non-zero answers
 * every cell and the batch not checkable with its message; a batch whose
 * cells fail still closes them.
 *
 * The suite builds and drops its OWN database (`@sip/store/test-server`).
 */
import { NodeServices } from "@effect/platform-node"
import { type Cell, RigPath } from "@sip/rig"
import { Attachments, Batches, Client, Jobs, Sessions } from "@sip/store"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { LaneHosting } from "../../src/deployment/index.js"
import type { BatchHost, Gate, SutHandle } from "../../src/replay/batch-protocol.js"
import * as LaneBatch from "../../src/replay/lane-batch.js"

const SCRATCH = await scratch("corpus_lane_batch_test")
const TREE = fs.mkdtempSync(path.join(os.tmpdir(), "lane-batch-"))

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

type Services = Attachments.Service | Batches.Service | Jobs.Service | LaneHosting.Service | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Layer.mergeAll(Attachments.layer, Batches.layer, Jobs.layer, hosting)),
      Effect.provide(Sessions.layer(TREE)),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  fs.rmSync(TREE, { recursive: true, force: true })
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

/** A script file under `dir`, executable. */
const script = (dir: string, name: string, body: string) => {
  fs.writeFileSync(path.join(dir, name), `#!/bin/sh\n${body}\n`, { mode: 0o755 })
}

/**
 * The rig's collector `lab` of a batch under a directory of its own, as the
 * rig path loads it: its `collect` answers `lab-check` for the cell `a` and
 * the batch and attaches a log to the batch, or, `failing`, says why on its
 * stderr and exits 3. The batch's collectors with it, its events in `events`.
 */
const labCollectors = async (failing: boolean): Promise<LaneBatch.Collectors<string, never>> => {
  const root = fs.mkdtempSync(path.join(TREE, "rig-"))
  const dir = path.join(root, "collectors", "lab")
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(
    path.join(dir, "collector.toml"),
    `schema = "collector.v1"\nname = "lab"\nscope = "batch"\nanswers = ["lab-check"]\n[run]\nopen = "./open"\ncollect = "./collect"\nclose = "./close"\n`
  )
  script(dir, "open", "exit 0")
  script(
    dir,
    "collect",
    failing
      ? `echo "the lab is gone" >&2\nexit 3`
      : [
        `echo '{"verification":"lab-check","cell":"a","answer":"pass"}'`,
        `echo '{"verification":"lab-check","answer":"pass"}'`,
        `echo batch > "$SIPRIG_OUT/batch.log"`,
        `echo '{"attachment":{"kind":"log","file":"batch.log"}}'`
      ].join("\n")
  )
  script(dir, "close", "exit 0")
  const loaded = await Effect.runPromise(Effect.result(RigPath.load({ workspace: root })).pipe(Effect.provide(NodeServices.layer)))
  if (!Result.isSuccess(loaded)) throw new Error(`the rig path does not load: ${String(loaded.failure)}`)
  const holder = path.join(root, "batch")
  const cell: Cell.Cell = {
    schema: "cell.v1",
    id: "a",
    case: "case-a",
    lane: STANDS,
    batch: "batch-1",
    attempt: 1,
    clock: "simulated",
    keys: { call_id: [], caller: [], callee: [], request_uri_user: [] },
    legs: [],
    timeline: { start: { lane_ms: 0, wall_ms: 0 }, end: { lane_ms: 1, wall_ms: 1 } },
    claims: {},
    records: {},
    verifications: [],
    dir: path.join(root, "cells", "a"),
    tags: [],
    needs: []
  }
  return {
    instances: [{
      collector: loaded.success.collectors.get("lab")!,
      id: "lab",
      label: "lab",
      scope: "batch",
      lane: undefined,
      lanes: undefined,
      params: {},
      dir: path.join(holder, "collectors", "lab"),
      holder
    }],
    options: {
      sink: (event) => Effect.sync(() => void events.push(`collector ${event.phase}`)),
      locker: () => Effect.succeed(Effect.void)
    },
    holder,
    cells: () => Effect.succeed([cell])
  }
}

describe.skipIf(!served)("the rig's collectors of a batch", () => {
  it("open once the lane stood, collect once the gate read and the caller's collectors closed, close before the lane is given back", async () => {
    endingAs({})
    const collectors = await labCollectors(false)
    const { ended, lines, attached } = await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      const ended = yield* LaneBatch.run({
        lane: STANDS,
        row: { jobId: job, lane: STANDS, cells: ["a"] },
        stand,
        collect,
        collectors,
        drive: driveOn(STANDS)
      })
      const [row] = yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job))
      const sql = yield* SqlClient
      const lines = yield* sql<{ name: string; source: string; answer: string }>`
        SELECT name, source, answer FROM verifications WHERE scope = 'batch' AND batch_id = ${row!.id} ORDER BY ordinal
      `
      const attached = yield* Effect.flatMap(Attachments.Service, (store) => store.of({ scope: "batch", batchId: row!.id }))
      return { ended, lines, attached }
    }))
    expect(events).toEqual([
      "batch stood for 2 cell(s), provisioned",
      "sut stood with A",
      "collector open",
      "collector opened",
      "collectors opened",
      "cells ran in the batch process",
      "gate",
      "collectors closed",
      "collector collect",
      "collector collected",
      "collector close",
      "collector closed",
      "sut stopped",
      "killed"
    ])
    expect(ended.collected[0]!.cells.get("a")!.verifications).toEqual([{ name: "lab-check", source: "lab", answer: "pass" }])
    expect(lines.map((line) => `${line.name} ${line.source} ${line.answer}`)).toEqual(["lab-check lab pass"])
    expect(attached.map((one) => [one.name, one.path])).toEqual([
      ["lab", path.join(collectors.holder, "collected", "lab", "batch.log")]
    ])
    expect(fs.readFileSync(attached[0]!.path!, "utf8")).toBe("batch\n")
  })

  it("answer every cell and the batch not checkable, quoting the step, where collect exits non-zero, and still close", async () => {
    endingAs({})
    const collectors = await labCollectors(true)
    const { ended, lines } = await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      const ended = yield* LaneBatch.run({ lane: STANDS, row: { jobId: job, lane: STANDS, cells: ["a"] }, stand, collectors, drive: driveOn(STANDS) })
      const [row] = yield* Effect.flatMap(Batches.Service, (batches) => batches.ofJob(job))
      const sql = yield* SqlClient
      const lines = yield* sql<{ name: string; answer: string; message: string }>`
        SELECT name, answer, message FROM verifications WHERE scope = 'batch' AND batch_id = ${row!.id} ORDER BY ordinal
      `
      return { ended, lines }
    }))
    const said = `collect exited 3: the lab is gone, its log ${path.join(collectors.instances[0]!.dir, "collect.log")}`
    expect(ended.collected[0]!.cells.get("a")!.verifications).toEqual([
      { name: "lab-check", source: "lab", answer: "not-checkable", message: said }
    ])
    expect(lines).toEqual([{ name: "lab-check", answer: "not-checkable", message: said }])
    expect(events.slice(-4)).toEqual(["collector close", "collector closed", "sut stopped", "killed"])
  })

  it("close where the batch's cells failed, before the lane is given back", async () => {
    endingAs({})
    const collectors = await labCollectors(false)
    await withStore(Effect.gen(function*() {
      const job = yield* jobOf
      yield* Effect.flip(LaneBatch.run({
        lane: STANDS,
        row: { jobId: job, lane: STANDS, cells: ["a"] },
        stand,
        collectors,
        drive: () => Effect.fail(new Error("no cell could start"))
      }))
    }))
    expect(events).not.toContain("collector collect")
    expect(events.slice(-4)).toEqual(["collector close", "collector closed", "sut stopped", "killed"])
  })
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

  it("holds its end gate's line: none on a clean end with no gate, failing on a dirty one, not checkable unjudged whatever it said in its place, none where nothing stood", async () => {
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
      endOn(STANDS, { gate: leak, gates: read, sut: "the SUT exited 137" }),
      endOn(STANDS, { gate: "the batch process wrote no end line within 85 s", judged: false })
    ]))
    expect(lines).toEqual([
      [],
      [`${LaneBatch.END_GATE} fail`],
      [`${LaneBatch.END_GATE} fail`],
      [`${LaneBatch.END_GATE} not-checkable`],
      [],
      ["reap fail", "contexts pass", "limiter-drain pass"],
      ["contexts pass", "limiter-drain pass"],
      ["reap fail", "contexts pass", "limiter-drain pass", `${LaneBatch.END_GATE} fail`],
      [`${LaneBatch.END_GATE} not-checkable`]
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
