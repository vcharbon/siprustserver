/**
 * The replay phase over the deployment's seams, against a real postgres and
 * a fake cell driver: each capture's run row closes on its last cell with its
 * bundle's index written beside the cells; every interpreter of a batch is
 * told its box and the batch's one side-row directory; a passing cell keeps
 * no terminated calls and a failing one keeps them compressed; the lane's
 * ceilings bound the load the rows record; and where the lane runs a batch
 * process, the rows close once its gate has read, a dirty gate marking every
 * run the batch touched.
 *
 * The suite builds and drops its OWN database (`@sip/store/test-server`).
 */
import { NodeServices } from "@effect/platform-node"
import { Campaign } from "@sip/contracts"
import { Layout, LanePresets, RoutingCompiler, type runCell } from "@sip/driver"
import { Classifier, Reclassifier } from "@sip/pipeline"
import { Cuts, Jobs, Model, Refusals, Requests, RuleHits, Runs, Sessions, Sweeps } from "@sip/store"
import { Client } from "@sip/store"
import { CURRENT } from "@sip/store/test-identity"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import { ReplayCli } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import * as zlib from "node:zlib"
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest"
import { Boxing, CaseLayout, CellHost, Identity, LaneHosting, Tools } from "../../src/deployment/index.js"
import type { Options } from "../../src/options.js"
import { TERMINATED } from "../../src/replay/cell-trim.js"
import { planWork } from "../../src/replay/plan.js"
import { phase, type StandBatch } from "../../src/replay/replay.js"

const SCRATCH = await scratch("corpus_replay_test")
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), "corpus-replay-test-"))
afterAll(() => fs.rmSync(ROOT, { recursive: true, force: true }))

const PAUSED = "paused"
const BATCHED = "batched"
const ONE = "capture_one.pcap.gz"
const TWO = "capture_two.pcap.gz"

/** Rows under `rows/`, handed to the interpreter as `ROWS_DIR`. */
const layout = CaseLayout.layerWith({
  rowsDir: "rows",
  rowFile: (number) => `${number}.json`,
  rowNumberOf: (name) => /^(.+)\.json$/.exec(name)?.[1],
  callKeyOf: () => undefined,
  interpreterVariable: "ROWS_DIR"
})

/** A box that names its cap to the interpreter as `BOX_MAX`. */
const boxing = Boxing.layerWith({
  box: (request) => ({ command: request.command, args: request.args }),
  refusal: Effect.succeed(null),
  interpreter: Effect.succeed("/bin/boxed-interpreter"),
  interpreterVariables: (memoryMax) => ({ BOX_MAX: memoryMax })
})

const hosting = LaneHosting.layerWith({
  lanes: [PAUSED, BATCHED],
  lane: (lane) =>
    Effect.succeed(
      lane === BATCHED ? { clock: "real", exclusive: true, cellCeiling: 2 } : { ...LaneHosting.NEUTRAL_LANE, cellCeiling: 3 }
    ),
  standBatch: () => Effect.succeed(undefined),
  standSut: () => Effect.succeed(undefined),
  preflight: () => Effect.succeed(null)
})

const tools = Tools.layerWith({
  ready: (tool) => Effect.succeed(`/bin/${tool}`),
  generator: Effect.die("unused"),
  measure: Effect.die("unused"),
  compileMs: Effect.succeed(0),
  decides: Effect.die("unused")
})

/** Cell services the fake driver never reaches for, built so the phase can provide them. */
const cellServices = Layer.mergeAll(
  Layer.succeed(ReplayCli.Service, ReplayCli.Service.of({ run: () => Effect.die("unused"), schema: () => Effect.die("unused") })),
  LanePresets.layer,
  RoutingCompiler.layer,
  Reclassifier.layer,
  Classifier.layer
)

