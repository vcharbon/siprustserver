/**
 * Retiring a source and deleting runs, against a real postgres and a fixture
 * tree of their own: a capture run on two lanes
 * leaves no row in any table and no directory on disk, the upload it came
 * from goes while a pcap elsewhere, or an upload a kept capture names too,
 * stays, the cache pass collects the case
 * content it alone cited and keeps what another capture cites; a delete of
 * the standing run leaves the previous full run standing and protected;
 * both are refused, removing nothing, while a running job holds them. The
 * suite builds and drops its OWN database (`@sip/store/test-server`)
 * and never touches the corpus.
 */
import { Client, Cuts, Jobs, Model, Population, Removal, type RunIdentity, Runs, Sessions } from "@sip/store"
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
import * as Delete from "../../src/housekeeping/delete-runs.js"
import * as Retention from "../../src/housekeeping/retention.js"
import * as Retire from "../../src/ingest/retire.js"
import { UPLOADS } from "../../src/ingest/source.js"
import type { Paths } from "../../src/paths.js"
import { host as hostOf, pathsOf } from "../cut/host.js"

const SCRATCH = await scratch("corpus_retire_test")

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
  root = fs.mkdtempSync(path.join(os.tmpdir(), "retire-test-"))
  paths = { ...pathsOf(root), corpus: path.join(root, "artifacts"), sessions: path.join(root, "sessions") }
})
afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true })
})

/** The seams over the platform. */
const host = hostOf("/nowhere")

type Store =
  | Removal.Service
  | Population.Service
  | Cuts.Service
  | Runs.Service
  | Jobs.Service
  | SqlClient
  | Layer.Success<typeof host>

const withStore = <A, E>(effect: Effect.Effect<A, E, Store>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(
        Layer.mergeAll(Removal.layer, Population.layer, Cuts.layer, Runs.layer, Jobs.layer).pipe(
          Layer.provideMerge(Layer.mergeAll(Client.layerFromUrl(urlFor(SCRATCH)), Sessions.layer(paths.sessions), host))
        )
      )
    )
  )

const GONE = "cap_gone.pcap.gz"
const KEPT = "cap_kept.pcap.gz"
const HOUR = 3_600_000

const bytes = (text: string): Uint8Array => new TextEncoder().encode(text)

/** A case whose content is a function of `id` alone: two captures cutting the same id share its hash. */
const caseOf = (id: string): Model.CutCaseRequest => ({
  caseId: id,
  callKey: `${id}@x`,
  document: { id, actors: [{ kind: "uas", endpoint: "ep2" }], steps: [] },
  assets: [],
  ledger: [],
  egressEndpoint: "ep2"
})

const result = (caseId: string, passed: boolean): Model.CaseResult => ({
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

const aged = (target: string, hours: number): void => {
  const when = new Date(Date.now() - hours * HOUR)
  fs.utimesSync(target, when, when)
}

/** A content entry as the materialiser publishes it, aged past any grace. */
const contentDir = (hash: string): string => {
  const dir = path.join(paths.sessions, CONTENT, hash)
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(path.join(dir, "case.json"), "{}")
  aged(dir, 48)
  return dir
}

const flowsFile = (hash: string): string => {
  const file = path.join(paths.sessions, CONTENT, FLOWS, `${hash}.json`)
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, "{}")
  aged(file, 48)
  return file
}

/** A finished full run on `lane` with its bundle on disk. */
const ran = (capture: string, lane: string, passed: boolean, jobId?: number) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const sessionDir = path.join(paths.sessions, capture.replace(".pcap.gz", ""))
    const [run] = yield* runs.openMany([{
      capture,
      lane,
      mode: "record",
      sessionDir,
      identity: CURRENT,
      ...(jobId === undefined ? {} : { jobId })
    }])
    yield* runs.finish({ id: run!.id, status: "done", cases: [result("shared", passed)] })
    fs.mkdirSync(run!.bundlePath, { recursive: true })
    fs.writeFileSync(path.join(run!.bundlePath, "campaign.json"), "{}")
    return run!
  })

