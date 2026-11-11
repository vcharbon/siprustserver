/**
 * Verification lines and the result reset against a real postgres: a cell's
 * lines land with its case result and read back in their order with their
 * marks, its final status is read off them, a batch and a job hold lines of
 * their own, and the reset empties every result while the frozen ids, the
 * cuts, the population and the sequences stand. The suite builds and drops
 * its own database.
 */
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Batches from "../src/batches.js"
import * as Client from "../src/client.js"
import * as Cuts from "../src/cuts.js"
import * as Jobs from "../src/jobs.js"
import type { CaseResult } from "../src/model.js"
import * as Pins from "../src/pins.js"
import * as ResultReset from "../src/result-reset.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import * as Verifications from "../src/verifications.js"
import { CURRENT } from "./identity.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_verifications_test")

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(Server.urlFor(database)))))

type Services =
  | Batches.Service
  | Cuts.Service
  | Jobs.Service
  | Pins.Service
  | ResultReset.Service
  | Runs.Service
  | Verifications.Service
  | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([
        Batches.layer,
        Cuts.layer,
        Jobs.layer,
        Pins.layer,
        ResultReset.layer,
        Runs.layer,
        Verifications.layer
      ]),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

const CAPTURE = "lines.pcap"

const cell = (caseId: string, lines: ReadonlyArray<Verifications.Line>): CaseResult => ({
  caseId,
  passed: false,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null,
  verifications: lines
})

const CALL_PASSES_RECORD_DIFFERS: ReadonlyArray<Verifications.Line> = [
  { name: "call", source: "interpreter", answer: "pass" },
  {
    name: "call-records",
    source: "judge",
    answer: "fail",
    message: "the duration differs",
    detail: { expected: "12", seen: "13", side: "sut", field: "duration" }
  },
  { name: "rfc-audit", source: "rfc audit", answer: "fail", waived: true, message: "a known bug of the lane" },
  { name: "timing", source: "interpreter", answer: "not-checkable", informative: true }
]

const openRun = Effect.gen(function*() {
  const runs = yield* Runs.Service
  const opened = yield* runs.open({ capture: CAPTURE, lane: "fake", mode: "record", sessionDir: "/w/lines", identity: CURRENT })
  return opened.id
})

const statusesOf = (runId: number) =>
  Effect.flatMap(SqlClient, (sql) =>
    sql<{ case_id: string; status: string }>`
      SELECT case_id, status FROM cell_statuses WHERE run_id = ${runId} ORDER BY case_id
    `)

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

describe.skipIf(!Server.served)("verification lines", () => {
  it("land with a cell's case result, read back in order with their marks, and fail the cell", () =>
    withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const lines = yield* Verifications.Service
      const runId = yield* openRun
      yield* runs.finish({
        id: runId,
        status: "done",
        cases: [
          cell("a", CALL_PASSES_RECORD_DIFFERS),
          cell("b", [{ name: "call", source: "interpreter", answer: "pass" }, {
            name: "call-records",
            source: "judge",
            answer: "pending"
          }]),
          cell("c", [{ name: "call", source: "interpreter", answer: "pass" }, {
            name: "call-records",
            source: "judge",
            answer: "fail",
            waived: true
          }]),
          cell("d", [{ name: "call", source: "interpreter", answer: "fail" }, {
            name: "call-records",
            source: "judge",
            answer: "pending"
          }]),
          cell("e", [{ name: "call", source: "interpreter", answer: "pass" }, {
            name: "call-records",
            source: "judge",
            answer: "not-checkable"
          }]),
          cell("f", [{ name: "call", source: "interpreter", answer: "pass", informative: true }])
        ]
      })
      const read = yield* lines.cells([runId])
      expect(read.get(Verifications.cellKey(runId, "a"))).toEqual(CALL_PASSES_RECORD_DIFFERS)
      expect(yield* statusesOf(runId)).toEqual([
        { case_id: "a", status: "fail" },
        { case_id: "b", status: "ok so far" },
        { case_id: "c", status: "pass" },
        { case_id: "d", status: "fail, still verifying" },
        { case_id: "e", status: "unverified" },
        { case_id: "f", status: "unverified" }
      ])
    })))

  it("are replaced whole when the cell is written again, by none where it states none", () =>
    withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const lines = yield* Verifications.Service
      const runId = yield* openRun
      yield* runs.finish({ id: runId, status: "done", cases: [cell("a", CALL_PASSES_RECORD_DIFFERS)] })
      yield* runs.finish({
        id: runId,
        status: "done",
        cases: [cell("a", [{ name: "call", source: "interpreter", answer: "pass" }])]
      })
      expect(yield* lines.of({ scope: "cell", runId, caseId: "a" })).toEqual([
        { name: "call", source: "interpreter", answer: "pass" }
      ])
      expect(yield* statusesOf(runId)).toEqual([{ case_id: "a", status: "pass" }])
      const { verifications: _, ...lineless } = cell("a", [])
      yield* runs.finish({ id: runId, status: "done", cases: [lineless] })
      expect(yield* lines.of({ scope: "cell", runId, caseId: "a" })).toEqual([])
      expect(yield* statusesOf(runId)).toEqual([])
    })))

  it("refuses a waived line that is no failure", async () => {
    const exit = await withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const runId = yield* openRun
      const refused = yield* Effect.exit(runs.finish({
        id: runId,
        status: "done",
        cases: [cell("a", [{ name: "call", source: "interpreter", answer: "pass", waived: true }])]
      }))
      yield* runs.finish({ id: runId, status: "done", cases: [] })
      return refused
    }))
    expect(Exit.isFailure(exit)).toBe(true)
  })

  it("are held by a batch and by a job apart from their cells", () =>
    withStore(Effect.gen(function*() {
      const lines = yield* Verifications.Service
      const jobs = yield* Jobs.Service
      const batches = yield* Batches.Service
      const job = yield* jobs.create({ kind: "campaign", args: {}, createdBy: "test" })
      const batch = yield* batches.open({ jobId: job, lane: "fake", cells: ["a"] })
      const gate: Verifications.Line = { name: "end-gate", source: "batch", answer: "fail", message: "a call left" }
      const guard: Verifications.Line = { name: "lab-guard", source: "collector", answer: "pass" }
      yield* lines.writeBatch(batch, [gate])
      yield* lines.writeJob(job, [guard])
      expect(yield* lines.of({ scope: "batch", batchId: batch })).toEqual([gate])
      expect(yield* lines.of({ scope: "job", jobId: job })).toEqual([guard])
      yield* batches.close(batch, null)
    })))
})

