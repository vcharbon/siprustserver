/**
 * Forgetting captures against a real postgres: every capture name a table
 * keys a row by, a read-only plan whose counts are exactly what the forget
 * then drops, the jobs, requests, sweeps and preps that name only forgotten
 * captures dropped and the mixed ones kept, content a kept job cell names
 * kept, the refusals and the writer lock-out that remove nothing,
 * and a forget that leaves a kept capture's rows, and the content it shares,
 * untouched. The suite builds
 * and drops its own database.
 */
import * as Deferred from "effect/Deferred"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as CaseIds from "../src/case-ids.js"
import * as CaseMaps from "../src/case-maps.js"
import * as Citations from "../src/citations.js"
import * as Client from "../src/client.js"
import * as Cuts from "../src/cuts.js"
import * as Forget from "../src/forget.js"
import * as Jobs from "../src/jobs.js"
import { type CaseResult, type CutCaseRequest, wholeFlows } from "../src/model.js"
import * as Pins from "../src/pins.js"
import * as Population from "../src/population.js"
import * as Resolver from "../src/resolver.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import { CURRENT } from "./identity.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_forget_test")

type Services =
  | CaseIds.Service
  | CaseMaps.Service
  | Citations.Service
  | Cuts.Service
  | Forget.Service
  | Jobs.Service
  | Pins.Service
  | Population.Service
  | Resolver.Service
  | Runs.Service
  | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([
        CaseIds.layer,
        CaseMaps.layer,
        Citations.layer,
        Cuts.layer,
        Forget.layer,
        Jobs.layer,
        Pins.layer,
        Population.layer,
        Resolver.layer,
        Runs.layer
      ]),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

const served = Server.served

/** Kept: in the population, cut, run. */
const KEPT = "capture-a.pcap.anon2"
/** Forgotten: cut, run twice (one pinned, one cited), in a job and a request, summarised. */
const GONE = "capture-b.pcap.anon"
/** Forgotten only with the population: still in it. */
const OLD = "capture-c.pcap.anon2"
/** Forgotten: named only by the history tables. */
const RAW = "capture-a.pcap"

const bytes = (text: string): Uint8Array => new TextEncoder().encode(text)

/** A case document whose content is `body` alone: two captures cutting one body share its hash. */
const documentOf = (body: string) => ({ id: body, actors: [], steps: [] })

const caseOf = (caseId: string, callKey: string, body: string): CutCaseRequest => ({
  caseId,
  callKey,
  document: documentOf(body),
  assets: [{ path: "resources/a.sdp", bytes: bytes(`v=0 ${body}`) }],
  ledger: [],
  egressEndpoint: null
})

const result = (caseId: string, document: unknown): CaseResult => ({
  caseId,
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  rfcGating: 0,
  mainError: null,
  classification: null,
  document
})

interface Seeded {
  readonly prep: number
  readonly recut: number
  readonly kept: number
  readonly cited: number
  readonly pinned: number
  /** Names a capture kept and one forgotten: kept, mixed; its request too. */
  readonly job: number
  /** Names the forgotten capture alone. */
  readonly goneJob: number
  /** Names no capture. */
  readonly quietJob: number
  readonly goneRequest: number
  readonly goneSweep: number
}