interface Seeded {
  readonly prep: number
  readonly shared: string
  readonly only: string
  readonly goneFlows: string
  readonly keptFlows: string
  readonly upload: string
  readonly sibling: string
  readonly goneFake: Runs.OpenedRun
  readonly goneReal: Runs.OpenedRun
  readonly keptFake: Runs.OpenedRun
}

/**
 * GONE and KEPT share the case `shared`; GONE alone has `only`; each has its
 * flows document. GONE's ingest names a pcap under `_uploads` and one beside
 * the checkout's siblings; KEPT's names nothing. GONE ran on two lanes.
 */
const seeded = Effect.gen(function*() {
  const sql = yield* SqlClient
  const population = yield* Population.Service
  const cuts = yield* Cuts.Service
  yield* sql`DELETE FROM jobs`
  yield* sql`DELETE FROM population`
  yield* sql`DELETE FROM runs`
  yield* sql`DELETE FROM preps`
  const uploads = path.join(paths.corpus, UPLOADS)
  fs.mkdirSync(uploads, { recursive: true })
  const upload = path.join(uploads, GONE)
  fs.writeFileSync(upload, "pcap")
  const siblings = path.join(root, "SamplePcapGz")
  fs.mkdirSync(siblings, { recursive: true })
  const sibling = path.join(siblings, "other.pcap.gz")
  fs.writeFileSync(sibling, "pcap")
  for (const [capture, pcaps] of [[GONE, [upload, sibling]], [KEPT, []]] as const) {
    const dir = path.join(paths.corpus, capture)
    fs.mkdirSync(dir, { recursive: true })
    fs.writeFileSync(path.join(dir, "flows.json"), "{}")
    fs.writeFileSync(
      path.join(dir, "ingest.json"),
      JSON.stringify({
        capture,
        pcaps: pcaps.map((file) => ({ path: file, size: 4, mtime: "2026-01-01T00:00:00.000Z", sha256: "x" })),
        extractor: "e",
        ingestedAt: "2026-01-01T00:00:00.000Z"
      })
    )
  }
  yield* population.add([
    { capture: GONE, sourcePath: upload, ingested: true, extractorHash: "e" },
    { capture: KEPT, sourcePath: null, ingested: true, extractorHash: "e" }
  ])
  const prep = yield* cuts.open(undefined, "cut")
  const gone = yield* cuts.record({
    prepId: prep,
    capture: GONE,
    flows: Model.wholeFlows(bytes(`{"capture":"${GONE}"}`)),
    cases: [caseOf("shared"), caseOf("only")],
    refusals: [{ caseId: "gone-x", reasons: ["source-call-incomplete"], callIds: ["X@gone"], detail: "" }],
    cutFingerprint: "cut"
  })
  yield* cuts.record({
    prepId: prep,
    capture: KEPT,
    flows: Model.wholeFlows(bytes(`{"capture":"${KEPT}"}`)),
    cases: [caseOf("shared")],
    refusals: [],
    cutFingerprint: "cut"
  })
  yield* cuts.finish(prep)
  const hashes = Object.fromEntries(gone.cases.map((entry) => [entry.caseId, entry.caseHash]))
  const goneFlows = (yield* cuts.standing(GONE))!.flowsHash!
  const keptFlows = (yield* cuts.standing(KEPT))!.flowsHash!
  contentDir(hashes["shared"]!)
  contentDir(hashes["only"]!)
  flowsFile(goneFlows)
  flowsFile(keptFlows)
  const goneFake = yield* ran(GONE, "paused", true)
  const goneReal = yield* ran(GONE, "real", false)
  const keptFake = yield* ran(KEPT, "paused", true)
  return {
    prep,
    shared: hashes["shared"]!,
    only: hashes["only"]!,
    goneFlows,
    keptFlows,
    upload,
    sibling,
    goneFake,
    goneReal,
    keptFake
  } satisfies Seeded
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

describe.skipIf(!served)("Retire", () => {
  it("plans the rows, the bundles, the directories, the upload and never a pcap elsewhere", () =>
    withStore(Effect.gen(function*() {
      const s = yield* seeded
      const [plan] = yield* Retire.plan({ paths, captures: [GONE], measure: true })
      expect(plan!.known).toBe(true)
      expect(plan!.rows).toMatchObject({ population: 1, captureCuts: 1, cutCases: 2, cutRefusals: 1, runs: 2, caseResults: 2 })
      expect(plan!.bundles).toEqual([s.goneFake.bundlePath, s.goneReal.bundlePath])
      expect(plan!.sessionDir).toBe(path.join(paths.sessions, "cap_gone"))
      expect(plan!.artifactDir).toBe(path.join(paths.corpus, GONE))
      expect(plan!.uploads).toEqual([s.upload])
      expect(plan!.bytes).toBeGreaterThan(0)
      const lines = Retire.lines(plan!)
      expect(lines[0]).toBe(`${GONE}:`)
      expect(lines.some((line) => line.includes("upload:") && line.includes(s.upload))).toBe(true)
      expect(lines.some((line) => line.includes(s.sibling))).toBe(false)
      const [unknown] = yield* Retire.plan({ paths, captures: ["nobody.pcap.gz"], measure: false })
      expect(unknown!.known).toBe(false)
      expect(Retire.lines(unknown!)).toEqual(["nobody.pcap.gz: nothing of it in the store or on disk"])
    })))

  it("retires a capture run on two lanes: no row, no directory, the upload gone, the sibling pcap untouched, the cache collected", () =>
    withStore(Effect.gen(function*() {
      const s = yield* seeded
      const runs = yield* Runs.Service
      const applied = yield* Retire.apply({ paths, captures: [GONE], measure: false, graceHours: 1 })
      for (const table of TABLES) expect([table, yield* countOf(table, GONE)]).toEqual([table, 0])
      expect(yield* countOf("cut_cases", KEPT)).toBe(1)
      expect(yield* countOf("runs", KEPT)).toBe(1)
      expect(fs.existsSync(path.join(paths.sessions, "cap_gone"))).toBe(false)
      expect(fs.existsSync(path.join(paths.corpus, GONE))).toBe(false)
      expect(fs.existsSync(s.upload)).toBe(false)
      expect(fs.existsSync(s.sibling)).toBe(true)
      expect(fs.existsSync(s.keptFake.bundlePath)).toBe(true)
      expect(fs.existsSync(path.join(paths.corpus, KEPT, "ingest.json"))).toBe(true)
      // The cache: what GONE alone cited goes, what KEPT still cites stays.
      expect(applied.cache.cases).toEqual([path.join(paths.sessions, CONTENT, s.only)])
      expect(applied.cache.flows).toEqual([path.join(paths.sessions, CONTENT, FLOWS, `${s.goneFlows}.json`)])
      expect(fs.existsSync(path.join(paths.sessions, CONTENT, s.only))).toBe(false)
      expect(fs.existsSync(path.join(paths.sessions, CONTENT, s.shared))).toBe(true)
      expect(fs.existsSync(path.join(paths.sessions, CONTENT, FLOWS, `${s.keptFlows}.json`))).toBe(true)
      expect((yield* runs.standings()).map((row) => row.capture)).toEqual([KEPT])
      // Retired means unknown: a second retire plans nothing and applies to nothing.
      const [again] = yield* Retire.plan({ paths, captures: [GONE], measure: false })
      expect(again!.known).toBe(false)
    })))

  it("keeps an upload a capture that stays names too", () =>
    withStore(Effect.gen(function*() {
      const s = yield* seeded
      const record = path.join(paths.corpus, KEPT, "ingest.json")
      const kept = JSON.parse(fs.readFileSync(record, "utf8")) as { pcaps: Array<unknown> }
      kept.pcaps.push({ path: s.upload, size: 4, mtime: "2026-01-01T00:00:00.000Z", sha256: "x" })
      fs.writeFileSync(record, JSON.stringify(kept))
      const applied = yield* Retire.apply({ paths, captures: [GONE], measure: false, graceHours: 1 })
      expect(applied.plans[0]!.uploads).toEqual([])
      expect(fs.existsSync(s.upload)).toBe(true)
      expect(fs.existsSync(path.join(paths.corpus, GONE))).toBe(false)
    })))

  it("is refused while a running job names the capture, removing nothing", () =>
    withStore(Effect.gen(function*() {
      const s = yield* seeded
      const jobs = yield* Jobs.Service
      const live = yield* jobs.create({ kind: "cut", args: { captures: [GONE], mode: "force" }, createdBy: "test" })
      yield* jobs.started(live, { pid: 1 })
      const refused = yield* Retire.apply({ paths, captures: [GONE], measure: false, graceHours: 1 }).pipe(Effect.flip)
      expect(Removal.isHeld(refused)).toBe(true)
      expect(yield* countOf("population", GONE)).toBe(1)
      expect(fs.existsSync(s.goneFake.bundlePath)).toBe(true)
      expect(fs.existsSync(s.upload)).toBe(true)
      expect(fs.existsSync(path.join(paths.sessions, CONTENT, s.only))).toBe(true)
    })))
})

describe.skipIf(!served)("Delete", () => {
  it("deletes the standing run with its bundle: the previous full run stands and is protected", () =>
    withStore(Effect.gen(function*() {
      const s = yield* seeded
      const runs = yield* Runs.Service
      const newer = yield* ran(GONE, "paused", false)
      expect((yield* runs.standing(GONE, "paused"))?.runId).toBe(newer.id)
      const plan = yield* Delete.plan({ paths, selector: { _tag: "ids", ids: [newer.id] }, measure: true })
      expect(plan.bundles).toEqual([newer.bundlePath])
      expect(plan.bytes).toBeGreaterThan(0)
      expect(Delete.lines(plan).some((line) => line.includes(`run${newer.id}`) && line.includes(newer.bundlePath))).toBe(true)
      const applied = yield* Delete.apply({ paths, selector: { _tag: "ids", ids: [newer.id] }, measure: false, graceHours: 1 })
      expect(applied.plan.rows).toEqual({ runs: 1, caseResults: 1, runRuleHits: 0 })
      expect(fs.existsSync(newer.bundlePath)).toBe(false)
      expect(fs.existsSync(s.goneFake.bundlePath)).toBe(true)
      const standing = yield* runs.standing(GONE, "paused")
      expect(standing?.runId).toBe(s.goneFake.id)
      expect(standing?.lastOkRunId).toBe(s.goneFake.id)
      // The retention pass keeps what stands: nothing is resurrected, nothing else goes.
      const retention = yield* Retention.plan({
        sessions: paths.sessions,
        livePreps: new Set([s.prep]),
        realLanes: ["real", "managed"],
        graceHours: Retention.GRACE_HOURS,
        forgetUnrecorded: false
      })
      expect(retention.removable).toEqual([])
      expect(retention.unrecorded).toEqual([])
      expect(yield* runs.protectedBundles).toContain(s.goneFake.bundlePath)
      // The cache is untouched: a run cites no content.
      expect(applied.cache.cases).toEqual([])
      expect(fs.existsSync(path.join(paths.sessions, CONTENT, s.only))).toBe(true)
    })))

  it("is refused while the run's job runs, removing nothing", () =>
    withStore(Effect.gen(function*() {
      yield* seeded
      const jobs = yield* Jobs.Service
      const live = yield* jobs.create({ kind: "replay", lane: "paused", args: {}, createdBy: "test" })
      yield* jobs.started(live, { pid: 1 })
      const run = yield* ran(GONE, "paused", true, live)
      const plan = yield* Delete.plan({ paths, selector: { _tag: "ids", ids: [run.id] }, measure: false })
      expect(plan.held).toEqual([{ id: run.id, why: `its job ${live} is running` }])
      const refused = yield* Delete.apply({ paths, selector: { _tag: "ids", ids: [run.id] }, measure: false, graceHours: 1 })
        .pipe(Effect.flip)
      expect(Removal.isHeld(refused)).toBe(true)
      expect(fs.existsSync(run.bundlePath)).toBe(true)
      expect(yield* countOf("runs", GONE)).toBe(3)
    })))
})
