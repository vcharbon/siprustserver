/**
 * A run's record over a fixture pack's store: opening writes the campaign run
 * row under the first lane's identity and one aside row per lane linked to
 * it; closing a lane finishes its row with its case results (a ref
 * no case covers skipped `not-ported`), done or an error, and writes each
 * bundled cell's record with the pack's log collector's line; aborting closes
 * every lane still open as an error and finishes the campaign row; a setup
 * that left the lab changed makes every lane an error, closed before or after.
 */
import { NodeServices } from "@effect/platform-node"
import { Aside, Runs } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { describe, expect, it } from "vitest"
import { type Cell, type CellResult, errored } from "../src/cell.js"
import * as Record from "../src/record.js"

const unused = Effect.die(new Error("not asked in this test"))

/** What the fake stores were asked, in order. */
interface Calls {
  readonly said: Array<string>
  readonly opened: Array<Runs.OpenRequest>
  readonly finished: Array<Runs.FinishRequest>
  readonly cells: Array<{ readonly runId: number; readonly caseId: string; readonly cell: Record.CellRecord }>
}

/** A run store numbering campaign rows from 7 and a runs table numbering lane rows from 100, both recording what they were asked. */
const fakes = () => {
  const calls: Calls = { said: [], opened: [], finished: [], cells: [] }
  let next = 100
  const runs = Layer.succeed(
    Runs.Service,
    Runs.Service.of({
      open: (request) =>
        Effect.sync(() => {
          calls.opened.push(request)
          const id = next++
          return { id, name: `run${id}`, bundlePath: `${request.sessionDir}/run${id}` }
        }),
      finish: (request) => Effect.sync(() => void calls.finished.push(request)),
      openMany: () => unused,
      importRun: () => unused,
      lanes: unused,
      ranOn: () => unused,
      standings: () => unused,
      standing: () => unused,
      protectedBundles: unused,
      allBundles: unused,
      unfinished: unused,
      abandonable: () => unused,
      abandon: () => unused,
      unfinishedOf: () => unused,
      dirty: () => unused,
      forced: () => unused,
      openOfSweep: () => unused,
      forget: () => unused
    })
  )
  const store: Record.RunStore<{ readonly label: string }> = {
    open: (request) => Effect.sync(() => (calls.said.push(`open ${request.capture} ${request.label} env=${request.env}`), 7)),
    link: (campaign, runId) => Effect.sync(() => void calls.said.push(`link ${campaign} ${runId}`)),
    writeCell: (runId, caseId, cell) => Effect.sync(() => void calls.cells.push({ runId, caseId, cell })),
    finish: (campaign) => Effect.sync(() => void calls.said.push(`finish ${campaign}`))
  }
  return { calls, runs, store }
}

const OPENING: Record.Opening<never, { readonly label: string }> = {
  capture: "every-case",
  sessions: "/sessions",
  dir: "/runs/r1",
  seed: null,
  scope: null,
  selection: { allow: [] } as unknown as Record.Opening<never, object>["selection"],
  columns: { label: "nightly" },
  provenance: { origin: "test" } as unknown as Record.Opening<never, object>["provenance"],
  jobId: 42,
  identity: (lane) => Effect.succeed({ env: `env-of-${lane}`, branch: "b", sha: "s", dirty: false } as never),
  idOf: (lane) => `rig-${lane}`
}

const cell = (id: string): Cell => ({
  id,
  ref: `Routing/${id}`,
  subcase: "s",
  retries: 0,
  source: { file: `cases/${id}.ts`, text: "" },
  checked: [],
  locks: [],
  needs: [],
  tags: [],
  alone: false,
  heal: { healMs: 0, healRealClockOnSimulatedMs: 0 },
  dir: `/cells/${id}`,
  endpoints: ["alice", "bob"],
  egress: "bob"
})

/** A passing result of `id` on `lane`, with no bundle unless `bundle`. */
const passed = (lane: string, id: string, bundle?: string): CellResult => ({
  ...errored(lane, cell(id), ""),
  status: "pass",
  ...(bundle === undefined ? {} : { bundle })
})

const PACK = { own: [], logCollector: "sut-log" }