const seed = Effect.gen(function*() {
  const sql = yield* SqlClient
  const cuts = yield* Cuts.Service
  const runs = yield* Runs.Service
  const pins = yield* Pins.Service
  const population = yield* Population.Service
  yield* sql`
    TRUNCATE population, preps, runs, jobs, requests, sweeps, case_maps, case_ids, frozen_captures,
             run_summaries, cell_summaries, citations, case_documents, flows_documents CASCADE
  `
  yield* population.add([
    { capture: KEPT, sourcePath: null, ingested: true, extractorHash: "e" },
    { capture: OLD, sourcePath: null, ingested: true, extractorHash: "e" }
  ])
  const prep = yield* cuts.open()
  yield* cuts.record({
    prepId: prep,
    capture: KEPT,
    flows: wholeFlows(bytes(`{"capture":"${KEPT}"}`)),
    cases: [caseOf(`${KEPT}-case1`, "call-1@a", "shared"), caseOf(`${KEPT}-case2`, "call-2@a", "kept-only")],
    refusals: []
  })
  yield* cuts.record({
    prepId: prep,
    capture: GONE,
    flows: wholeFlows(bytes(`{"capture":"${GONE}"}`)),
    cases: [caseOf(`${GONE}-case1`, "call-1@b", "shared"), caseOf(`${GONE}-case2`, "call-2@b", "gone-only")],
    refusals: [{ caseId: `${GONE}-cut3`, reasons: ["refused-why"], callIds: ["call-3@b"], detail: "", callKey: "call-3@b" }]
  })
  yield* cuts.record({
    prepId: prep,
    capture: OLD,
    flows: null,
    cases: [caseOf(`${OLD}-case1`, "call-1@c", "old-only")],
    refusals: []
  })
  yield* cuts.finish(prep)
  // A re-cut of the forgotten capture alone: a prep left with no row.
  const recut = yield* cuts.open()
  yield* cuts.record({
    prepId: recut,
    capture: GONE,
    flows: wholeFlows(bytes(`{"capture":"${GONE}"}`)),
    cases: [caseOf(`${GONE}-case1`, "call-1@b", "shared"), caseOf(`${GONE}-case2`, "call-2@b", "gone-only")],
    refusals: [{ caseId: `${GONE}-cut3`, reasons: ["refused-why"], callIds: ["call-3@b"], detail: "", callKey: "call-3@b" }]
  })
  yield* cuts.finish(recut)
  const sweep = (selection: string) =>
    Effect.map(
      sql<{ id: string }>`
        INSERT INTO sweeps (lane, universe, captures, concurrency, batch_size, memory_max, finished_at, selection)
        VALUES ('fake-lane', 'named', 1, 1, 1, '1G', now(), ${selection}) RETURNING id
      `,
      (rows) => Number(rows[0]!.id)
    )
  const goneSweep = yield* sweep(`${GONE} --case ${GONE}-case1`)
  const mixedSweep = yield* sweep(`${KEPT} ${GONE}`)
  const run = (capture: string, documents: ReadonlyArray<unknown>, sweepId?: number) =>
    Effect.gen(function*() {
      const opened = yield* runs.open({
        capture,
        lane: "fake-lane",
        mode: "record",
        sessionDir: `/w/${capture}`,
        prepId: prep,
        identity: CURRENT,
        ...(sweepId === undefined ? {} : { sweepId })
      })
      yield* runs.finish({
        id: opened.id,
        status: "done",
        cases: documents.map((document, at) => result(`${capture}-case${at + 1}`, document))
      })
      return opened.id
    })
  const kept = yield* run(KEPT, [documentOf("shared"), null], mixedSweep)
  const cited = yield* run(GONE, [documentOf("shared"), documentOf("ran-gone")], goneSweep)
  const pinned = yield* run(GONE, [null])
  yield* pins.pin([pinned], "proj/9")
  const requestOf = (selection: unknown) =>
    Effect.map(
      sql<{ id: string }>`
        INSERT INTO requests (lane, selection, knobs, created_by)
        VALUES ('fake-lane', ${JSON.stringify(selection)}::jsonb, '{}'::jsonb, 'test') RETURNING id
      `,
      (rows) => Number(rows[0]!.id)
    )
  const jobOf = (kind: string, request: number | null, args: unknown) =>
    Effect.map(
      sql<{ id: string }>`
        INSERT INTO jobs (kind, request_id, args, status, created_by)
        VALUES (${kind}, ${request}, ${JSON.stringify(args)}::jsonb, 'done', 'test') RETURNING id
      `,
      (rows) => Number(rows[0]!.id)
    )
  const request = yield* requestOf({})
  const job = yield* jobOf("replay", request, {})
  const goneRequest = yield* requestOf({ kind: "captures", captures: [GONE], whole: [] })
  yield* sql`
    INSERT INTO request_cells (request_id, capture, call_key, origin) VALUES (${goneRequest}, ${GONE}, 'call-2', 'selected')
  `
  const goneJob = yield* jobOf("cut", null, { captures: [GONE], mode: "force" })
  const quietJob = yield* jobOf("cut", null, { all: true, mode: "stale" })
  for (const capture of [KEPT, GONE]) {
    yield* sql`
      INSERT INTO job_cells (job_id, request_id, capture, call_key, outcome)
      VALUES (${job}, ${request}, ${capture}, 'call-1', 'ran')
    `
    yield* sql`
      INSERT INTO request_cells (request_id, capture, call_key, origin) VALUES (${request}, ${capture}, 'call-1', 'selected')
    `
  }
  yield* Effect.flatMap(CaseMaps.Service, (maps) => maps.export)
  yield* Effect.flatMap(CaseIds.Service, (ids) => ids.markSeeded([KEPT, GONE]))
  yield* Effect.flatMap(Citations.Service, (citations) =>
    Effect.andThen(
      citations.record([
        { kind: "run", ref: String(cited), citedBy: "proj/1", citedOn: null },
        { kind: "run", ref: String(kept), citedBy: "proj/2", citedOn: null }
      ]),
      citations.summarise
    ))
  // The history of a name no live row keys any more.
  yield* sql`INSERT INTO case_ids (capture, case_id, call_key, origin) VALUES (${RAW}, ${`${RAW}-case1`}, 'call-1@r', 'seed')`
  yield* sql`
    INSERT INTO case_maps (prep_id, capture, case_id, kind, call_key, cut_at)
    VALUES (${prep}, ${RAW}, ${`${RAW}-case1`}, 'case', 'call-1@r', now())
  `
  yield* sql`INSERT INTO frozen_captures (capture) VALUES (${RAW})`
  yield* sql`
    INSERT INTO run_summaries (run_id, capture, lane, set_kind, status, cases, failed, started_at)
    VALUES (424242, ${RAW}, 'fake-lane', 'full', 'done', 1, 0, now())
  `
  yield* sql`
    INSERT INTO cell_summaries (run_id, case_id, capture, lane, passed, verdict, started_at)
    VALUES (424242, ${`${RAW}-case1`}, ${RAW}, 'fake-lane', true, 'ok', now())
  `
  return { prep, recut, kept, cited, pinned, job, goneJob, quietJob, goneRequest, goneSweep } satisfies Seeded
})

