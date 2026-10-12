/**
 * Jobs against a real postgres: a row moves queued → running → its end, its
 * cells and its runs reference it, and its progress is ONE statement over the
 * two indexes. The suite builds and drops its own database.
 */
import * as Effect from "effect/Effect"
import * as Tracer from "effect/Tracer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Cuts from "../src/cuts.js"
import * as Jobs from "../src/jobs.js"
import { type CaseResult, type CutCaseRequest, wholeFlows } from "../src/model.js"
import * as Requests from "../src/requests.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"

const SCRATCH = await Server.scratch("store_jobs_test")

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

type Store = Jobs.Service | Requests.Service | Cuts.Service | Runs.Service | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Store>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Jobs.layer),
      Effect.provide(Requests.layer),
      Effect.provide(Cuts.layer),
      Effect.provide(Runs.layer),
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

const KNOBS: Requests.Knobs = { concurrency: 8, batchSize: 1000, memoryMax: "4G", startsPerSecond: 0, cleanup: true }

const resultOf = (caseId: string, rest: Partial<CaseResult>): CaseResult => ({
  caseId,
  passed: false,
  verdict: "failed",
  skipped: null,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null,
  ...rest
})

const caseOf = (id: string, callKey: string): CutCaseRequest => ({
  caseId: id,
  callKey,
  document: { id, actors: [{ kind: "uas", endpoint: "ep2" }], steps: [] },
  assets: [],
  ledger: [],
  egressEndpoint: "ep2"
})

/** A capture cut with `n` cases, a request over it whole, resolved: a job's worth of cells. */
const requestOf = (capture: string, n: number) =>
  Effect.gen(function*() {
    const cuts = yield* Cuts.Service
    const requests = yield* Requests.Service
    const prep = yield* cuts.open()
    const keys = Array.from({ length: n }, (_, i) => `K${i}@x`)
    yield* cuts.record({
      prepId: prep,
      capture,
      flows: wholeFlows(new TextEncoder().encode(`{"capture":"${capture}"}`)),
      cases: keys.map((key, i) => caseOf(`${capture}-case${i}`, key)),
      refusals: [{ caseId: `${capture}-cutX`, reasons: ["source-call-incomplete"], callIds: ["KX@x"], detail: "" }]
    })
    const created = yield* requests.create({
      lane: "fake",
      selection: { kind: "captures", captures: [capture], whole: [capture] },
      knobs: KNOBS,
      createdBy: "test",
      cells: [...keys, "KX@x"].map((callKey) => ({ capture, callKey }))
    })
    return { prep, resolution: (yield* requests.resolve(created.id, prep))!, keys }
  })