describe("a run's record", () => {
  it("opens the campaign row under the first lane's identity, then an aside row per lane linked to it", async () => {
    const { calls, runs, store } = fakes()
    const record = await Effect.runPromise(Record.open(store, ["sim", "hosted"], OPENING).pipe(Effect.provide(runs)))
    expect(calls.said).toEqual(["open every-case nightly env=env-of-rig-sim", "link 7 100", "link 7 101"])
    expect(calls.opened.map((one) => [one.lane, one.mode, one.sessionDir, one.jobId, one.identity?.env])).toEqual([
      ["rig-sim", "provisioned", `/sessions/every-case/${Aside.DIR}`, 42, Aside.ENV],
      ["rig-hosted", "provisioned", `/sessions/every-case/${Aside.DIR}`, 42, Aside.ENV]
    ])
    expect(record.lanes.map((row) => [row.lane, row.runId, row.bundlePath])).toEqual([
      ["sim", 100, `/sessions/every-case/${Aside.DIR}/run100`],
      ["hosted", 101, `/sessions/every-case/${Aside.DIR}/run101`]
    ])
    expect(record.jobId).toBe(42)
  })

  it("closes a lane done with its results and the refs no case covers, each bundled cell's record beside", async () => {
    const { calls, runs, store } = fakes()
    const closed = Effect.gen(function*() {
      const record = yield* Record.open(store, ["sim"], OPENING)
      const result = {
        ...passed("sim", "a", "/nowhere/a-cell"),
        collected: [{ name: "sut-log", source: "sut-log", answer: "pass", detail: { records: 3 } }]
      } as unknown as CellResult
      yield* Record.close(store, PACK, {
        record,
        row: record.lanes[0]!,
        results: [result, passed("sim", "b")],
        notPorted: ["Routing/unported"],
        error: undefined,
        unlogged: undefined
      })
      return record
    })
    const record = await Effect.runPromise(closed.pipe(Effect.provide(runs), Effect.provide(NodeServices.layer)))
    const finish = calls.finished[0]!
    expect([finish.id, finish.status, finish.detail]).toEqual([100, "done", undefined])
    expect(finish.cases.map((one) => [one.caseId, one.skipped ?? null])).toEqual([
      ["Routing/a [s]", null],
      ["Routing/b [s]", null],
      ["Routing/unported", "not-ported"]
    ])
    expect(calls.cells).toEqual([{ runId: 100, caseId: "Routing/a [s]", cell: { cell: "a-cell", log: { records: 3 } } }])
    expect(record.closed.get(100)).toBeUndefined()
  })

  it("aborts every lane still open as an error and finishes the campaign row; a closed lane keeps its close", async () => {
    const { calls, runs, store } = fakes()
    await Effect.runPromise(Effect.gen(function*() {
      const record = yield* Record.open(store, ["sim", "hosted"], OPENING)
      yield* Record.close(store, PACK, {
        record,
        row: record.lanes[0]!,
        results: [],
        notPorted: [],
        error: "the lane's batch died",
        unlogged: undefined
      })
      yield* Record.abort(store, record, "interrupted")
    }).pipe(Effect.provide(runs), Effect.provide(NodeServices.layer)))
    expect(calls.finished.map((one) => [one.id, one.status, one.detail])).toEqual([
      [100, "error", "the lane's batch died"],
      [101, "error", "interrupted"]
    ])
    expect(calls.said.at(-1)).toBe("finish 7")
  })

  it("makes every lane an error once a setup left the lab changed, closed before it was learnt or after", async () => {
    const { calls, runs, store } = fakes()
    await Effect.runPromise(Effect.gen(function*() {
      const record = yield* Record.open(store, ["sim", "hosted"], OPENING)
      const closing = { results: [], notPorted: [], unlogged: undefined }
      yield* Record.close(store, PACK, { ...closing, record, row: record.lanes[0]!, error: undefined })
      yield* Record.leftChanged(record, "the heal left a call up")
      yield* Record.close(store, PACK, { ...closing, record, row: record.lanes[1]!, error: "slow" })
    }).pipe(Effect.provide(runs), Effect.provide(NodeServices.layer)))
    expect(calls.finished.map((one) => [one.id, one.status, one.detail])).toEqual([
      [100, "done", undefined],
      [100, "error", "the heal left a call up"],
      [101, "error", "slow; the heal left a call up"]
    ])
  })
})
