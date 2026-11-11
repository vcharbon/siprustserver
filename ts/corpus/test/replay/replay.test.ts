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
import { Batches, Cuts, Jobs, Model, Refusals, Requests, RuleHits, Runs, Sessions, Sweeps } from "@sip/store"
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
import * as Content from "../../src/content/content.js"
import { cellsOf, planWork } from "../../src/replay/plan.js"
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
  decides: Effect.die("unused"),
  reading: []
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
      Effect.provide(Layer.mergeAll(Batches.layer, Cuts.layer, Refusals.layer, Jobs.layer, Runs.layer, RuleHits.layer, Sweeps.layer)),
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

const options = (lane: string, concurrency = 8, batchSize = 1000): Options => ({
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
  batchSize,
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

/** The cell directories {@link pendingDrive} landed, for the batch's end to answer. */
let landedPending: Array<string> = []

/**
 * A driver whose every cell's call passes and whose `cdr` line its batch's end
 * answers: it lands `pending`, the cell not passed, naming it.
 */
const pendingDrive: typeof runCell = ((cell: Campaign.CampaignCell, context: { runDir: string }) =>
  Effect.sync(() => {
    const id = Campaign.cellCaseId(cell)
    const dir = path.join(context.runDir, Layout.cellDir(cell))
    fs.mkdirSync(dir, { recursive: true })
    fs.writeFileSync(path.join(dir, "verdict.json"), `{"status":"passed","case":"${id}"}\n`)
    writeLines(dir, "pending")
    landedPending.push(dir)
    return {
      summary: { cell: Layout.cellIdOf(cell), passed: false, dir: Layout.cellDir(cell), pending: ["cdr"] },
      detail: "still verifying: cdr pending"
    }
  })) as unknown as typeof runCell

/** The cell's lines: its `call` line answering `call`, its `cdr` line answering `cdr`. */
const writeLines = (dir: string, cdr: string, call = "pass"): void =>
  fs.writeFileSync(
    path.join(dir, "verifications.json"),
    JSON.stringify([
      { name: "call", source: "interpreter", answer: call },
      { name: "cdr", source: "cdr oracle", answer: cdr }
    ])
  )

/** The `call` line's answer in the cell's lines. */
const callIn = (dir: string): string =>
  (JSON.parse(fs.readFileSync(path.join(dir, "verifications.json"), "utf8")) as Array<{ name: string; answer: string }>)
    .find((line) => line.name === "call")!.answer

/**
 * A driver whose every cell's call failed and was restated as passed by the
 * reclassifier as it landed, its `cdr` line pending on its batch's end.
 */
const restatedDrive: typeof runCell = ((cell: Campaign.CampaignCell, context: { runDir: string }) =>
  Effect.sync(() => {
    const id = Campaign.cellCaseId(cell)
    const dir = path.join(context.runDir, Layout.cellDir(cell))
    fs.mkdirSync(dir, { recursive: true })
    fs.writeFileSync(path.join(dir, "verdict.json"), `{"status":"failed","case":"${id}"}\n`)
    writeLines(dir, "pending", "fail")
    landedPending.push(dir)
    return {
      summary: {
        cell: Layout.cellIdOf(cell),
        passed: false,
        dir: Layout.cellDir(cell),
        restated: "rule r1",
        restatedLines: ["call"],
        pending: ["cdr"]
      },
      detail: "still verifying: cdr pending"
    }
  })) as unknown as typeof runCell

/** A batch process whose end answers every landed cell's `cdr` line `cdr`, or leaves it where `cdr` is undefined. */
const answering = (cdr: string | undefined): StandBatch => () =>
  Effect.succeed({
    run: () => Effect.die("the fake driver runs no interpreter"),
    died: () => undefined,
    end: Effect.sync(() => {
      if (cdr !== undefined) for (const dir of landedPending) writeLines(dir, cdr, callIn(dir))
      return undefined
    }),
    contexts: () => undefined,
    kill: Effect.void
  })

const replay = (
  lane: string,
  extra: {
    readonly host?: StandBatch
    readonly drive?: typeof runCell
    readonly concurrency?: number
    readonly batchSize?: number
    /** A capture whose cases' cache entries are left unreadable, so the batch cannot materialise it. */
    readonly breaking?: string
  } = {}
) =>
  withStore(
    Effect.gen(function*() {
      const opts = options(lane, extra.concurrency, extra.batchSize)
      const captures = yield* planWork([{ capture: ONE, set: { kind: "full" } }, { capture: TWO, set: { kind: "full" } }], prep, 2)
      for (const ref of cellsOf(captures)) {
        if (ref.capture === extra.breaking) fs.mkdirSync(Content.caseDir(sessions, ref.caseHash), { recursive: true })
      }
      const done = yield* phase({
        captures,
        options: opts,
        prepId: prep,
        drive: extra.drive ?? fakeDrive,
        binaries: () => Effect.void,
        identity: (_lane, load) => Effect.succeed({ ...CURRENT, load }),
        provenance: Effect.succeed([]),
        ...(extra.host === undefined ? {} : { host: extra.host })
      })
      const sql = yield* SqlClient
      const rows = yield* sql<{ capture: string; status: string; concurrency: number; cleanup: string | null }>`
        SELECT capture, status, concurrency, cleanup FROM runs WHERE sweep_id = ${done.sweep} ORDER BY capture`
      const batches = yield* Effect.flatMap(Batches.Service, (store) => store.ofSweep(done.sweep))
      const lines = yield* sql<{ caseId: string; name: string; answer: string; waived: boolean }>`
        SELECT v.case_id AS "caseId", v.name, v.answer, v.waived FROM verifications v JOIN runs r ON r.id = v.run_id
        WHERE r.sweep_id = ${done.sweep} AND v.scope = 'cell' ORDER BY v.case_id, v.ordinal`
      const statuses = yield* sql<{ status: string }>`
        SELECT s.status FROM cell_statuses s JOIN runs r ON r.id = s.run_id
        WHERE r.sweep_id = ${done.sweep} ORDER BY s.case_id`
      return { done, rows, batches, lines, statuses: statuses.map((row) => row.status) }
    })
  )

describe.skipIf(!served)("the replay phase", () => {
  beforeEach(() => {
    driven = []
    landedPending = []
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
    const { done, rows, batches } = await replay(BATCHED, { host })
    expect(events).toEqual(["gate", "killed"])
    expect(batches.map((row) => row.detail)).toEqual(["the SUT still held a call"])
    expect(done.dirty).toEqual(["the SUT still held a call"])
    expect(rows.every((row) => row.cleanup === "the SUT still held a call")).toBe(true)
    expect(rows.every((row) => row.concurrency === 2)).toBe(true)
  })

  it("passes a batch process's cell that landed pending once its end answers the line it waited on", async () => {
    const { done, lines } = await replay(BATCHED, { host: answering("pass"), drive: pendingDrive })
    expect(landedPending).toHaveLength(3)
    const index = JSON.parse(fs.readFileSync(path.join(done.bundles[0]!, "campaign.json"), "utf8"))
    expect(index.cells.map((cell: { passed: boolean; pending?: unknown }) => [cell.passed, cell.pending])).toEqual([
      [true, undefined],
      [true, undefined]
    ])
    expect(new Set(lines.map((line) => `${line.name} ${line.answer}`))).toEqual(new Set(["call pass", "cdr pass"]))
  })

  it("keeps a batch process's cell ok so far, failing nothing, where its end never answered", async () => {
    const { done, lines } = await replay(BATCHED, { host: answering(undefined), drive: pendingDrive })
    const index = JSON.parse(fs.readFileSync(path.join(done.bundles[0]!, "campaign.json"), "utf8"))
    expect(index.cells.map((cell: { passed: boolean; pending?: unknown }) => [cell.passed, cell.pending])).toEqual([
      [false, ["cdr"]],
      [false, ["cdr"]]
    ])
    expect(new Set(lines.map((line) => `${line.name} ${line.answer}`))).toEqual(new Set(["call pass", "cdr pending"]))
  })

  it("fails a batch process's cell whose end failed the line it waited on, nothing left pending", async () => {
    const { done, lines, statuses } = await replay(BATCHED, { host: answering("fail"), drive: pendingDrive })
    const index = JSON.parse(fs.readFileSync(path.join(done.bundles[0]!, "campaign.json"), "utf8"))
    expect(index.cells.map((cell: { passed: boolean; pending?: unknown }) => [cell.passed, cell.pending])).toEqual([
      [false, undefined],
      [false, undefined]
    ])
    expect(new Set(lines.map((line) => `${line.name} ${line.answer}`))).toEqual(new Set(["call pass", "cdr fail"]))
    expect(statuses).toEqual(["fail", "fail", "fail"])
  })

  it("fails a restated cell on the line its batch's end failed, waiving only the call the reclassifier saw land", async () => {
    const { done, lines, statuses } = await replay(BATCHED, { host: answering("fail"), drive: restatedDrive })
    const index = JSON.parse(fs.readFileSync(path.join(done.bundles[0]!, "campaign.json"), "utf8"))
    expect(index.cells.map((cell: { passed: boolean; pending?: unknown }) => [cell.passed, cell.pending])).toEqual([
      [false, undefined],
      [false, undefined]
    ])
    expect(new Set(lines.map((line) => `${line.name} ${line.answer}${line.waived === true ? " waived" : ""}`))).toEqual(
      new Set(["call fail waived", "cdr fail", "reclassifier pass"])
    )
    expect(statuses).toEqual(["fail", "fail", "fail"])
  })

  it("writes each batch as a row of the sweep, its cells and its window, closed once it ended", async () => {
    const { done, batches } = await replay(PAUSED, { batchSize: 2 })
    expect(done.batches).toBe(2)
    expect(batches.map((row) => [row.jobId, row.lane, row.attempt, row.cells.length, row.detail])).toEqual([
      [null, PAUSED, 1, 2, null],
      [null, PAUSED, 1, 1, null]
    ])
    expect(batches.flatMap((row) => row.cells).sort()).toEqual([`${ONE}#${ONE}-a@key`, `${ONE}#${ONE}-b@key`, `${TWO}#${TWO}-a@key`])
    for (const row of batches) expect(Date.parse(row.finishedAt!)).toBeGreaterThanOrEqual(Date.parse(row.startedAt))
    expect(Date.parse(batches[1]!.startedAt)).toBeGreaterThanOrEqual(Date.parse(batches[0]!.finishedAt!))
  })

  it("names in a batch's row only the cells it drove, not those of a capture it could not materialise", async () => {
    const { done, rows, batches } = await replay(PAUSED, { breaking: TWO })
    expect(rows.map((row) => [row.capture, row.status])).toEqual([[ONE, "done"], [TWO, "error"]])
    expect(driven.map((cell) => cell.id).sort()).toEqual([`${ONE}-a`, `${ONE}-b`])
    expect(done.batches).toBe(1)
    expect(batches.map((row) => [...row.cells].sort())).toEqual([[`${ONE}#${ONE}-a@key`, `${ONE}#${ONE}-b@key`]])
  })

  it("files no row for a batch whose every capture it could not materialise", async () => {
    const { rows, batches } = await replay(PAUSED, { batchSize: 2, breaking: ONE })
    expect(rows.map((row) => [row.capture, row.status])).toEqual([[ONE, "error"], [TWO, "done"]])
    expect(batches.map((row) => row.cells)).toEqual([[`${TWO}#${TWO}-a@key`]])
  })
})
