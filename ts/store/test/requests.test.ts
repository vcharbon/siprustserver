/**
 * Replay requests against a real postgres: a request names calls by key, a
 * resolution reads them against one prep, and a whole-capture request grows
 * where a filter request does not. Every claim is about what the
 * database does — a renamed case found by its key, one insert for N cells —
 * so none survives a fake. The suite builds and drops its own database.
 */
import * as Effect from "effect/Effect"
import * as Tracer from "effect/Tracer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Cuts from "../src/cuts.js"
import * as Jobs from "../src/jobs.js"
import { type CaptureCutRequest, type CutCaseRequest, wholeFlows } from "../src/model.js"
import * as Requests from "../src/requests.js"
import * as Sessions from "../src/sessions.js"

const SCRATCH = Client.scratchDatabase("store_request_test")

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

const withStore = <A, E>(
  effect: Effect.Effect<A, E, Requests.Service | Cuts.Service | Jobs.Service | SqlClient>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Requests.layer),
      Effect.provide(Cuts.layer),
      Effect.provide(Jobs.layer),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = await Effect.runPromise(
  Effect.gen(function*() {
    const sql = yield* SqlClient
    yield* sql`SELECT 1`
  }).pipe(
    Effect.provide(Client.layerFromUrl(urlFor("postgres"))),
    Effect.match({ onFailure: () => false, onSuccess: () => true })
  )
)

const bytes = (text: string): Uint8Array => new TextEncoder().encode(text)

const caseOf = (id: string, callKey: string): CutCaseRequest => ({
  caseId: id,
  callKey,
  document: { id, actors: [{ kind: "uas", endpoint: "ep2" }], steps: [] },
  assets: [],
  ledger: [{ number: "4242000000000001", document: { case: id, route: "site" } }],
  egressEndpoint: "ep2"
})

const cutOf = (
  prepId: number,
  capture: string,
  cases: ReadonlyArray<CutCaseRequest>,
  refusals: CaptureCutRequest["refusals"] = []
): CaptureCutRequest => ({
  prepId,
  capture,
  flows: wholeFlows(bytes(`{"capture":"${capture}"}`)),
  cases,
  refusals
})

const KNOBS: Requests.Knobs = { concurrency: 8, batchSize: 1000, memoryMax: "4G", startsPerSecond: 0, cleanup: true }

const cells = (capture: string, keys: ReadonlyArray<string>): ReadonlyArray<Requests.Cell> =>
  keys.map((callKey) => ({ capture, callKey }))

/**
 * Two preps of one capture: the first cuts three cases; the second renames
 * one, refuses one, keeps one and adds one.
 */
const twoPreps = (capture: string) =>
  Effect.gen(function*() {
    const cuts = yield* Cuts.Service
    const first = yield* cuts.open()
    yield* cuts.record(cutOf(first, capture, [
      caseOf(`${capture}-case1`, "K1@x"),
      caseOf(`${capture}-case2`, "K2@x"),
      caseOf(`${capture}-case3`, "K3@x")
    ]))
    const second = yield* cuts.open()
    yield* cuts.record(cutOf(
      second,
      capture,
      [
        caseOf(`${capture}-cut9`, "K1@x"),
        caseOf(`${capture}-case3`, "K3@x"),
        caseOf(`${capture}-case4`, "K4@x")
      ],
      [{
        caseId: `${capture}-cut2`,
        reasons: ["source-truncated-at-edge", "source-call-incomplete"],
        callIds: ["K2@x", "1-K2@x"],
        detail: "edge"
      }]
    ))
    return { first, second }
  })

beforeAll(async () => {
  if (!served) return
  await on(
    "postgres",
    Effect.gen(function*() {
      const sql = yield* SqlClient
      yield* sql.unsafe(`DROP DATABASE IF EXISTS ${SCRATCH} WITH (FORCE)`)
      yield* sql.unsafe(`CREATE DATABASE ${SCRATCH}`)
    })
  )
  await on(SCRATCH, Server.migrated)
}, 60_000)

afterAll(async () => {
  if (!served) return
  await on(
    "postgres",
    Effect.gen(function*() {
      const sql = yield* SqlClient
      yield* sql.unsafe(`DROP DATABASE IF EXISTS ${SCRATCH} WITH (FORCE)`)
    })
  )
}, 60_000)

