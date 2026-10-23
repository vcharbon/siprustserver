/**
 * `compare A B --request [--repeat N]`: the calls that moved between two
 * run sets, frozen as one replay request on the compared lane, each call by
 * the key its case id is frozen to, replayed N times per job.
 */
import { CaseIds, type Compare, Requests } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { describe, expect, it } from "vitest"
import { LaneHosting } from "../../src/deployment/index.js"
import { DEFAULT_BATCH, DEFAULT_CONCURRENCY, DEFAULT_STARTS_PER_SECOND } from "../../src/knobs.js"
import * as CompareRequest from "../../src/requests/compare-request.js"

const C = "trace.pcap.gz"

const cell = (caseId: string, passed: boolean, lane = "fake"): Compare.Cell => ({
  capture: C,
  caseId,
  runId: 1,
  lane,
  env: "integration",
  branch: "main",
  laneHash: "l1",
  prepId: 1,
  bundlePath: "/w",
  passed,
  verdict: passed ? "ok" : "failed",
  skipped: null,
  failedStep: passed ? null : "s6",
  documentHash: "d",
  mainError: null,
  deltas: null,
  code: []
})

const move = (caseId: string, moveClass: Compare.MoveClass, lane = "fake"): Compare.Move => ({
  class: moveClass,
  capture: C,
  caseId,
  document: "same",
  code: "different",
  lane: "same",
  before: cell(caseId, moveClass !== "red→green", lane),
  after: cell(caseId, moveClass === "red→green", lane)
})

const side: Compare.Side = { selector: "@current", runs: 1, sweeps: [], cells: [], elsewhere: { captures: 0, calls: 0, keys: [] } }

const comparison = (moves: ReadonlyArray<Compare.Move>): Compare.Comparison => ({
  before: side,
  after: { ...side, selector: "@env:task-1" },
  compared: moves.length,
  counts: {} as Compare.Comparison["counts"],
  moves,
  onlyBefore: { captures: 0, calls: 0, keys: [] },
  onlyAfter: { captures: 0, calls: 0, keys: [] }
})

const created: Array<Requests.CreateRequest> = []

const refuse = Effect.die("not in this test")

/** `fake` states no default of its own; `wall` defaults every knob a lane may. */
const hosting = LaneHosting.layerWith({
  lanes: ["fake", "wall"],
  lane: (lane) =>
    lane === "wall"
      ? Effect.succeed({ clock: "real", exclusive: true, defaults: { concurrency: 100, batchSize: 4000, startsPerSecond: 10 } })
      : Effect.succeed(LaneHosting.NEUTRAL_LANE),
  standBatch: () => Effect.succeed(undefined),
  standSut: () => Effect.succeed(undefined),
  preflight: () => Effect.succeed(null)
})

const services = Layer.mergeAll(
  hosting,
  Layer.succeed(
    CaseIds.Service,
    CaseIds.Service.of({
      of: (captures) =>
        Effect.succeed(
          new Map(captures.map((capture) => [capture, [
            { caseId: `${C}-case1`, callKey: "k1@h", first: null },
            { caseId: `${C}-case2`, callKey: "k2@h", first: null }
          ]]))
        ),
      freeze: () => refuse,
      markSeeded: () => refuse,
      unseeded: () => refuse,
      count: refuse
    })
  ),
  Layer.succeed(
    Requests.Service,
    Requests.Service.of({
      create: (request) =>
        Effect.sync(() => {
          created.push(request)
          return { id: 77, cells: request.cells.length, captures: [C] }
        }),
      list: () => refuse,
      get: () => refuse,
      show: () => refuse,
      resolve: () => refuse,
      recordJob: () => refuse,
      ran: () => refuse,
      unran: () => refuse,
      jobCells: () => refuse
    })
  )
)

const asked: CompareRequest.Asked = { lane: "fake", repeat: 3, createdBy: "task-1" }

describe("CompareRequest.request", () => {
  it("freezes the moved calls, keyed by their frozen ids, on the compared lane with the repeat", async () => {
    created.length = 0
    const got = await Effect.runPromise(
      CompareRequest.request(
        comparison([move(`${C}-case1`, "red→green"), move(`${C}-case2`, "unchanged"), move(`${C}-case9`, "green→red")]),
        asked
      ).pipe(Effect.provide(services))
    )
    expect(got).toEqual({ _tag: "created", id: 77, cells: 1, unkeyed: [`${C}/${C}-case9`] })
    expect(created[0]!.cells).toEqual([{ capture: C, callKey: "k1@h" }])
    expect(created[0]!.lane).toBe("fake")
    expect(created[0]!.knobs.repeat).toBe(3)
    expect(created[0]!.selection).toEqual({ kind: "cells", file: "compare @current @env:task-1", whole: [] })
  })

  it("states the lane's own knob defaults on a lane that has them, the generic ones on a lane that has none", async () => {
    created.length = 0
    await Effect.runPromise(
      CompareRequest.request(comparison([move(`${C}-case1`, "red→green", "wall")]), asked).pipe(Effect.provide(services))
    )
    await Effect.runPromise(
      CompareRequest.request(comparison([move(`${C}-case1`, "red→green")]), asked).pipe(Effect.provide(services))
    )
    expect(created.map((one) => [one.lane, one.knobs.concurrency, one.knobs.batchSize, one.knobs.startsPerSecond]))
      .toEqual([
        ["wall", 100, 4000, 10],
        ["fake", DEFAULT_CONCURRENCY, DEFAULT_BATCH, DEFAULT_STARTS_PER_SECOND]
      ])
  })

  it("refuses where nothing moved: there is nothing to replay", async () => {
    created.length = 0
    const got = await Effect.runPromise(
      CompareRequest.request(comparison([move(`${C}-case2`, "unchanged")]), asked).pipe(Effect.provide(services))
    )
    expect(got._tag).toBe("refused")
    expect(created).toEqual([])
  })
})

describe("CompareRequest.repeatRefusal", () => {
  it("refuses --repeat without --request, and a repeat under one", () => {
    expect(CompareRequest.repeatRefusal(false, 3)).toContain("--request")
    expect(CompareRequest.repeatRefusal(true, 0)).toContain("at least 1")
    expect(CompareRequest.repeatRefusal(false, 1)).toBeNull()
    expect(CompareRequest.repeatRefusal(true, 3)).toBeNull()
  })
})

describe("CompareRequest.passes", () => {
  it("reads a request's repeat, one pass where it states none", () => {
    expect(CompareRequest.passes({ concurrency: 8, batchSize: 1000, memoryMax: "4G", startsPerSecond: 0, cleanup: true }))
      .toBe(1)
    expect(
      CompareRequest.passes({ concurrency: 8, batchSize: 1000, memoryMax: "4G", startsPerSecond: 0, cleanup: true, repeat: 3 })
    ).toBe(3)
  })
})