const cellHost = CellHost.layerWith({
  tests: Effect.succeed({ program: "cargo", args: [] } as never),
  services: () => cellServices,
  auditLines: () => Effect.succeed([]),
  charges: (_dir, findings) => Effect.succeed(findings.filter((finding) => finding.gating)),
  hitsOf: () => []
})

const seams = Layer.mergeAll(layout, boxing, hosting, tools, cellHost, Identity.layer)

const withStore = <A, E>(effect: Effect.Effect<A, E, any>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Requests.layer),
      Effect.provide(Layer.mergeAll(Cuts.layer, Refusals.layer, Jobs.layer, Runs.layer, RuleHits.layer, Sweeps.layer)),
      Effect.provide(Layer.mergeAll(Sessions.layer(ROOT), Client.layerFromUrl(urlFor(SCRATCH)))),
      Effect.provide(seams),
      Effect.provide(NodeServices.layer)
    ) as Effect.Effect<A, E>
  )

const bytes = (text: string): Uint8Array => new TextEncoder().encode(text)

const caseOf = (id: string, number: string): Model.CutCaseRequest => ({
  caseId: id,
  callKey: `${id}@key`,
  document: { case: { id }, actors: [{ kind: "uas", endpoint: "ep2" }] },
  assets: [],
  ledger: [{ number, document: { number } }],
  egressEndpoint: "ep2"
})

let prep = 0
let sessions = ""

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
  prep = await withStore(
    Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const opened = yield* cuts.open()
      yield* cuts.record({
        prepId: opened,
        capture: ONE,
        flows: Model.wholeFlows(bytes(`{"capture":"${ONE}"}`)),
        cases: [caseOf(`${ONE}-a`, "1001"), caseOf(`${ONE}-b`, "1002")],
        refusals: []
      })
      yield* cuts.record({
        prepId: opened,
        capture: TWO,
        flows: Model.wholeFlows(bytes(`{"capture":"${TWO}"}`)),
        cases: [caseOf(`${TWO}-a`, "1003")],
        refusals: []
      })
      return opened
    })
  )
}, 120_000)

