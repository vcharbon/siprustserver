/**
 * Forgetting captures, store and disk, against a real postgres and a fixture
 * tree of their own: a selection by name and by pattern plans every file it
 * owns (job records and cut logs included) and every cited case id it
 * touches, and removes nothing; a confirmed forget drops the rows, then the
 * files, leaving the captures that stay and a session directory they share
 * untouched, and the cache pass collects what only the forgotten named; a
 * forget whose rows cannot go removes no file. The suite builds and drops its
 * OWN database (`@sip/store/test-server`) and never touches the corpus.
 */
import { Citations, Client, Cuts, Forget, Jobs, Model, Population, Resolver, Runs, Sessions } from "@sip/store"
import { CURRENT } from "@sip/store/test-identity"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest"
import { CONTENT, FLOWS } from "../../src/content/content.js"
import { Extraction } from "../../src/deployment/index.js"
import * as Forgetting from "../../src/housekeeping/forget.js"
import { UPLOADS } from "../../src/ingest/source.js"
import { jobDir } from "../../src/jobs/job.js"
import { type Paths, prepDir } from "../../src/paths.js"
import { host as hostOf, pathsOf } from "../cut/host.js"

const SCRATCH = await scratch("corpus_forget_test")

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

let root: string
let paths: Paths

beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "forget-test-"))
  paths = { ...pathsOf(root), corpus: path.join(root, "artifacts"), sessions: path.join(root, "sessions") }
})
afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true })
})

/** Kept seeds as a deployment keeps them: one file per capture name, under the artifact store. */
const seedFile = (capture: string): string => path.join(paths.corpus, ".seeds", `${capture}.seed`)

const seeds = Layer.effect(
  Extraction.Service,
  Effect.gen(function*() {
    const unstated = yield* Effect.flatMap(Extraction.Service, Effect.succeed)
    return Extraction.Service.of({
      ...unstated,
      keptSeed: (_artifacts, capture) => Effect.sync(() => (fs.existsSync(seedFile(capture)) ? seedFile(capture) : undefined))
    })
  })
).pipe(Layer.provide(Extraction.layer))

type Store =
  | Forget.Service
  | Citations.Service
  | Resolver.Service
  | Population.Service
  | Cuts.Service
  | Runs.Service
  | Jobs.Service
  | Sessions.Service
  | SqlClient
  | Layer.Success<ReturnType<typeof hostOf>>

const storeLayer = () =>
  Layer.mergeAll(
    Forget.layer,
    Citations.layer,
    Resolver.layer,
    Population.layer,
    Cuts.layer,
    Runs.layer,
    Jobs.layer
  ).pipe(
    Layer.provideMerge(
      Layer.mergeAll(Client.layerFromUrl(urlFor(SCRATCH)), Sessions.layer(paths.sessions), hostOf("/nowhere"))
    )
  )

const withStore = <A, E>(effect: Effect.Effect<A, E, Store>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(seeds), Effect.provide(storeLayer())))

/** Kept, its own stem. */
const KEPT = "capture-a.pcap.anon2"
/** Forgotten by pattern: cut, run on two lanes, uploaded, seeded. */
const GONE = "capture-b.pcap.anon"
/** Forgotten by pattern: an artifact directory and a session directory, a frozen id and nothing else. */
const RAW = "capture-a.pcap"
/** Kept, sharing its stem `capture-d` with {@link SHARER}. */
const NEIGHBOUR = "capture-d.pcapng"
/** Forgotten by name: a run in the session directory it shares with {@link NEIGHBOUR}. */
const SHARER = "capture-d.pcap"
const HOUR = 3_600_000

const bytes = (text: string): Uint8Array => new TextEncoder().encode(text)

const caseOf = (id: string): Model.CutCaseRequest => ({
  caseId: id,
  callKey: `${id}@x`,
  document: { id, actors: [{ kind: "uas", endpoint: "ep2" }], steps: [] },
  assets: [],
  ledger: [],
  egressEndpoint: "ep2"
})

const aged = (target: string): void => {
  const when = new Date(Date.now() - 48 * HOUR)
  fs.utimesSync(target, when, when)
}

const writeFile = (file: string, text: string): string => {
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, text)
  return file
}