describe.skipIf(!served)("Requests", () => {
  it("resolves a whole-capture request against two preps: a renamed case by key, a refused key gone with its reasons, the new case added", () =>
    withStore(Effect.gen(function*() {
      const requests = yield* Requests.Service
      const capture = "capture_whole.pcap.gz"
      const { first, second } = yield* twoPreps(capture)
      const created = yield* requests.create({
        lane: "fake",
        selection: { kind: "captures", captures: [capture], whole: [capture] },
        knobs: KNOBS,
        createdBy: "test",
        cells: cells(capture, ["K1@x", "K2@x", "K3@x"])
      })
      expect(created).toEqual({ id: expect.any(Number), cells: 3, captures: [capture] })

      const before = (yield* requests.resolve(created.id, first))!
      expect(before.captures).toHaveLength(1)
      const one = before.captures[0]!
      expect(one.kind).toEqual("full")
      expect(one.planned.map((cell) => [cell.callKey, cell.caseId])).toEqual([
        ["K1@x", `${capture}-case1`],
        ["K2@x", `${capture}-case2`],
        ["K3@x", `${capture}-case3`]
      ])
      expect(one.gone).toEqual([])
      expect(one.added).toEqual([])

      const after = (yield* requests.resolve(created.id, second))!
      const grown = after.captures[0]!
      // The renamed case is found by its key, never by the id the first cut minted.
      expect(grown.planned.map((cell) => [cell.callKey, cell.caseId, cell.origin])).toEqual([
        ["K1@x", `${capture}-cut9`, "selected"],
        ["K3@x", `${capture}-case3`, "selected"],
        ["K4@x", `${capture}-case4`, "added_by_prep"]
      ])
      expect(grown.gone).toEqual([{
        capture,
        callKey: "K2@x",
        origin: "selected",
        gone: "refused",
        reasons: ["source-truncated-at-edge", "source-call-incomplete"]
      }])
      expect(grown.added).toEqual(["K4@x"])
      // Whole means whole today: every key of the pinned cut is on the request.
      expect(grown.kind).toEqual("full")
      const detail = (yield* requests.show(created.id))!
      expect(detail.cells).toEqual([
        { capture, callKey: "K1@x", origin: "selected" },
        { capture, callKey: "K2@x", origin: "selected" },
        { capture, callKey: "K3@x", origin: "selected" },
        { capture, callKey: "K4@x", origin: "added_by_prep" }
      ])
      // A second resolution against the same prep adds nothing more.
      expect((yield* requests.resolve(created.id, second))!.captures[0]!.added).toEqual([])
      expect((yield* requests.show(created.id))!.request.cells).toEqual(4)
    })))

  it("keeps a filter request frozen: a new case of the prep is not added, and the capture resolves as cases", () =>
    withStore(Effect.gen(function*() {
      const requests = yield* Requests.Service
      const capture = "capture_filter.pcap.gz"
      const { second } = yield* twoPreps(capture)
      const created = yield* requests.create({
        lane: "fake",
        selection: { kind: "filter", scope: "failed", whole: [] },
        knobs: KNOBS,
        createdBy: "test",
        cells: cells(capture, ["K1@x", "K2@x", "K3@x"])
      })
      const resolved = (yield* requests.resolve(created.id, second))!
      const one = resolved.captures[0]!
      expect(one.kind).toEqual("cases")
      expect(one.planned.map((cell) => cell.callKey)).toEqual(["K1@x", "K3@x"])
      expect(one.gone.map((cell) => [cell.callKey, cell.gone])).toEqual([["K2@x", "refused"]])
      expect(one.added).toEqual([])
      expect((yield* requests.show(created.id))!.request.cells).toEqual(3)
    })))

  it("answers a capture the pin holds no cut of as gone, and a whole capture refused whole grows when a re-cut makes cases of it", () =>
    withStore(Effect.gen(function*() {
      const requests = yield* Requests.Service
      const cuts = yield* Cuts.Service
      const capture = "capture_uncut.pcap.gz"
      const created = yield* requests.create({
        lane: "fake",
        selection: { kind: "captures", captures: [capture], whole: [capture] },
        knobs: KNOBS,
        createdBy: "test",
        cells: cells(capture, ["K1@x"])
      })
      const empty = yield* cuts.open()
      yield* cuts.record(cutOf(empty, capture, []))
      const nothing = (yield* requests.resolve(created.id, empty))!
      expect(nothing.captures[0]).toMatchObject({
        kind: "cases",
        planned: [],
        gone: [{ callKey: "K1@x", gone: "not-in-this-cut", reasons: [] }],
        added: []
      })
      const recut = yield* cuts.open()
      yield* cuts.record(cutOf(recut, capture, [caseOf(`${capture}-case1`, "K1@x"), caseOf(`${capture}-case2`, "K2@x")]))
      const grown = (yield* requests.resolve(created.id, recut))!
      expect(grown.captures[0]).toMatchObject({ kind: "full", added: ["K2@x"], gone: [] })
    })))

  it("records what a job resolved each cell to, one row per cell, and marks the ones it ran", () =>
    withStore(Effect.gen(function*() {
      const requests = yield* Requests.Service
      const jobs = yield* Jobs.Service
      const capture = "capture_job.pcap.gz"
      const { second } = yield* twoPreps(capture)
      const created = yield* requests.create({
        lane: "fake",
        selection: { kind: "captures", captures: [capture], whole: [capture] },
        knobs: KNOBS,
        createdBy: "test",
        cells: cells(capture, ["K1@x", "K2@x"])
      })
      const resolution = (yield* requests.resolve(created.id, second))!
      const job = yield* jobs.create({ kind: "replay", requestId: created.id, lane: "fake", args: {}, createdBy: "test" })
      expect(yield* requests.recordJob(job, resolution)).toEqual(4)
      yield* requests.ran(job, [{ capture, callKey: "K1@x" }])
      expect((yield* requests.jobCells(job)).map((cell) => [cell.callKey, cell.caseId, cell.outcome, cell.reasons]))
        .toEqual([
          ["K1@x", `${capture}-cut9`, "ran", []],
          ["K2@x", null, "gone: refused", ["source-truncated-at-edge", "source-call-incomplete"]],
          ["K3@x", `${capture}-case3`, "planned", []],
          ["K4@x", `${capture}-case4`, "planned", []]
        ])
      expect((yield* requests.show(created.id))!.jobs).toEqual([{ jobId: job, planned: 2, ran: 1, gone: 1 }])
      // What a resume replays: every cell of the request no job ran —
      // the gone one and the planned ones alike, since the next resolution decides them anew.
      expect((yield* requests.unran(created.id)).map((cell) => cell.callKey)).toEqual(["K2@x", "K3@x", "K4@x"])
      // A second job that runs one more leaves two; a cell ran under ANY job is done.
      const again = yield* jobs.create({ kind: "replay", requestId: created.id, lane: "fake", args: {}, createdBy: "test", restartOf: job })
      yield* requests.recordJob(again, resolution)
      yield* requests.ran(again, [{ capture, callKey: "K3@x" }])
      expect((yield* requests.unran(created.id)).map((cell) => cell.callKey)).toEqual(["K2@x", "K4@x"])
      expect((yield* jobs.show(again))!.job.restartOf).toBe(job)
    })))

  it("freezes 5 000 cells in one insert", () =>
    withStore(Effect.gen(function*() {
      const requests = yield* Requests.Service
      const many = Array.from({ length: 5000 }, (_, index) => ({
        capture: `capture_${index % 50}.pcap.gz`,
        callKey: `K${index}@x`
      }))
      // Every statement the client runs opens a `sql.execute` span; count them.
      const executed: Array<string> = []
      const tracer = Tracer.make({
        span(options) {
          if (options.name === "sql.execute") executed.push(options.name)
          return new Tracer.NativeSpan(options)
        }
      })
      const created = yield* requests.create({
        lane: "fake",
        selection: { kind: "cells", file: "ticked.txt", whole: [] },
        knobs: KNOBS,
        createdBy: "test",
        cells: many
      }).pipe(Effect.withTracer(tracer))
      expect(created.cells).toEqual(5000)
      expect(created.captures).toHaveLength(50)
      // The request row, the cells, the count: three statements, not 5 002.
      expect(executed).toHaveLength(3)
    })))
})
