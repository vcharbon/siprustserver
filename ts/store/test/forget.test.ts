/**
 * Forgetting captures against a real postgres: every capture name a table
 * keys a row by, a read-only plan whose counts are exactly what the forget
 * then drops, the refusals that remove nothing, and a forget that leaves a
 * kept capture's rows, and the content it shares, untouched. The suite builds
 * and drops its own database.
 */
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
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
  readonly kept: number
  readonly cited: number
  readonly pinned: number
  readonly job: number
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
  const run = (capture: string, documents: ReadonlyArray<unknown>) =>
    Effect.gen(function*() {
      const opened = yield* runs.open({
        capture,
        lane: "fake-lane",
        mode: "record",
        sessionDir: `/w/${capture}`,
        prepId: prep,
        identity: CURRENT
      })
      yield* runs.finish({
        id: opened.id,
        status: "done",
        cases: documents.map((document, at) => result(`${capture}-case${at + 1}`, document))
      })
      return opened.id
    })
  const kept = yield* run(KEPT, [documentOf("shared"), null])
  const cited = yield* run(GONE, [documentOf("shared"), documentOf("ran-gone")])
  const pinned = yield* run(GONE, [null])
  yield* pins.pin([pinned], "proj/9")
  const request = Number(
    (yield* sql<{ id: string }>`
      INSERT INTO requests (lane, selection, knobs, created_by)
      VALUES ('fake-lane', '{}'::jsonb, '{}'::jsonb, 'test') RETURNING id
    `)[0]!.id
  )
  const job = Number(
    (yield* sql<{ id: string }>`
      INSERT INTO jobs (kind, request_id, args, status, created_by)
      VALUES ('replay', ${request}, '{}'::jsonb, 'done', 'test') RETURNING id
    `)[0]!.id
  )
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
  return { prep, kept, cited, pinned, job } satisfies Seeded
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
      capture_cuts: 1,
      cut_cases: 2,
      cut_refusals: 1,
      cut_refusal_calls: 1,
      runs: 2,
      case_results: 3,
      pins: 1,
      job_cells: 1,
      request_cells: 1,
      case_maps: 4,
      frozen_captures: 2,
      run_summaries: 2,
      cell_summaries: 3,
      cases: 1,
      case_assets: 1,
      case_documents: 2,
      flows_documents: 1
    })
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
    const before = await withStore(counts)
    const refused = async (request: Forget.Request) => {
      const exit = await forget((service) => Effect.exit(service.execute(request)))
      expect(Exit.isFailure(exit)).toBe(true)
      return String(Exit.isFailure(exit) ? exit.cause : "")
    }
    expect(await refused({ captures: [GONE, OLD] })).toContain(`still in the population, not allowed: ${OLD}`)
    expect(await refused({ captures: [] })).toContain("no capture named")
    const job = await withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const id = yield* jobs.create({ kind: "replay", lane: "fake-lane", args: {}, createdBy: "test" })
      yield* jobs.started(id, { pid: 1 })
      return id
    }))
    expect(await refused({ captures: [GONE] })).toContain(`job(s) ${job} running`)
    expect(await withStore(counts)).toEqual(before)
  })
})