afterAll(async () => {
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

beforeEach(() => {
  sessions = fs.mkdtempSync(path.join(ROOT, "session-"))
})

const options = (lane: string, concurrency = 8): Options => ({
  paths: { corpus: path.join(sessions, "corpus"), sessions, census: sessions, registry: sessions, repoRoot: sessions },
  lane,
  selection: {
    universe: "named",
    positional: [],
    onlyNew: false,
    maxSpanSeconds: undefined,
    greenOnLane: undefined,
    sampleSize: undefined,
    limit: undefined,
    cases: []
  },
  batchSize: 1000,
  concurrency,
  startsPerSecond: 0,
  memoryMax: "2G",
  patience: { waitMs: 0 },
  dryRun: false,
  prep: undefined,
  cleanup: false,
  forceRecut: false
})

/** Every cell driven, and what its interpreter was told. */
let driven: Array<{ readonly id: string; readonly env: Readonly<Record<string, string>> }> = []

/** A driver that lays out a cell's verdict and terminated calls, passing the cells whose id ends `-a`. */
const fakeDrive: typeof runCell = ((cell: Campaign.CampaignCell, context: { runDir: string }) =>
  Effect.gen(function*() {
    const env = yield* ReplayCli.Environment
    const id = Campaign.cellCaseId(cell)
    driven.push({ id, env })
    const dir = path.join(context.runDir, Layout.cellDir(cell))
    fs.mkdirSync(dir, { recursive: true })
    const passed = id.endsWith("-a")
    fs.writeFileSync(path.join(dir, "verdict.json"), `{"status":"${passed ? "passed" : "failed"}","case":"${id}"}\n`)
    fs.writeFileSync(path.join(dir, TERMINATED), `[{"call":"${id}"}]\n`)
    return { summary: { cell: Layout.cellIdOf(cell), passed, dir: Layout.cellDir(cell) }, detail: "" }
  })) as unknown as typeof runCell

const replay = (lane: string, extra: { readonly host?: StandBatch; readonly concurrency?: number } = {}) =>
  withStore(
    Effect.gen(function*() {
      const opts = options(lane, extra.concurrency)
      const captures = yield* planWork([{ capture: ONE, set: { kind: "full" } }, { capture: TWO, set: { kind: "full" } }], prep, 2)
      const done = yield* phase({
        captures,
        options: opts,
        prepId: prep,
        drive: fakeDrive,
        binaries: () => Effect.void,
        identity: (_lane, load) => Effect.succeed({ ...CURRENT, load }),
        provenance: Effect.succeed([]),
        ...(extra.host === undefined ? {} : { host: extra.host })
      })
      const sql = yield* SqlClient
      const rows = yield* sql<{ capture: string; status: string; concurrency: number; cleanup: string | null }>`
        SELECT capture, status, concurrency, cleanup FROM runs WHERE sweep_id = ${done.sweep} ORDER BY capture`
      return { done, rows }
    })
  )

describe.skipIf(!served)("the replay phase", () => {
  beforeEach(() => {
    driven = []
  })

  it("closes each capture's row on its last cell, its index written and its terminated calls trimmed", async () => {
    const { done, rows } = await replay(PAUSED)
    expect(done.outcomes.map((outcome) => outcome.status)).toEqual(["done", "done"])
    expect(rows.map((row) => [row.capture, row.status])).toEqual([[ONE, "done"], [TWO, "done"]])
    expect(driven.map((cell) => cell.id).sort()).toEqual([`${ONE}-a`, `${ONE}-b`, `${TWO}-a`])
    for (const bundle of done.bundles) expect(fs.existsSync(path.join(bundle, "campaign.json"))).toBe(true)
    const one = done.bundles[0]!
    const cells = fs.readdirSync(one).filter((entry) => fs.existsSync(path.join(one, entry, "verdict.json")))
    const kept = (suffix: string) => path.join(one, cells.find((cell) => cell.includes(suffix))!, TERMINATED)
    expect(fs.existsSync(kept("-a"))).toBe(false)
    expect(fs.existsSync(`${kept("-a")}.gz`)).toBe(false)
    expect(JSON.parse(zlib.gunzipSync(fs.readFileSync(`${kept("-b")}.gz`)).toString())).toEqual([{ call: `${ONE}-b` }])
  })

  it("tells every interpreter its box and the batch's one side-row directory, and records the lane's ceiling", async () => {
    const { rows } = await replay(PAUSED, { concurrency: 8 })
    const dirs = new Set(driven.map((cell) => cell.env["ROWS_DIR"]))
    expect(dirs.size).toBe(1)
    const rowsDir = [...dirs][0]!
    expect(fs.readdirSync(rowsDir).sort()).toEqual(["1001.json", "1002.json", "1003.json"])
    expect(driven.every((cell) => cell.env["BOX_MAX"] === "2G")).toBe(true)
    expect(rows.every((row) => row.concurrency === 3)).toBe(true)
  })

  it("closes a batch process's rows once its gate has read, and marks every run it touched dirty", async () => {
    const events: Array<string> = []
    const host: StandBatch = () =>
      Effect.succeed({
        run: () => Effect.die("the fake driver runs no interpreter"),
        died: () => undefined,
        end: Effect.sync(() => {
          events.push("gate")
          return "the SUT still held a call"
        }),
        contexts: () => undefined,
        kill: Effect.sync(() => void events.push("killed"))
      })
    const { done, rows } = await replay(BATCHED, { host })
    expect(events).toEqual(["gate", "killed"])
    expect(done.dirty).toEqual(["the SUT still held a call"])
    expect(rows.every((row) => row.cleanup === "the SUT still held a call")).toBe(true)
    expect(rows.every((row) => row.concurrency === 2)).toBe(true)
  })
})