describe.skipIf(!Server.served)("the result reset", () => {
  it("lists the pins it drops, empties every result, keeps the frozen ids and cuts, and mints no id again", () =>
    withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const pins = yield* Pins.Service
      const cuts = yield* Cuts.Service
      const reset = yield* ResultReset.Service
      const sql = yield* SqlClient
      const prep = yield* cuts.open()
      yield* cuts.record({
        prepId: prep,
        capture: CAPTURE,
        flows: null,
        cases: [{
          caseId: `${CAPTURE}-case1`,
          callKey: "call-1@h",
          document: { id: "case1", actors: [], steps: [] },
          assets: [],
          ledger: [],
          egressEndpoint: null
        }],
        refusals: []
      })
      yield* cuts.finish(prep)
      const frozen = () => sql<{ case_id: string; call_key: string }>`SELECT case_id, call_key FROM case_ids ORDER BY case_id`
      const before = yield* frozen()
      const runId = yield* openRun
      yield* runs.finish({ id: runId, status: "done", cases: [cell("a", CALL_PASSES_RECORD_DIFFERS)] })
      yield* pins.pin([runId], "proj/7")

      const plan = yield* reset.plan({})
      expect(plan.pins).toEqual([expect.objectContaining({ runId, capture: CAPTURE, lane: "fake", ticket: "proj/7" })])
      expect(plan.emptied).toEqual(expect.arrayContaining(["batches", "case_results", "pins", "runs", "verifications"]))
      expect(plan.emptied).not.toContain("case_ids")
      expect(plan.dropped.verifications).toBeGreaterThan(0)
      expect((yield* sql<{ n: string }>`SELECT count(*) AS n FROM pins`)[0]!.n).toBe("1")

      yield* reset.execute({})
      const left = yield* sql<Record<string, string>>`
        SELECT (SELECT count(*) FROM runs) AS runs, (SELECT count(*) FROM case_results) AS cells,
               (SELECT count(*) FROM verifications) AS lines, (SELECT count(*) FROM batches) AS batches,
               (SELECT count(*) FROM pins) AS pins, (SELECT count(*) FROM capture_cuts) AS cuts
      `
      expect(left[0]).toEqual({ runs: "0", cells: "0", lines: "0", batches: "0", pins: "0", cuts: "1" })
      expect(yield* frozen()).toEqual(before)
      expect(before.length).toBe(1)
      const next = yield* openRun
      expect(next).toBeGreaterThan(runId)
    })))

  it("refuses while a run is open, removing nothing", async () => {
    const outcome = await withStore(Effect.gen(function*() {
      const reset = yield* ResultReset.Service
      const sql = yield* SqlClient
      const open = yield* openRun
      const exit = yield* Effect.exit(reset.execute({}))
      const held = yield* sql<{ id: string }>`SELECT id FROM runs WHERE id = ${open}`
      yield* sql`UPDATE runs SET finished_at = now() WHERE id = ${open}`
      return { exit, held: held.length }
    }))
    expect(Exit.isFailure(outcome.exit)).toBe(true)
    expect(String(Exit.isFailure(outcome.exit) ? outcome.exit.cause : "")).toContain("still open")
    expect(outcome.held).toBe(1)
  })

  it("refuses a table name it cannot quote", async () => {
    const exit = await withStore(Effect.exit(Effect.flatMap(ResultReset.Service, (reset) =>
      reset.execute({ tables: ["runs; DROP TABLE case_ids"] }))))
    expect(Exit.isFailure(exit)).toBe(true)
  })
})
