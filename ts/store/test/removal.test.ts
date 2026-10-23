/**
 * Retiring a source and deleting runs against a real postgres:
 * the rows a retire takes out of every table keyed by the capture,
 * nothing of another capture, the refusal while a running job names it; a
 * delete that takes the standing run and leaves the previous full run
 * standing with nothing recomputed, refused while the run's job runs. The
 * suite builds and drops its own database.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Cuts from "../src/cuts.js"
import * as Jobs from "../src/jobs.js"
import { type CaseResult, type CutCaseRequest, wholeFlows } from "../src/model.js"
import * as Population from "../src/population.js"
import * as Removal from "../src/removal.js"
import * as Runs from "../src/runs.js"
import { CURRENT } from "./identity.js"
import * as Sessions from "../src/sessions.js"

const SCRATCH = await Server.scratch("store_removal_test")

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

type Store = Removal.Service | Population.Service | Cuts.Service | Runs.Service | Jobs.Service | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Store>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Removal.layer),
      Effect.provide(Population.layer),
      Effect.provide(Cuts.layer),
      Effect.provide(Runs.layer),
      Effect.provide(Jobs.layer),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = Server.served

beforeAll(async () => {
  if (!served) return
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
  if (!served) return
  await on(
    "postgres",
    Effect.gen(function*() {
      yield* Server.dropDatabase(SCRATCH)
    })
  )
}, 60_000)

const GONE = "cap_gone.pcap.gz"
const KEPT = "cap_kept.pcap.gz"

const caseOf = (id: string): CutCaseRequest => ({
  caseId: id,
  callKey: `${id}@x`,
  document: { id, actors: [{ kind: "uas", endpoint: "ep2" }], steps: [] },
  assets: [],
  ledger: [],
  egressEndpoint: "ep2"
})

const result = (caseId: string, passed: boolean): CaseResult => ({
  caseId,
  passed,
  verdict: passed ? "passed" : "failed",
  skipped: null,
  failedStep: passed ? null : "s1",
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null
})

/** A finished full run of `capture` on `lane`, with one case result. */
const ran = (capture: string, lane: string, passed: boolean, jobId?: number) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const sql = yield* SqlClient
    const [run] = yield* runs.openMany([{
      capture,
      lane,
      mode: "record",
      sessionDir: `/w/${capture.replace(".pcap.gz", "")}`,
      identity: CURRENT,
      ...(jobId === undefined ? {} : { jobId })
    }])
    yield* runs.finish({ id: run!.id, status: "done", cases: [result(`${capture}-c1`, passed)] })
    yield* sql`
      INSERT INTO run_rule_hits (run_id, family, rule, bucket, hits, cells)
      VALUES (${run!.id}, 'f', 'r', 'b', 1, 1)
    `
    return run!.id
  })

/**
 * Two captures cut under one prep, one refusal each; GONE ran on two lanes
 * and KEPT on one; a request and a done job hold cells of both.
 */
const seeded = Effect.gen(function*() {
  const sql = yield* SqlClient
  const population = yield* Population.Service
  const cuts = yield* Cuts.Service
  const jobs = yield* Jobs.Service
  yield* sql`DELETE FROM jobs`
  yield* sql`DELETE FROM requests`
  yield* sql`DELETE FROM population`
  yield* sql`DELETE FROM runs`
  yield* sql`DELETE FROM preps`
  yield* population.add([
    { capture: GONE, sourcePath: "/p/gone", ingested: true, extractorHash: "e" },
    { capture: KEPT, sourcePath: "/p/kept", ingested: true, extractorHash: "e" }
  ])
  const prep = yield* cuts.open(undefined, "cut")
  for (const capture of [GONE, KEPT]) {
    yield* cuts.record({
      prepId: prep,
      capture,
      flows: wholeFlows(new TextEncoder().encode(`{"capture":"${capture}"}`)),
      cases: [caseOf(`${capture}-c1`), caseOf(`${capture}-c2`)],
      refusals: [{ caseId: `${capture}-x`, reasons: ["source-call-incomplete"], callIds: [`X@${capture}`], detail: "" }],
      cutFingerprint: "cut"
    })
  }
  yield* cuts.finish(prep)
  const request = yield* sql<{ id: string }>`
    INSERT INTO requests (lane, selection, knobs, created_by) VALUES ('fake', '{}', '{}', 'test') RETURNING id
  `
  const requestId = Number(request[0]!.id)
  const job = yield* jobs.create({ kind: "replay", requestId, lane: "fake", args: {}, createdBy: "test" })
  yield* jobs.started(job, { pid: 1 })
  yield* jobs.close(job, { status: "done", exit: 0 })
  for (const capture of [GONE, KEPT]) {
    yield* sql`INSERT INTO request_cells (request_id, capture, call_key, origin)
               VALUES (${requestId}, ${capture}, ${`${capture}-c1@x`}, 'selected')`
    yield* sql`INSERT INTO job_cells (job_id, request_id, capture, call_key, outcome)
               VALUES (${job}, ${requestId}, ${capture}, ${`${capture}-c1@x`}, 'ran')`
  }
  const goneFake = yield* ran(GONE, "fake", true, job)
  const goneRef = yield* ran(GONE, "ref", false)
  const keptFake = yield* ran(KEPT, "fake", true, job)
  return { prep, requestId, job, goneFake, goneRef, keptFake }
})

