/**
 * The source list against a real postgres: every capture
 * the population holds, with its ingest state read off the extractor hash and
 * the failure column, its standing cut's counts and longest call, its
 * standing per lane; filtered by name and by staleness; paged and counted in
 * ONE statement. The suite builds and drops its own database.
 */
import * as Effect from "effect/Effect"
import * as Tracer from "effect/Tracer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Cuts from "../src/cuts.js"
import { type CutCaseRequest, wholeFlows } from "../src/model.js"
import * as Population from "../src/population.js"
import * as Runs from "../src/runs.js"
import { CURRENT } from "./identity.js"
import * as Sessions from "../src/sessions.js"
import * as Sources from "../src/sources.js"

const SCRATCH = await Server.scratch("store_sources_test")

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

type Store = Sources.Service | Population.Service | Cuts.Service | Runs.Service | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Store>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Sources.layer),
      Effect.provide(Population.layer),
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

const EXTRACTOR = "extractor-now"
const CUT_CODE = "cut-now"

const caseOf = (id: string, spanMs: number | null): CutCaseRequest => ({
  caseId: id,
  callKey: `${id}@x`,
  document: {
    id,
    actors: [{ kind: "uas", endpoint: "ep2" }],
    steps: [],
    ...(spanMs === null ? {} : { timing: { capture_span_ms: spanMs } })
  },
  assets: [],
  ledger: [],
  egressEndpoint: "ep2"
})

/** Four captures: current, stale by extractor, failed at ingest, and never cut; two lanes ran the first. */
const seeded = Effect.gen(function*() {
  const sql = yield* SqlClient
  const population = yield* Population.Service
  const cuts = yield* Cuts.Service
  const runs = yield* Runs.Service
  yield* sql`DELETE FROM population`
  yield* sql`DELETE FROM runs`
  yield* sql`DELETE FROM preps`
  yield* population.add([
    { capture: "cap_current.pcap.gz", sourcePath: "/p/cur", ingested: true, extractorHash: EXTRACTOR },
    { capture: "cap_stale.pcap.gz", sourcePath: "/p/stale", ingested: true, extractorHash: "extractor-old" },
    {
      capture: "cap_failed.pcap.gz",
      sourcePath: "/p/failed",
      ingested: true,
      extractorHash: EXTRACTOR,
      ingestFailed: "sipflow failed: two files overlap"
    },
    { capture: "cap_uncut.pcap.gz", sourcePath: "/p/uncut", ingested: true, extractorHash: EXTRACTOR }
  ])
  const prep = yield* cuts.open(undefined, CUT_CODE)
  yield* cuts.record({
    prepId: prep,
    capture: "cap_current.pcap.gz",
    flows: wholeFlows(new TextEncoder().encode(`{"capture":"cur"}`)),
    cases: [caseOf("cur-c1", 12_500), caseOf("cur-c2", 3_000), caseOf("cur-c3", null)],
    refusals: [{ caseId: "cur-x", reasons: ["source-call-incomplete"], callIds: ["X@x"], detail: "" }],
    cutFingerprint: CUT_CODE
  })
  yield* cuts.record({
    prepId: prep,
    capture: "cap_stale.pcap.gz",
    flows: wholeFlows(new TextEncoder().encode(`{"capture":"stale"}`)),
    cases: [caseOf("stale-c1", null)],
    refusals: [],
    cutFingerprint: CUT_CODE
  })
  yield* cuts.record({
    prepId: prep,
    capture: "cap_failed.pcap.gz",
    flows: null,
    cases: [],
    refusals: [],
    cutFingerprint: CUT_CODE,
    broke: "[prep] the cut failed: unreadable"
  })
  yield* cuts.finish(prep)
  for (const lane of ["fake", "ref"]) {
    const [run] = yield* runs.openMany([{
      capture: "cap_current.pcap.gz",
      lane,
      mode: "record",
      sessionDir: "/w/cap_current",
      prepId: prep,
      identity: CURRENT
    }])
    yield* runs.finish({
      id: run!.id,
      status: "done",
      cases: [{
        caseId: "cur-c1",
        passed: lane === "fake",
        verdict: lane === "fake" ? "passed" : "failed",
        skipped: null,
        failedStep: null,
        rfcGating: null,
        mainError: null,
        classification: null,
        document: null
      }]
    })
  }
  return prep
})

const query = (rest: Partial<Sources.Query> = {}): Sources.Query => ({
  limit: 50,
  offset: 0,
  extractorHash: EXTRACTOR,
  cutFingerprint: CUT_CODE,
  ...rest
})