/** A finished run with its bundle on disk under `<sessions>/<stem>`. */
const ran = (capture: string, stem: string, lane: string) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const [run] = yield* runs.openMany([{
      capture,
      lane,
      mode: "record",
      sessionDir: path.join(paths.sessions, stem),
      identity: CURRENT
    }])
    yield* runs.finish({ id: run!.id, status: "done", cases: [] })
    writeFile(path.join(run!.bundlePath, "campaign.json"), "{}")
    return run!
  })

interface Seeded {
  readonly prep: number
  readonly recut: number
  readonly goneJob: number
  readonly mixedJob: number
  readonly only: string
  readonly shared: string
  readonly goneFlows: string
  readonly keptFlows: string
  readonly upload: string
  readonly gone: ReadonlyArray<Runs.OpenedRun>
  readonly kept: Runs.OpenedRun
  readonly sharer: Runs.OpenedRun
  readonly neighbour: Runs.OpenedRun
}

const seeded = Effect.gen(function*() {
  const sql = yield* SqlClient
  const population = yield* Population.Service
  const cuts = yield* Cuts.Service
  yield* sql`
    TRUNCATE population, preps, runs, jobs, requests, sweeps, case_maps, case_ids, frozen_captures,
             run_summaries, cell_summaries, citations, case_documents, flows_documents CASCADE
  `
  const upload = writeFile(path.join(paths.corpus, UPLOADS, "capture-b.pcap"), "pcap")
  for (const [capture, pcaps] of [[GONE, [upload]], [KEPT, []], [RAW, []]] as const) {
    writeFile(
      path.join(paths.corpus, capture, "ingest.json"),
      JSON.stringify({
        capture,
        pcaps: pcaps.map((file) => ({ path: file, size: 4, mtime: "2026-01-01T00:00:00.000Z", sha256: "x" })),
        extractor: "e",
        ingestedAt: "2026-01-01T00:00:00.000Z"
      })
    )
  }
  writeFile(seedFile(GONE), "seed")
  writeFile(seedFile(KEPT), "seed")
  writeFile(path.join(paths.sessions, "capture-a", "run1", "campaign.json"), "{}")
  writeFile(path.join(paths.sessions, "capture-d", SHARER, "excluded.json"), "{}")
  yield* population.add([
    { capture: KEPT, sourcePath: null, ingested: true, extractorHash: "e" },
    { capture: NEIGHBOUR, sourcePath: null, ingested: true, extractorHash: "e" }
  ])
  const prep = yield* cuts.open(undefined, "cut")
  const gone = yield* cuts.record({
    prepId: prep,
    capture: GONE,
    flows: Model.wholeFlows(bytes(`{"capture":"${GONE}"}`)),
    cases: [caseOf("shared"), caseOf("only"), caseOf(`${GONE}-case3`)],
    refusals: [],
    cutFingerprint: "cut"
  })
  yield* cuts.record({
    prepId: prep,
    capture: KEPT,
    flows: Model.wholeFlows(bytes(`{"capture":"${KEPT}"}`)),
    cases: [caseOf("shared"), caseOf(`${KEPT}-case3`)],
    refusals: [],
    cutFingerprint: "cut"
  })
  yield* cuts.finish(prep)
  // A re-cut of the forgotten capture alone, and a job naming it alone and one naming both: each with its log.
  const recut = yield* cuts.open(undefined, "cut")
  yield* cuts.record({
    prepId: recut,
    capture: GONE,
    flows: Model.wholeFlows(bytes(`{"capture":"${GONE}"}`)),
    cases: [caseOf("only")],
    refusals: [],
    cutFingerprint: "cut"
  })
  yield* cuts.finish(recut)
  writeFile(path.join(prepDir(paths.sessions, recut), "manifest.json"), "{}")
  writeFile(path.join(prepDir(paths.sessions, prep), "manifest.json"), "{}")
  const jobs = yield* Jobs.Service
  const goneJob = yield* jobs.create({ kind: "cut", args: { captures: [GONE] }, createdBy: "test" })
  const mixedJob = yield* jobs.create({ kind: "cut", args: { captures: [GONE, KEPT] }, createdBy: "test" })
  for (const id of [goneJob, mixedJob]) writeFile(path.join(jobDir(paths.sessions, id), "log"), "done")
  yield* sql`INSERT INTO case_ids (capture, case_id, call_key, origin) VALUES (${RAW}, 'raw-case1', 'raw@x', 'seed')`
  const hashes = Object.fromEntries(gone.cases.map((entry) => [entry.caseId, entry.caseHash]))
  const goneFlows = (yield* cuts.standing(GONE))!.flowsHash!
  const keptFlows = (yield* cuts.standing(KEPT))!.flowsHash!
  for (const hash of [hashes["shared"]!, hashes["only"]!]) {
    aged(path.dirname(writeFile(path.join(paths.sessions, CONTENT, hash, "case.json"), "{}")))
  }
  for (const hash of [goneFlows, keptFlows]) aged(writeFile(path.join(paths.sessions, CONTENT, FLOWS, `${hash}.json`), "{}"))
  return {
    only: hashes["only"]!,
    shared: hashes["shared"]!,
    goneFlows,
    keptFlows,
    upload,
    gone: [yield* ran(GONE, GONE, "paused"), yield* ran(GONE, GONE, "real")],
    kept: yield* ran(KEPT, KEPT, "paused"),
    sharer: yield* ran(SHARER, "capture-d", "paused"),
    neighbour: yield* ran(NEIGHBOUR, "capture-d", "paused"),
    prep,
    recut,
    goneJob,
    mixedJob
  } satisfies Seeded
})

