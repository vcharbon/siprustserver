/**
 * The wipe against a real postgres: a read-only plan, the refusals that keep
 * it from losing what a citation names, what it keeps and drops, and every
 * cited form still read back to its call once the cuts and the cited runs
 * are gone. The suite builds and drops its own database.
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
import type { CaseResult, CutCaseRequest } from "../src/model.js"
import * as Pins from "../src/pins.js"
import * as Resolver from "../src/resolver.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import * as Wipe from "../src/wipe.js"
import { CURRENT } from "./identity.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_wipe_test")

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(Server.urlFor(database)))))

type Services =
  | Cuts.Service
  | Runs.Service
  | CaseIds.Service
  | CaseMaps.Service
  | Citations.Service
  | Pins.Service
  | Resolver.Service
  | Wipe.Service
  | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([
        Cuts.layer,
        Runs.layer,
        CaseIds.layer,
        CaseMaps.layer,
        Citations.layer,
        Pins.layer,
        Resolver.layer,
        Wipe.layer
      ]),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

const served = Server.served

const CAPTURE = "wiped.pcap"
const REAL = "real-lane"
const FAKE = "fake-lane"
const KEEP = { keepLanes: [REAL] }

const caseOf = (id: string, callKey: string): CutCaseRequest => ({
  caseId: id,
  callKey,
  document: { id, actors: [], steps: [] },
  assets: [{ path: "resources/a.sdp", bytes: new TextEncoder().encode(`v=0 ${id}`) }],
  ledger: [],
  egressEndpoint: null
})

const result = (caseId: string, passed: boolean): CaseResult => ({
  caseId,
  passed,
  verdict: passed ? "ok" : "failed",
  skipped: null,
  failedStep: passed ? null : "s4",
  rfcGating: 0,
  mainError: passed ? null : { kind: "died", rule: null, rfc: null, step: "s4", expected: "200", detail: "timed out" },
  classification: null,
  document: null
})

interface Seeded {
  readonly prep: number
  readonly cited: number
  readonly loose: number
  readonly real: number
  readonly pinned: number
  readonly swept: number
}

/** One capture cut once, and five finished runs of it: cited, uncited, real-lane, pinned, in a cited sweep. */
const seed = Effect.gen(function*() {
  const cuts = yield* Cuts.Service
  const runs = yield* Runs.Service
  const pins = yield* Pins.Service
  const sql = yield* SqlClient
  const prep = yield* cuts.open()
  yield* cuts.record({
    prepId: prep,
    capture: CAPTURE,
    flows: null,
    cases: [caseOf(`${CAPTURE}-case1`, "call-1@h"), caseOf(`${CAPTURE}-case2`, "call-2@h")],
    refusals: [{ caseId: `${CAPTURE}-cut3`, reasons: ["refused-why"], callIds: ["call-3@h"], detail: "", callKey: "call-3@h" }]
  })
  yield* cuts.finish(prep)
  const sweep = Number(
    (yield* sql<{ id: string }>`
      INSERT INTO sweeps (lane, universe, captures, concurrency, batch_size, memory_max)
      VALUES (${FAKE}, 'named', 1, 1, 1, '1G') RETURNING id
    `)[0]!.id
  )
  const run = (lane: string, sweepId?: number) =>
    Effect.gen(function*() {
      const opened = yield* runs.open({
        capture: CAPTURE,
        lane,
        mode: "record",
        sessionDir: "/w/wiped",
        prepId: prep,
        identity: CURRENT,
        ...(sweepId === undefined ? {} : { sweepId })
      })
      yield* runs.finish({
        id: opened.id,
        status: "done",
        cases: [result(`${CAPTURE}-case1`, false), result(`${CAPTURE}-case2`, true)]
      })
      return opened.id
    })
  const cited = yield* run(FAKE)
  const loose = yield* run(FAKE)
  const real = yield* run(REAL)
  const pinned = yield* run(FAKE)
  const swept = yield* run(FAKE, sweep)
  yield* pins.pin([pinned], "proj/1")
  return { prep, cited, loose, real, pinned, swept, sweep }
})

let store: (Seeded & { readonly sweep: number }) | undefined

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
  store = await withStore(seed)
}, 60_000)

afterAll(async () => {
  if (!served) return
  await on("postgres", Server.dropDatabase(SCRATCH))
}, 60_000)

const citations = (): ReadonlyArray<Citations.Citation> => [
  { kind: "run", ref: String(store!.cited), citedBy: "proj/2", citedOn: null },
  { kind: "sweep", ref: String(store!.sweep), citedBy: "proj/3", citedOn: null },
  { kind: "run", ref: String(store!.loose), citedBy: "proj/old", citedOn: "2000-01-01" }
]

const resolve = (input: string) => withStore(Effect.flatMap(Resolver.Service, (resolver) => resolver.resolve(input)))