describe.skipIf(!served)("Jobs", () => {
  it("moves queued → running → its end, keeping the launch facts, the stop asked for and the end", () =>
    withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const id = yield* jobs.create({ kind: "replay", lane: "fake", args: { batch: 5 }, createdBy: "test" })
      expect((yield* jobs.show(id))!.job).toMatchObject({ status: "queued", pid: null, args: { batch: 5 } })
      yield* jobs.started(id, { pid: 4242, logPath: "/w/_jobs/1/log", hooksPath: "/w/_jobs/1/hooks" })
      expect((yield* jobs.show(id))!.job).toMatchObject({ status: "running", pid: 4242, logPath: "/w/_jobs/1/log" })
      yield* jobs.waiting(id, "pid 7 has the lane")
      expect((yield* jobs.show(id))!.job.waitingOn).toBe("pid 7 has the lane")
      yield* jobs.waiting(id, null)
      yield* jobs.batch(id, 2, 3)
      expect((yield* jobs.show(id))!.job).toMatchObject({ stopRequestedAt: null, stopGrace: null, restartOf: null })
      // A stop is written before the process is signalled: when, and the grace.
      yield* jobs.stop(id, 30)
      expect((yield* jobs.show(id))!.job.stopGrace).toBe(30)
      expect((yield* jobs.show(id))!.job.stopRequestedAt).not.toBeNull()
      yield* jobs.close(id, { status: "interrupted", exit: 130 })
      const shown = (yield* jobs.show(id))!.job
      expect(shown).toMatchObject({ status: "interrupted", exit: 130, batchAt: 2, batches: 3, waitingOn: null })
      expect(shown.finishedAt).not.toBeNull()
      expect((yield* jobs.list(10)).map((row) => row.id)).toContain(id)
      expect((yield* jobs.running).map((row) => row.id)).not.toContain(id)
    })))

  it("keeps the end a fast process wrote before its launcher recorded the start", () =>
    withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const id = yield* jobs.create({ kind: "replay", lane: "fake", args: {}, createdBy: "test" })
      yield* jobs.close(id, { status: "failed", exit: 2 })
      yield* jobs.started(id, { pid: 1, logPath: "/w/_jobs/2/log" })
      expect((yield* jobs.show(id))!.job).toMatchObject({ status: "failed", exit: 2, pid: 1 })
    })))

  it("aggregates a job's progress in one statement off its cells and its runs", () =>
    withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const requests = yield* Requests.Service
      const runs = yield* Runs.Service
      const capture = "capture_progress.pcap.gz"
      const { prep, resolution, keys } = yield* requestOf(capture, 6)
      const id = yield* jobs.create({
        kind: "replay",
        requestId: resolution.requestId,
        lane: "fake",
        args: {},
        createdBy: "test"
      })
      yield* jobs.started(id, { pid: 1 })
      expect(yield* requests.recordJob(id, resolution)).toBe(7)
      const [run] = yield* runs.openMany([{
        capture,
        lane: "fake",
        mode: "record",
        sessionDir: "/w/capture_progress",
        prepId: prep,
        jobId: id
      }])
      yield* requests.ran(id, keys.slice(0, 4).map((callKey) => ({ capture, callKey })))
      const before = (yield* jobs.show(id))!.progress
      expect(before).toEqual({
        total: 7,
        landed: 4,
        pending: 2,
        goneRefused: 1,
        goneMissing: 0,
        passed: 0,
        failed: 0,
        skipped: 0,
        runs: 1,
        openRuns: 1
      })
      yield* runs.finish({
        id: run!.id,
        status: "done",
        cases: [
          resultOf(`${capture}-case0`, { passed: true, verdict: "passed" }),
          resultOf(`${capture}-case1`, { failedStep: "s1" }),
          resultOf(`${capture}-case2`, { passed: true, verdict: "skipped", skipped: "lane blocked" })
        ]
      })
      // One statement: every statement the client runs opens a `sql.execute` span.
      const executed: Array<string> = []
      const tracer = Tracer.make({
        span(options) {
          if (options.name === "sql.execute") executed.push(options.name)
          return new Tracer.NativeSpan(options)
        }
      })
      const after = (yield* jobs.show(id).pipe(Effect.withTracer(tracer)))!
      expect(after.progress).toMatchObject({ landed: 4, passed: 1, failed: 1, skipped: 1, runs: 1, openRuns: 0 })
      expect(executed).toHaveLength(1)
      // The job's runs are left open by a job that ended without closing them.
      expect(yield* runs.unfinishedOf([id])).toEqual([])
    })))

  it("counts a replay's planned cells as its total until its job_cells are written", () =>
    withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const requests = yield* Requests.Service
      const capture = "capture_waiting.pcap.gz"
      const { resolution } = yield* requestOf(capture, 6)
      const id = yield* jobs.create({
        kind: "replay",
        requestId: resolution.requestId,
        lane: "fake",
        args: {},
        createdBy: "test",
        planned: 7
      })
      yield* jobs.started(id, { pid: 1 })
      // Waiting on the lane: no cell written yet, the plan is the total.
      const waiting = (yield* jobs.show(id))!
      expect(waiting.job.planned).toBe(7)
      expect(waiting.progress).toMatchObject({ total: 7, landed: 0, pending: 7, goneRefused: 0 })
      const listed = (yield* jobs.page({ status: "active", limit: 10, offset: 0 })).rows.find((row) => row.id === id)!
      expect([listed.landed, listed.total]).toEqual([0, 7])
      // Under the lane: the cells say, gone ones included.
      yield* requests.recordJob(id, resolution)
      expect((yield* jobs.show(id))!.progress).toMatchObject({ total: 7, landed: 0, pending: 6, goneRefused: 1 })
      // A replay the launcher planned nothing for reads 0/0, as before.
      const bare = yield* jobs.create({ kind: "replay", requestId: resolution.requestId, lane: "fake", args: {}, createdBy: "test" })
      expect((yield* jobs.show(bare))!.progress).toMatchObject({ total: 0, landed: 0, pending: 0 })
    })))

  it("reads the progress of 5 000 planned cells through the two indexes", () =>
    withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      const jobs = yield* Jobs.Service
      const requests = yield* Requests.Service
      const created = yield* requests.create({
        lane: "fake",
        selection: { kind: "cells", file: "ticked.txt", whole: [] },
        knobs: KNOBS,
        createdBy: "test",
        cells: [{ capture: "capture_0.pcap.gz", callKey: "K0@x" }]
      })
      const id = yield* jobs.create({ kind: "replay", requestId: created.id, lane: "fake", args: {}, createdBy: "test" })
      yield* sql`
        INSERT INTO job_cells (job_id, request_id, capture, call_key, outcome)
        SELECT ${id}, ${created.id}, 'capture_' || (i % 50) || '.pcap.gz', 'K' || i || '@x', 'planned'
        FROM generate_series(1, 5000) AS i
      `
      expect((yield* jobs.show(id))!.progress).toMatchObject({ total: 5000, pending: 5000, landed: 0 })
      const plan = yield* jobs.explain(id)
      console.log(["[jobs] EXPLAIN (ANALYZE, BUFFERS) of Jobs.show over 5 000 planned cells:", ...plan].join("\n"))
      // The cells come through their index; `runs` holds one row here, which the
      // planner reads whole by cost, so `runs_job_idx` shows on a real store only.
      const text = plan.join("\n")
      expect(text).toContain("job_cells_job_outcome_idx")
      expect(text).not.toContain("Seq Scan on job_cells")
    })))

  it("reads a multi-run job's results run by run through the case_results key, never over the table", () =>
    withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      const jobs = yield* Jobs.Service
      const runs = yield* Runs.Service
      // The table holds ten other runs' worth of results, landed interleaved as
      // concurrent captures land them, each row carrying an error line: the shape
      // a store has after a few jobs, where a hash join over the table looks cheap.
      const others = yield* runs.openMany(
        Array.from({ length: 10 }, (_, i) => ({
          capture: `capture_bulk${i}.pcap.gz`,
          lane: "fake",
          mode: "record" as const,
          sessionDir: `/w/capture_bulk${i}`
        }))
      )
      yield* sql`
        INSERT INTO case_results (run_id, case_id, passed, verdict, error_kind, error_detail)
        SELECT (SELECT id FROM runs WHERE id IN ${sql.in(others.map((run) => run.id))} ORDER BY id OFFSET i % ${others.length} LIMIT 1), 'bulk-case' || i,
               i % 7 <> 0, CASE WHEN i % 7 = 0 THEN 'failed' ELSE 'passed' END,
               CASE WHEN i % 7 = 0 THEN 'bug' END, CASE WHEN i % 7 = 0 THEN repeat('the offered answer differs at step ', 8) END
        FROM generate_series(1, 40000) AS i
      `
      // The job under test: twenty runs of a few results each, as a request over many captures lands.
      const id = yield* jobs.create({ kind: "replay", lane: "fake", args: {}, createdBy: "test" })
      const mine = yield* runs.openMany(
        Array.from({ length: 20 }, (_, i) => ({
          capture: `capture_many${i}.pcap.gz`,
          lane: "fake",
          mode: "record" as const,
          sessionDir: `/w/capture_many${i}`,
          jobId: id
        }))
      )
      for (const run of mine) {
        yield* runs.finish({
          id: run.id,
          status: "done",
          cases: [
            resultOf(`${run.name}-case0`, { passed: true, verdict: "passed" }),
            resultOf(`${run.name}-case1`, { failedStep: "s1" }),
            resultOf(`${run.name}-case2`, { passed: true, verdict: "skipped", skipped: "lane blocked" })
          ]
        })
      }
      yield* sql.unsafe("ANALYZE runs; ANALYZE case_results")
      expect((yield* jobs.show(id))!.progress).toMatchObject({ passed: 20, failed: 20, skipped: 20, runs: 20, openRuns: 0 })
      const plan = yield* jobs.explain(id)
      console.log(["[jobs] EXPLAIN (ANALYZE, BUFFERS) of Jobs.show over 20 runs beside 40 000 other results:", ...plan].join("\n"))
      const text = plan.join("\n")
      expect(text).toContain("case_results_pkey")
      expect(text).not.toContain("Seq Scan on case_results")
    })))

  it("counts a cut job's progress as the captures its preps cut over what it planned (preps.job_id)", () =>
    withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const cuts = yield* Cuts.Service
      const id = yield* jobs.create({ kind: "cut", args: { mode: "stale-only" }, createdBy: "test" })
      expect((yield* jobs.show(id))!.job).toMatchObject({ lane: null, planned: null })
      yield* jobs.planned(id, 3)
      expect((yield* jobs.show(id))!.progress).toMatchObject({ total: 3, landed: 0, pending: 3 })
      const prep = yield* cuts.open(undefined, "cut-code", id)
      for (const capture of ["cut_a.pcap.gz", "cut_b.pcap.gz"]) {
        yield* cuts.record({ prepId: prep, capture, flows: null, cases: [], refusals: [], cutFingerprint: "cut-code" })
      }
      // Another job's prep counts for nothing here.
      const other = yield* jobs.create({ kind: "cut", args: {}, createdBy: "test" })
      const elsewhere = yield* cuts.open(undefined, "cut-code", other)
      yield* cuts.record({ prepId: elsewhere, capture: "cut_c.pcap.gz", flows: null, cases: [], refusals: [] })
      expect((yield* jobs.show(id))!.progress).toMatchObject({ total: 3, landed: 2, pending: 1 })
      const listed = (yield* jobs.list(10)).find((row) => row.id === id)!
      expect([listed.landed, listed.total]).toEqual([2, 3])
    })))

  it("pages the queue running first, then queued, then the ended newest first, and reads a job's kin and its cells", () =>
    withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const requests = yield* Requests.Service
      const runs = yield* Runs.Service
      const capture = "capture_page.pcap.gz"
      const { prep, resolution, keys } = yield* requestOf(capture, 3)
      const first = yield* jobs.create({ kind: "replay", requestId: resolution.requestId, lane: "fake", args: {}, createdBy: "test" })
      yield* jobs.started(first, { pid: 1 })
      yield* requests.recordJob(first, resolution)
      const [run] = yield* runs.openMany([{
        capture,
        lane: "fake",
        mode: "record",
        sessionDir: "/w/capture_page",
        prepId: prep,
        jobId: first
      }])
      yield* requests.ran(first, keys.slice(0, 2).map((callKey) => ({ capture, callKey })))
      yield* runs.finish({
        id: run!.id,
        status: "done",
        cases: [
          resultOf(`${capture}-case0`, { passed: true, verdict: "passed" }),
          resultOf(`${capture}-case1`, {
            failedStep: "s2",
            mainError: { kind: "died", rule: null, rfc: null, step: "s2", expected: "200 to INVITE", detail: "" }
          })
        ]
      })
      yield* jobs.close(first, { status: "interrupted", exit: 130 })
      const resumed = yield* jobs.create({
        kind: "replay",
        requestId: resolution.requestId,
        lane: "fake",
        args: { unran: true },
        createdBy: "test",
        restartOf: first
      })
      yield* jobs.started(resumed, { pid: 2 })
      const queued = yield* jobs.create({ kind: "cut", args: {}, createdBy: "test" })

      const active = yield* jobs.page({ status: "active", limit: 10, offset: 0 })
      const ids = active.rows.map((row) => row.id)
      expect(ids.indexOf(resumed)).toBeLessThan(ids.indexOf(queued))
      expect(ids).not.toContain(first)
      const ended = yield* jobs.page({ status: "ended", limit: 2, offset: 0 })
      expect(ended.rows.every((row) => row.finishedAt !== null)).toBe(true)
      expect(ended.total).toBeGreaterThanOrEqual(ended.rows.length)
      const mine = ended.rows.find((row) => row.id === first) ?? (yield* jobs.page({ status: "interrupted", limit: 50, offset: 0 })).rows.find((row) => row.id === first)!
      expect(mine).toMatchObject({ landed: 2, total: 4, failed: 1, gone: 1 })
      const past = yield* jobs.page({ status: "ended", limit: 2, offset: 100000 })
      expect(past.rows).toEqual([])
      expect(past.total).toBe(ended.total)

      expect(yield* jobs.lineage(resumed)).toEqual({
        ancestors: [{ id: first, status: "interrupted", mode: null }],
        descendants: []
      })
      expect((yield* jobs.lineage(first)).descendants).toEqual([{ id: resumed, status: "running", mode: "resume" }])

      const gone = yield* jobs.gone(first, 10)
      expect(gone).toEqual({
        cells: [{ capture, callKey: "KX@x", outcome: "gone: refused", reasons: ["source-call-incomplete"] }],
        total: 1
      })
      const red = yield* jobs.red(first, 10)
      expect(red).toEqual({
        cells: [{
          capture,
          lane: "fake",
          caseId: `${capture}-case1`,
          runId: run!.id,
          error: { kind: "died", rule: null, rfc: null, step: "s2", expected: "200 to INVITE", detail: "" },
          bodyFailing: 0
        }],
        total: 1
      })
      expect((yield* jobs.red(first, 0)).cells).toEqual([])
    })))

  it("refuses a job cell with no job behind it", () =>
    withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      const exit = yield* Effect.exit(sql`
        INSERT INTO job_cells (job_id, request_id, capture, call_key, outcome)
        VALUES (999999, 1, 'c', 'k', 'planned')
      `)
      expect(exit._tag).toBe("Failure")
    })))
})