const request = (population = false): Forgetting.Request => ({
  paths,
  names: [SHARER, "nobody.pcap"],
  patterns: [/\.anon$/, /^capture-a\.pcap$/],
  citations: [],
  population
})

const rowsOf = (capture: string) =>
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const rows = yield* sql<{ n: number }>`
      SELECT ((SELECT count(*) FROM capture_cuts WHERE capture = ${capture})
        + (SELECT count(*) FROM runs WHERE capture = ${capture})
        + (SELECT count(*) FROM case_ids WHERE capture = ${capture}))::int AS n
    `
    return Number(rows[0]!.n)
  })

describe.skipIf(!served)("Forget", () => {
  it("plans the files a selection by name and pattern owns, measured, and removes nothing", () =>
    withStore(Effect.gen(function*() {
      const s = yield* seeded
      const plan = yield* Forgetting.plan({
        ...request(),
        citations: [
          { kind: "case", ref: `${GONE}-case3`, citedBy: "proj/1", citedOn: null },
          { kind: "case", ref: `${KEPT}-case3`, citedBy: "proj/2", citedOn: null },
          { kind: "case", ref: "case3", citedBy: "proj/3", citedOn: null }
        ]
      })
      // A bare id ambiguous between a capture kept and one forgotten loses a candidate, not its call.
      expect(plan.citedCases).toEqual([
        { ref: `${GONE}-case3`, citedBy: "proj/1", capture: GONE, narrowed: false },
        { ref: "case3", citedBy: "proj/3", capture: GONE, narrowed: true }
      ])
      expect(plan.store.captures).toEqual([RAW, GONE, SHARER])
      expect(plan.matched).toEqual([{ pattern: "\\.anon$", captures: 1 }, { pattern: "^capture-a\\.pcap$", captures: 1 }])
      expect(plan.unknown).toEqual(["nobody.pcap"])
      expect(plan.files.sessionDirs.paths).toEqual([
        path.join(paths.sessions, "capture-a"),
        path.join(paths.sessions, GONE),
        path.join(paths.sessions, "capture-d", SHARER)
      ])
      expect(plan.files.bundles.paths).toEqual([s.sharer.bundlePath])
      expect(plan.bundlesOnDisk).toBe(3)
      expect(plan.files.artifactDirs.paths).toEqual([path.join(paths.corpus, RAW), path.join(paths.corpus, GONE)])
      expect(plan.files.uploads.paths).toEqual([s.upload])
      expect(plan.files.seeds.paths).toEqual([seedFile(GONE)])
      // The job naming the forgotten capture alone and the re-cut of it alone go with their logs.
      expect(plan.files.jobLogs.paths).toEqual([jobDir(paths.sessions, s.goneJob)])
      expect(plan.files.prepLogs.paths).toEqual([prepDir(paths.sessions, s.recut)])
      expect(plan.store.mixed.jobs).toBe(1)
      for (const set of Object.values(plan.files)) expect(set.bytes).toBeGreaterThan(0)
      expect(plan.refusals).toEqual([])
      expect(Forgetting.lines(plan)).toContain("[forget] nobody.pcap: nothing of it in the store or on disk")
      for (const one of Object.values(plan.files).flatMap((set) => set.paths)) expect(fs.existsSync(one)).toBe(true)
      expect(yield* rowsOf(GONE)).toBeGreaterThan(0)
    })))

  it("drops the rows, then the files; what stays, and a session directory it shares, untouched; the cache collected", () =>
    withStore(Effect.gen(function*() {
      const s = yield* seeded
      const kept = yield* rowsOf(KEPT)
      const applied = yield* Forgetting.apply({ ...request(), graceHours: 1 })
      for (const capture of [RAW, GONE, SHARER]) expect([capture, yield* rowsOf(capture)]).toEqual([capture, 0])
      for (const one of Object.values(applied.plan.files).flatMap((set) => set.paths)) {
        expect([one, fs.existsSync(one)]).toEqual([one, false])
      }
      expect(yield* rowsOf(KEPT)).toBe(kept)
      expect(yield* rowsOf(NEIGHBOUR)).toBe(1)
      expect(fs.existsSync(s.kept.bundlePath)).toBe(true)
      expect(fs.existsSync(s.neighbour.bundlePath)).toBe(true)
      expect(fs.existsSync(path.join(paths.corpus, KEPT, "ingest.json"))).toBe(true)
      expect(fs.existsSync(seedFile(KEPT))).toBe(true)
      expect(fs.existsSync(jobDir(paths.sessions, s.mixedJob))).toBe(true)
      expect(fs.existsSync(prepDir(paths.sessions, s.prep))).toBe(true)
      expect(applied.cache.cases).toEqual([path.join(paths.sessions, CONTENT, s.only)])
      expect(applied.cache.flows).toEqual([path.join(paths.sessions, CONTENT, FLOWS, `${s.goneFlows}.json`)])
      expect(fs.existsSync(path.join(paths.sessions, CONTENT, s.shared))).toBe(true)
      // Forgotten means unknown: the same selection now names only what never was.
      const again = yield* Forgetting.plan(request())
      expect(again.store.captures).toEqual([])
      expect(again.refusals).toEqual(["no capture named: the names resolve to nothing"])
    })))

  it("removes no file where the rows may not go: a capture still in the population", () =>
    withStore(Effect.gen(function*() {
      yield* seeded
      yield* Effect.flatMap(Population.Service, (population) =>
        population.add([{ capture: GONE, sourcePath: null, ingested: true, extractorHash: "e" }]))
      const listed = Object.values((yield* Forgetting.plan(request())).files).flatMap((set) => set.paths)
      const refused = yield* Forgetting.apply({ ...request(), graceHours: 1 }).pipe(Effect.flip)
      expect(refused).toBeInstanceOf(Forget.Refused)
      expect(String(refused.message)).toContain(`still in the population, not allowed: ${GONE}`)
      for (const one of listed) expect([one, fs.existsSync(one)]).toEqual([one, true])
      expect(yield* rowsOf(GONE)).toBeGreaterThan(0)
    })))

  it("removes no file where the transaction dropping the rows fails", () => {
    const failing = Layer.effect(
      Forget.Service,
      Effect.map(Forget.Service, (real) => Forget.Service.of({ ...real, execute: () => Effect.die("rows stay") }))
    ).pipe(Layer.provide(Forget.layer))
    return withStore(Effect.gen(function*() {
      yield* seeded
      const listed = Object.values((yield* Forgetting.plan(request())).files).flatMap((set) => set.paths)
      const exit = yield* Forgetting.apply({ ...request(), graceHours: 1 }).pipe(Effect.provide(failing), Effect.exit)
      expect(exit._tag).toBe("Failure")
      for (const one of listed) expect([one, fs.existsSync(one)]).toEqual([one, true])
    }))
  })
})