/** Every table's row count, as one record. */
const counts = Effect.gen(function*() {
  const sql = yield* SqlClient
  const out: Record<string, number> = {}
  for (const table of Forget.TABLES) {
    const rows = yield* sql<{ n: number }>`SELECT count(*)::int AS n FROM ${sql(table)}`
    out[table] = Number(rows[0]!.n)
  }
  return out as Forget.Rows
})

/** The rows any table keys by `capture`, summed. */
const keyedBy = (capture: string) =>
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const rows = yield* sql<{ n: number }>`
      SELECT ((SELECT count(*) FROM population WHERE capture = ${capture})
        + (SELECT count(*) FROM capture_cuts WHERE capture = ${capture})
        + (SELECT count(*) FROM cut_cases WHERE capture = ${capture})
        + (SELECT count(*) FROM cut_refusals WHERE capture = ${capture})
        + (SELECT count(*) FROM runs WHERE capture = ${capture})
        + (SELECT count(*) FROM job_cells WHERE capture = ${capture})
        + (SELECT count(*) FROM request_cells WHERE capture = ${capture})
        + (SELECT count(*) FROM case_maps WHERE capture = ${capture})
        + (SELECT count(*) FROM case_ids WHERE capture = ${capture})
        + (SELECT count(*) FROM frozen_captures WHERE capture = ${capture})
        + (SELECT count(*) FROM run_summaries WHERE capture = ${capture})
        + (SELECT count(*) FROM cell_summaries WHERE capture = ${capture}))::int AS n
    `
    return Number(rows[0]!.n)
  })