const countOf = (table: string, capture: string) =>
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const rows = yield* sql.unsafe<{ n: string }>(
      table === "case_results" || table === "run_rule_hits"
        ? `SELECT count(*) AS n FROM ${table} t JOIN runs r ON r.id = t.run_id WHERE r.capture = $1`
        : `SELECT count(*) AS n FROM ${table} WHERE capture = $1`,
      [capture]
    )
    return Number(rows[0]!.n)
  })

const TABLES = [
  "population",
  "capture_cuts",
  "cut_cases",
  "cut_refusals",
  "cut_refusal_calls",
  "runs",
  "case_results",
  "run_rule_hits",
  "job_cells",
  "request_cells"
]

describe.skipIf(!served)("Removal", () => {
  it("plans a retire with the rows of every table and the runs' bundles", () =>
    withStore(Effect.gen(function*() {
      const { goneFake, goneRef } = yield* seeded
      const removal = yield* Removal.Service
      const [plan] = yield* removal.retirePlan([GONE])
      expect(plan!.rows).toEqual({
        population: 1,
        captureCuts: 1,
        cutCases: 2,
        cutRefusals: 1,
        cutRefusalCalls: 1,
        runs: 2,
        caseResults: 2,
        runRuleHits: 2,
        jobCells: 1,
        requestCells: 1
      })
      expect(plan!.runs.map((run) => [run.id, run.lane, run.bundlePath])).toEqual([
        [goneFake, "fake", `/w/cap_gone/run${goneFake}`],
        [goneRef, "ref", `/w/cap_gone/run${goneRef}`]
      ])
      expect(plan!.holders).toEqual([])
      const [unknown] = yield* removal.retirePlan(["nobody.pcap.gz"])
      expect(Object.values(unknown!.rows).every((count) => count === 0)).toBe(true)
    })))

  it("retires a capture run on two lanes: no row of it in any table, the other capture untouched", () =>
    withStore(Effect.gen(function*() {
      yield* seeded
      const removal = yield* Removal.Service
      const runs = yield* Runs.Service
      const before: Record<string, number> = {}
      for (const table of TABLES) before[table] = yield* countOf(table, KEPT)
      const [removed] = yield* removal.retire([GONE])
      expect(removed!.rows.runs).toBe(2)
      for (const table of TABLES) {
        expect([table, yield* countOf(table, GONE)]).toEqual([table, 0])
        expect([table, yield* countOf(table, KEPT)]).toEqual([table, before[table]])
      }
      expect((yield* runs.standings()).map((row) => row.capture)).toEqual([KEPT])
      // The prep survives: it is a cut that happened, and it still cuts KEPT.
      const cuts = yield* Cuts.Service
      expect(yield* cuts.standing(KEPT)).not.toBeNull()
    })))

  it("refuses a retire while a running job names the capture, removing nothing", () =>
    withStore(Effect.gen(function*() {
      const { requestId } = yield* seeded
      const removal = yield* Removal.Service
      const jobs = yield* Jobs.Service
      const sql = yield* SqlClient
      const live = yield* jobs.create({ kind: "replay", requestId, lane: "fake", args: {}, createdBy: "test" })
      yield* jobs.started(live, { pid: 1 })
      yield* sql`INSERT INTO job_cells (job_id, request_id, capture, call_key, outcome)
                 VALUES (${live}, ${requestId}, ${GONE}, ${`${GONE}-c2@x`}, 'planned')`
      const [plan] = yield* removal.retirePlan([GONE])
      expect(plan!.holders).toEqual([{ jobId: live, kind: "replay", how: "a cell of it" }])
      const refused = yield* removal.retire([GONE]).pipe(Effect.flip)
      expect(Removal.isHeld(refused)).toBe(true)
      expect(refused.message).toContain(`held by job ${live} (replay, a cell of it)`)
      expect(yield* countOf("population", GONE)).toBe(1)
      expect(yield* countOf("runs", GONE)).toBe(2)

      // A cut job in flight names it by its arguments, or takes the whole population.
      yield* jobs.close(live, { status: "done", exit: 0 })
      const cut = yield* jobs.create({ kind: "cut", args: { captures: [GONE], mode: "force" }, createdBy: "test" })
      yield* jobs.started(cut, { pid: 1 })
      expect((yield* removal.retirePlan([GONE]))[0]!.holders).toEqual([
        { jobId: cut, kind: "cut", how: "it is one of its captures" }
      ])
      expect((yield* removal.retirePlan([KEPT]))[0]!.holders).toEqual([])
      yield* jobs.close(cut, { status: "done", exit: 0 })
      const whole = yield* jobs.create({ kind: "cut", args: { captures: [], standing: true }, createdBy: "test" })
      yield* jobs.started(whole, { pid: 1 })
      expect((yield* removal.retirePlan([KEPT]))[0]!.holders).toEqual([
        { jobId: whole, kind: "cut", how: "it cuts the whole population" }
      ])
    })))

  it("deletes the standing run: the previous full run stands, nothing recomputed", () =>
    withStore(Effect.gen(function*() {
      const { goneFake } = yield* seeded
      const removal = yield* Removal.Service
      const runs = yield* Runs.Service
      const sql = yield* SqlClient
      const newer = yield* ran(GONE, "fake", false)
      expect((yield* runs.standing(GONE, "fake"))?.runId).toBe(newer)
      const plan = yield* removal.deletePlan({ _tag: "ids", ids: [newer] })
      expect(plan.rows).toEqual({ runs: 1, caseResults: 1, runRuleHits: 1 })
      expect(plan.held).toEqual([])
      const removed = yield* removal.deleteRuns([newer])
      expect(removed.runs.map((run) => run.id)).toEqual([newer])
      const standing = yield* runs.standing(GONE, "fake")
      expect(standing?.runId).toBe(goneFake)
      expect(standing?.lastOkRunId).toBe(goneFake)
      expect(yield* runs.protectedBundles).toContain(`/w/cap_gone/run${goneFake}`)
      const rows = yield* sql<{ n: string }>`SELECT count(*) AS n FROM case_results WHERE run_id = ${newer}`
      expect(Number(rows[0]!.n)).toBe(0)
      // The job's cells keep their outcome: what a job ran is its history.
      const cells = yield* sql<{ n: string }>`SELECT count(*) AS n FROM job_cells WHERE capture = ${GONE}`
      expect(Number(cells[0]!.n)).toBe(1)
    })))

  it("selects runs by request, by job, by sweep and by age on a lane; refuses a run whose job runs or that is open", () =>
    withStore(Effect.gen(function*() {
      const { requestId, job, goneFake, keptFake, goneRef } = yield* seeded
      const removal = yield* Removal.Service
      const jobs = yield* Jobs.Service
      const runs = yield* Runs.Service
      const sql = yield* SqlClient
      const byRequest = yield* removal.deletePlan({ _tag: "request", requestId })
      expect(byRequest.runs.map((run) => run.id)).toEqual([goneFake, keptFake])
      const byJob = yield* removal.deletePlan({ _tag: "job", jobId: job })
      expect(byJob.runs.map((run) => run.jobId)).toEqual(byJob.runs.map(() => job))
      expect(byJob.runs.length).toBeGreaterThan(0)
      yield* sql`UPDATE runs SET finished_at = now() - interval '3 hours' WHERE id = ${goneRef}`
      const older = yield* removal.deletePlan({ _tag: "older", lane: "ref", hours: 2 })
      expect(older.runs.map((run) => run.id)).toEqual([goneRef])
      expect((yield* removal.deletePlan({ _tag: "older", lane: "ref", hours: 4 })).runs).toEqual([])
      const sweep = yield* sql<{ id: string }>`
        INSERT INTO sweeps (lane, universe, captures, concurrency, batch_size, memory_max)
        VALUES ('fake', 'all', 1, 1, 1, '1G') RETURNING id
      `
      yield* sql`UPDATE runs SET sweep_id = ${sweep[0]!.id} WHERE id = ${keptFake}`
      expect((yield* removal.deletePlan({ _tag: "sweep", sweepId: Number(sweep[0]!.id) })).runs.map((run) => run.id))
        .toEqual([keptFake])

      yield* sql`UPDATE jobs SET status = 'running' WHERE id = ${job}`
      const held = yield* removal.deletePlan({ _tag: "ids", ids: [goneFake, goneRef] })
      expect(held.held).toEqual([{ id: goneFake, why: `its job ${job} is running` }])
      const refused = yield* removal.deleteRuns([goneFake, goneRef]).pipe(Effect.flip)
      expect(Removal.isHeld(refused)).toBe(true)
      expect(yield* countOf("runs", GONE)).toBe(2)
      yield* jobs.close(job, { status: "done", exit: 0 })
      const [open] = yield* runs.openMany([{ capture: KEPT, lane: "fake", mode: "record", sessionDir: "/w/cap_kept" }])
      expect((yield* removal.deletePlan({ _tag: "ids", ids: [open!.id] })).held).toEqual([
        { id: open!.id, why: "it is still open and no job answers for it — reap first" }
      ])
    })))
})