/** What every cited form reads back to, as the test compares it before and after. */
const readings = async () => {
  const inputs = [
    `${CAPTURE}-case1`,
    `${CAPTURE}-cut3`,
    "case2",
    "call-1@h",
    `run${store!.real}/case1`,
    `run${store!.cited}/case1`,
    `run${store!.swept}/case2`
  ]
  const out: Record<string, { status: string; callKey: string | null }> = {}
  for (const input of inputs) {
    const answer = await resolve(input)
    out[input] = { status: answer.status, callKey: answer.calls[0]?.callKey ?? null }
  }
  return out
}

const tableCounts = () =>
  withStore(Effect.gen(function*() {
    const sql = yield* SqlClient
    const rows = yield* sql<{ relname: string; n: number }>`
      SELECT 'runs' AS relname, count(*)::int AS n FROM runs
      UNION ALL SELECT 'preps', count(*)::int FROM preps
      UNION ALL SELECT 'cut_cases', count(*)::int FROM cut_cases
      UNION ALL SELECT 'cases', count(*)::int FROM cases
      UNION ALL SELECT 'case_assets', count(*)::int FROM case_assets
      UNION ALL SELECT 'case_documents', count(*)::int FROM case_documents
      UNION ALL SELECT 'case_maps', count(*)::int FROM case_maps
      UNION ALL SELECT 'case_ids', count(*)::int FROM case_ids
      UNION ALL SELECT 'case_results', count(*)::int FROM case_results
      UNION ALL SELECT 'cell_summaries', count(*)::int FROM cell_summaries
    `
    return Object.fromEntries(rows.map((row) => [row.relname, Number(row.n)]))
  }))

describe.skipIf(!served)("the wipe", () => {
  let before: Awaited<ReturnType<typeof readings>> | undefined

  it("plans in a read-only transaction: what it keeps and drops, and writes nothing", async () => {
    before = await readings()
    const counts = await tableCounts()
    const plan = await withStore(Effect.flatMap(Wipe.Service, (wipe) => wipe.plan({ ...KEEP, citations: citations() })))
    expect(plan.kept).toEqual({ lane: 1, pinned: 1, open: 0 })
    expect(plan.dropped.runs).toBe(3)
    // The cited run and the cited sweep's run; a citation older than the store names none of its runs.
    expect(plan.cited).toBe(2)
    expect(plan.unsummarised).toBe(2)
    expect(plan.dropped.preps).toBe(1)
    expect(plan.dropped.cutCases).toBe(2)
    expect(plan.bundles).toHaveLength(3)
    expect(plan.unexported).toBe(0)
    expect(plan.unmapped).toBe(0)
    expect(await tableCounts()).toEqual(counts)
  })

  it("refuses, removing nothing, while a cited run it drops has no summary", async () => {
    const counts = await tableCounts()
    const exit = await withStore(Effect.exit(Effect.flatMap(Wipe.Service, (wipe) =>
      wipe.execute({ ...KEEP, citations: citations() })
    )))
    expect(Exit.isFailure(exit)).toBe(true)
    expect(String(Exit.isFailure(exit) ? exit.cause : "")).toContain("have no summary")
    expect(await tableCounts()).toEqual(counts)
  })

  it("refuses, removing nothing, while a run is open", async () => {
    const counts = await tableCounts()
    const open = await withStore(Effect.flatMap(Runs.Service, (runs) =>
      runs.open({ capture: CAPTURE, lane: FAKE, mode: "record", sessionDir: "/w/wiped", identity: CURRENT })
    ))
    const exit = await withStore(Effect.exit(Effect.flatMap(Wipe.Service, (wipe) => wipe.execute(KEEP))))
    expect(String(Exit.isFailure(exit) ? exit.cause : "")).toContain("still open")
    await withStore(Effect.flatMap(Runs.Service, (runs) => runs.finish({ id: open.id, status: "done", cases: [] })))
    const after = await tableCounts()
    expect({ ...after, runs: after["runs"]! - 1 }).toEqual(counts)
  })

  it("drops every cut and every unkept run once the cited runs are summarised, and keeps the rest", async () => {
    await withStore(Effect.gen(function*() {
      const cited = yield* Citations.Service
      yield* cited.record(citations())
      expect(yield* cited.summarise).toBe(4)
    }))
    const plan = await withStore(Effect.flatMap(Wipe.Service, (wipe) => wipe.execute(KEEP)))
    expect(plan.dropped.runs).toBe(4)
    const counts = await tableCounts()
    expect(counts).toMatchObject({
      runs: 2,
      preps: 0,
      cut_cases: 0,
      cases: 0,
      case_assets: 0,
      case_documents: 2,
      case_maps: 3,
      case_ids: 3,
      case_results: 4,
      cell_summaries: 4
    })
  })

  it("reads every cited form back to the same call after the wipe", async () => {
    const after = await readings()
    expect(after).toEqual(before)
    expect(after[`run${store!.cited}/case1`]).toEqual({ status: "resolved", callKey: "call-1@h" })
    expect(after[`${CAPTURE}-cut3`]?.status).toBe("refused")
    expect((await resolve(`run${store!.loose}`)).status).toBe("not-found")
  })
})