const forget = <A, E>(use: (service: Forget.Interface) => Effect.Effect<A, E>) =>
  withStore(Effect.flatMap(Forget.Service, use))

beforeAll(async () => {
  if (!served) return
  await Server.fresh(SCRATCH)
  await Server.on(SCRATCH, Server.migrated)
}, 60_000)

afterAll(async () => {
  if (!served) return
  await Server.drop(SCRATCH)
}, 60_000)

describe.skipIf(!served)("Forget", () => {
  it("names every capture a table keys a row by, history tables included", async () => {
    await withStore(seed)
    expect(await forget((service) => service.names)).toEqual([RAW, KEPT, GONE, OLD])
  })

  it("plans read-only: per-table counts, the runs, the pins and the cited ids, nothing written", async () => {
    const s = await withStore(seed)
    const before = await withStore(counts)
    const plan = await forget((service) =>
      service.plan({
        captures: [GONE, RAW, GONE],
        citations: [
          { kind: "prep", ref: String(s.prep), citedBy: "proj/3", citedOn: null },
          { kind: "job", ref: String(s.job), citedBy: "proj/4", citedOn: null },
          { kind: "run", ref: "424242", citedBy: "proj/5", citedOn: null },
          { kind: "run", ref: String(s.pinned), citedBy: "proj/old", citedOn: "2000-01-01" }
        ]
      })
    )
    expect(plan.captures).toEqual([RAW, GONE])
    expect(plan.population).toEqual([])
    expect(plan.rows).toMatchObject({
      population: 0,
      capture_cuts: 2,
      cut_cases: 4,
      cut_refusals: 2,
      cut_refusal_calls: 2,
      runs: 2,
      case_results: 3,
      pins: 1,
      job_cells: 1,
      request_cells: 2,
      case_maps: 7,
      frozen_captures: 2,
      run_summaries: 2,
      cell_summaries: 3,
      cases: 1,
      case_assets: 1,
      case_documents: 2,
      flows_documents: 1,
      jobs: 1,
      requests: 1,
      sweeps: 1,
      preps: 1
    })
    // Every capture it names forgotten: dropped. One kept too: kept whole, counted.
    expect(plan.invocations).toEqual({
      jobs: [s.goneJob],
      requests: [s.goneRequest],
      sweeps: [s.goneSweep],
      preps: [s.recut]
    })
    expect(plan.mixed).toEqual({ jobs: 1, requests: 1, sweeps: 1 })
    expect(plan.runs.map((one) => [one.id, one.capture, one.bundlePath])).toEqual([
      [s.cited, GONE, expect.stringMatching(new RegExp(`^/w/${GONE.replaceAll(".", "\\.")}/run${s.cited}`))],
      [s.pinned, GONE, expect.stringMatching(new RegExp(`^/w/${GONE.replaceAll(".", "\\.")}/run${s.pinned}`))]
    ])
    expect(plan.pins).toEqual([{ runId: s.pinned, ticket: "proj/9" }])
    // A citation dated before the store's first cut names another store's run.
    expect(plan.cited).toEqual([
      { kind: "job", ref: String(s.job), citedBy: "proj/4" },
      { kind: "prep", ref: String(s.prep), citedBy: "proj/3" },
      { kind: "run", ref: String(s.cited), citedBy: "proj/1" },
      { kind: "run", ref: "424242", citedBy: "proj/5" }
    ])
    expect(plan.holders).toEqual([])
    expect(await withStore(counts)).toEqual(before)
  })

  it("drops exactly what it planned, and leaves the kept capture and the content it shares untouched", async () => {
    const s = await withStore(seed)
    const before = await withStore(counts)
    const kept = await withStore(keyedBy(KEPT))
    const keptCase = await withStore(Effect.flatMap(Cuts.Service, (cuts) => cuts.standing(KEPT)))
    const done = await forget((service) => service.execute({ captures: [GONE, RAW, OLD], population: true }))
    const after = await withStore(counts)
    for (const table of Forget.TABLES) expect([table, after[table]]).toEqual([table, before[table] - done.rows[table]])
    for (const capture of [GONE, RAW, OLD]) expect([capture, await withStore(keyedBy(capture))]).toEqual([capture, 0])
    expect(await withStore(keyedBy(KEPT))).toBe(kept)
    expect(await withStore(Effect.flatMap(Cuts.Service, (cuts) => cuts.standing(KEPT)))).toEqual(keptCase)
    expect(await forget((service) => service.names)).toEqual([KEPT])
    const left = await withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      const ids = (rows: ReadonlyArray<{ id: string }>) => rows.map((row) => Number(row.id))
      return {
        jobs: ids(yield* sql<{ id: string }>`SELECT id FROM jobs ORDER BY id`),
        preps: ids(yield* sql<{ id: string }>`SELECT id FROM preps ORDER BY id`),
        sweeps: ids(yield* sql<{ id: string }>`SELECT id FROM sweeps ORDER BY id`)
      }
    }))
    expect(left.jobs).toEqual([s.job, s.quietJob])
    expect(left.preps).toEqual([s.prep])
    expect(left.sweeps).not.toContain(s.goneSweep)
    expect(left.sweeps).toHaveLength(1)
    const sql = await withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      return yield* sql<{ pins: number; kept: number }>`
        SELECT (SELECT count(*)::int FROM pins) AS pins,
               (SELECT count(*)::int FROM case_results WHERE run_id = ${s.kept} AND document_hash IS NOT NULL) AS kept
      `
    }))
    expect(sql[0]).toEqual({ pins: 0, kept: 1 })
    // Its case ids resolve to nothing now; the kept capture's still name their call.
    const resolve = (input: string) => withStore(Effect.flatMap(Resolver.Service, (resolver) => resolver.resolve(input)))
    expect((await resolve(`${GONE}-case2`)).status).toBe("not-found")
    expect((await resolve(`${GONE}-cut3`)).status).toBe("not-found")
    expect(await resolve(`${KEPT}-case1`)).toMatchObject({ status: "resolved", calls: [{ callKey: "call-1@a" }] })
  })

  it("refuses, removing nothing: a population capture without leave, no capture at all, a running job", async () => {
    await withStore(seed)
    const refused = async (request: Forget.Request) => {
      const exit = await forget((service) => Effect.exit(service.execute(request)))
      expect(Exit.isFailure(exit)).toBe(true)
      return String(Exit.isFailure(exit) ? exit.cause : "")
    }
    const before = await withStore(counts)
    expect(await refused({ captures: [GONE, OLD] })).toContain(`still in the population, not allowed: ${OLD}`)
    expect(await refused({ captures: [] })).toContain("no capture named")
    expect(await withStore(counts)).toEqual(before)
    const job = await withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const id = yield* jobs.create({ kind: "replay", lane: "fake-lane", args: {}, createdBy: "test" })
      yield* jobs.started(id, { pid: 1 })
      return id
    }))
    const held = await withStore(counts)
    expect(await refused({ captures: [GONE] })).toContain(`job(s) ${job} queued or running`)
    expect(await withStore(counts)).toEqual(held)
  })

  it("keeps a case a kept capture's job cell names, though only forgotten cuts cut it", async () => {
    const s = await withStore(seed)
    const hash = await withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      const [row] = yield* sql<{ case_hash: string }>`SELECT case_hash FROM cut_cases WHERE case_id = ${`${GONE}-case2`}`
      yield* sql`
        INSERT INTO job_cells (job_id, request_id, capture, call_key, case_hash, outcome)
        SELECT id, request_id, ${KEPT}, 'call-9', ${row!.case_hash}, 'ran' FROM jobs WHERE id = ${s.job}
      `
      return row!.case_hash
    }))
    const plan = await forget((service) => service.plan({ captures: [GONE, RAW] }))
    expect(plan.rows.cases).toBe(0)
    await forget((service) => service.execute({ captures: [GONE, RAW] }))
    const left = await withStore(Effect.flatMap(SqlClient, (sql) =>
      sql<{ n: number }>`SELECT count(*)::int AS n FROM cases WHERE hash = ${hash}`))
    expect(left[0]!.n).toBe(1)
  })

  it("refuses, removing nothing, while anything may still write: a queued job, a prep, a sweep, a run of its own", async () => {
    const holders: ReadonlyArray<readonly [string, Effect.Effect<string, unknown, Services>]> = [
      ["a queued job", Effect.map(
        Effect.flatMap(Jobs.Service, (jobs) => jobs.create({ kind: "cut", args: {}, createdBy: "test" })),
        (id) => `job(s) ${id} queued or running`
      )],
      ["an unfinished prep", Effect.map(
        Effect.flatMap(Cuts.Service, (cuts) => cuts.open()),
        (id) => `prep(s) ${id} not finished`
      )],
      ["an open sweep", Effect.map(
        Effect.flatMap(SqlClient, (sql) =>
          sql<{ id: string }>`
            INSERT INTO sweeps (lane, universe, captures, concurrency, batch_size, memory_max)
            VALUES ('fake-lane', 'named', 1, 1, 1, '1G') RETURNING id
          `),
        (rows) => `sweep(s) ${rows[0]!.id} not ended`
      )],
      ["an open run of a capture it drops", Effect.map(
        Effect.flatMap(Runs.Service, (runs) =>
          runs.open({ capture: GONE, lane: "fake-lane", mode: "record", sessionDir: `/w/${GONE}`, identity: CURRENT })),
        (opened) => `run(s) ${opened.id} of a capture it drops still open`
      )]
    ]
    for (const [what, hold] of holders) {
      await withStore(seed)
      const reason = await withStore(hold)
      const before = await withStore(counts)
      const exit = await forget((service) => Effect.exit(service.execute({ captures: [GONE] })))
      expect([what, Exit.isFailure(exit)]).toEqual([what, true])
      expect([what, String(Exit.isFailure(exit) ? exit.cause : "")]).toEqual([what, expect.stringContaining(reason)])
      expect([what, await withStore(counts)]).toEqual([what, before])
    }
  })

  it("holds off a job written meanwhile: it waits on the writer, then fails, removing nothing", async () => {
    await withStore(seed)
    const before = await withStore(counts)
    const exit = await withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      const written = yield* Deferred.make<void>()
      const release = yield* Deferred.make<void>()
      // A writer's transaction, open across the forget: a job row inserted, not yet committed.
      const writer = yield* Effect.forkChild(sql.withTransaction(Effect.gen(function*() {
        yield* sql`INSERT INTO jobs (kind, args, status, created_by) VALUES ('cut', '{}'::jsonb, 'done', 'test')`
        yield* Deferred.succeed(written, undefined)
        yield* Deferred.await(release)
        return yield* Effect.fail("rolled back")
      })))
      yield* Deferred.await(written)
      const forgot = yield* Effect.exit(Effect.flatMap(Forget.Service, (service) => service.execute({ captures: [GONE] })))
      yield* Deferred.succeed(release, undefined)
      yield* Fiber.await(writer)
      return forgot
    }))
    expect(Exit.isFailure(exit)).toBe(true)
    expect(String(Exit.isFailure(exit) ? exit.cause : "")).toContain("lock timeout")
    expect(await withStore(counts)).toEqual(before)
  }, 30_000)
})