describe("Sources.ingestState", () => {
  const facts = (rest: Partial<Sources.IngestFacts>): Sources.IngestFacts => ({
    ingestedAt: "2026-09-20T00:00:00.000Z",
    extractorHash: EXTRACTOR,
    ingestFailed: null,
    ...rest
  })

  it("is stale where never ingested or under another extractor, failed under this one, else current", () => {
    expect(Sources.ingestState(facts({}), EXTRACTOR)).toBe("current")
    expect(Sources.ingestState(facts({ ingestedAt: null }), EXTRACTOR)).toBe("stale")
    expect(Sources.ingestState(facts({ extractorHash: null }), EXTRACTOR)).toBe("stale")
    expect(Sources.ingestState(facts({ extractorHash: "extractor-old" }), EXTRACTOR)).toBe("stale")
    expect(Sources.ingestState(facts({ ingestFailed: "overlap" }), EXTRACTOR)).toBe("failed")
    // A refusal recorded by an older extractor is stale: this one may take the capture.
    expect(Sources.ingestState(facts({ extractorHash: "extractor-old", ingestFailed: "overlap" }), EXTRACTOR)).toBe("stale")
  })
})

describe.skipIf(!served)("Sources.list", () => {
  it("lists every capture with its ingest, its standing cut's counts and span, and its standing per lane", () =>
    withStore(Effect.gen(function*() {
      const prep = yield* seeded
      const sources = yield* Sources.Service
      const page = yield* sources.list(query())
      expect(page.total).toBe(4)
      expect(page.rows.map((row) => row.capture)).toEqual([
        "cap_current.pcap.gz",
        "cap_failed.pcap.gz",
        "cap_stale.pcap.gz",
        "cap_uncut.pcap.gz"
      ])
      const [current, failed, stale, uncut] = page.rows
      expect(current).toMatchObject({
        ingest: "current",
        ingestFailed: null,
        prepId: prep,
        cut: "current",
        cases: 3,
        refusals: 1,
        spanSeconds: 12.5
      })
      expect(current!.standings.map((one) => [one.lane, one.status, one.failed])).toEqual([
        ["fake", "done", 0],
        ["ref", "done", 1]
      ])
      // S6: a capture whose ingest failed is a row with its failure.
      expect(failed).toMatchObject({ ingest: "failed", ingestFailed: "sipflow failed: two files overlap", cut: "broke" })
      expect(failed!.cutBroke).toContain("unreadable")
      expect(stale).toMatchObject({ ingest: "stale", cut: "current", cases: 1, refusals: 0, spanSeconds: null })
      expect(uncut).toMatchObject({ ingest: "current", cut: "never", prepId: null, cases: 0, standings: [] })
    })))

  it("filters by staleness and by name, paged and counted", () =>
    withStore(Effect.gen(function*() {
      yield* seeded
      const sources = yield* Sources.Service
      const stale = yield* sources.list(query({ stale: true }))
      // Stale by extractor, broken cut, never cut; the current one is out.
      expect(stale.rows.map((row) => row.capture)).toEqual(["cap_failed.pcap.gz", "cap_stale.pcap.gz", "cap_uncut.pcap.gz"])
      expect(stale.total).toBe(3)
      // Other cut code: every cut reading is stale.
      const recut = yield* sources.list(query({ stale: true, cutFingerprint: "cut-next" }))
      expect(recut.total).toBe(4)
      // Another extractor: every ingest is stale, the failed one's refusal included.
      const reingest = yield* sources.list(query({ stale: true, extractorHash: "extractor-next" }))
      expect(reingest.total).toBe(4)
      expect(reingest.rows.map((row) => row.ingest)).toEqual(["stale", "stale", "stale", "stale"])
      const named = yield* sources.list(query({ q: "CAP_S" }))
      expect(named.rows.map((row) => row.capture)).toEqual(["cap_stale.pcap.gz"])
      expect(named.total).toBe(1)
      const paged = yield* sources.list(query({ limit: 2, offset: 2 }))
      expect(paged.rows.map((row) => row.capture)).toEqual(["cap_stale.pcap.gz", "cap_uncut.pcap.gz"])
      expect(paged.total).toBe(4)
      const past = yield* sources.list(query({ limit: 2, offset: 10 }))
      expect(past.rows).toEqual([])
      expect(past.total).toBe(4)
    })))

  it("is one statement for a page that has rows", () =>
    withStore(Effect.gen(function*() {
      yield* seeded
      const sources = yield* Sources.Service
      const executed: Array<string> = []
      const tracer = Tracer.make({
        span(options) {
          if (options.name === "sql.execute") executed.push(options.name)
          return new Tracer.NativeSpan(options)
        }
      })
      const page = yield* sources.list(query()).pipe(Effect.withTracer(tracer))
      expect(page.rows).toHaveLength(4)
      expect(executed).toHaveLength(1)
    })))
})
