/**
 * The replay phase over the deployment's seams, against a real postgres and
 * a fake cell driver: each capture's run row closes on its last cell with its
 * bundle's index written beside the cells; every interpreter of a batch is
 * told its box and the batch's one side-row directory; a passing cell keeps
 * no terminated calls and a failing one keeps them compressed; the lane's
 * ceilings bound the load the rows record; and where the lane runs a batch
 * process, the rows close once its gate has read, a dirty gate marking every
 * run the batch touched. The lane's batch collectors run over the cells a
 * batch drove, what they answered merged into each cell once its row closed,
 * a row spanning batches included.
 *
 * The suite builds and drops its OWN database (`@sip/store/test-server`).
 */
import { NodeServices } from "@effect/platform-node"
import { Campaign } from "@sip/contracts"
import { Layout, LanePresets, RoutingCompiler, type runCell } from "@sip/driver"
import { Classifier, Reclassifier } from "@sip/pipeline"
import { type Lanes, RigPath } from "@sip/rig"
import {
  Batches,
  CellAnswers,
  Cuts,
  JobEvents,
  Jobs,
  Model,
  Refusals,
  Requests,
  RuleHits,
  Runs,
  Sessions,
  Sweeps
} from "@sip/store"
import { Client } from "@sip/store"
import { CURRENT } from "@sip/store/test-identity"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import { ReplayCli } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import * as zlib from "node:zlib"
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest"
import { Boxing, CaseLayout, CellHost, Collectors, Identity, LaneHosting, Roots, Tools } from "../../src/deployment/index.js"
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

const seams = Layer.mergeAll(layout, boxing, hosting, tools, cellHost, Identity.layer, Roots.layer)

const withStore = <A, E>(effect: Effect.Effect<A, E, any>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Requests.layer),
      Effect.provide(
        Layer.mergeAll(
          Batches.layer,
          CellAnswers.layer,
          Cuts.layer,
          JobEvents.layer,
          Refusals.layer,
          Jobs.layer,
          Runs.layer,
          RuleHits.layer,
          Sweeps.layer
        )
      ),
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
    /** The deployment's collectors, none where absent. */
    readonly collectors?: Layer.Layer<Collectors.Service>
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
      const lines = yield* sql<{ caseId: string; name: string; answer: string; waived: boolean; message: string | null }>`
        SELECT v.case_id AS "caseId", v.name, v.answer, v.waived, v.message FROM verifications v JOIN runs r ON r.id = v.run_id
        WHERE r.sweep_id = ${done.sweep} AND v.scope = 'cell' ORDER BY v.case_id, v.ordinal`
      const statuses = yield* sql<{ status: string }>`
        SELECT s.status FROM cell_statuses s JOIN runs r ON r.id = s.run_id
        WHERE r.sweep_id = ${done.sweep} ORDER BY s.case_id`
      return { done, rows, batches, lines, statuses: statuses.map((row) => row.status) }
    }).pipe(Effect.provide(extra.collectors ?? Collectors.layer))
  )

/** A driver like {@link fakeDrive} whose every cell passes and states its clock and its window, as a cell description reads them. */
const describedDrive: typeof runCell = ((cell: Campaign.CampaignCell, context: { runDir: string }) =>
  Effect.sync(() => {
    const id = Campaign.cellCaseId(cell)
    const dir = path.join(context.runDir, Layout.cellDir(cell))
    fs.mkdirSync(dir, { recursive: true })
    fs.writeFileSync(path.join(dir, "verdict.json"), `{"status":"ok","case":"${id}"}\n`)
    fs.writeFileSync(
      path.join(dir, "run-config.json"),
      `{"claim_numbers":{},"clock":"virtual","identities":{},"lane":"${PAUSED}","route_target":"127.0.0.1:5080"}\n`
    )
    fs.writeFileSync(
      path.join(dir, "timing.json"),
      `{"settle_budget_ms":100,"settled_at_ms":10,"started_at_ms":0,"wall_started_at_ms":1,"wall_ended_at_ms":2}\n`
    )
    return { summary: { cell: Layout.cellIdOf(cell), passed: true, dir: Layout.cellDir(cell) }, detail: "" }
  })) as unknown as typeof runCell

/** A driver like {@link describedDrive} whose every cell's call passes and lands `pending` on a `cdr` line its batch's end answers. */
const describedPendingDrive: typeof runCell = ((cell: Campaign.CampaignCell, context: { runDir: string }) =>
  Effect.map(describedDrive(cell, context as never), (ran) => ({
    summary: { ...ran.summary, passed: false, pending: ["cdr"] },
    detail: "still verifying: cdr pending"
  }))) as unknown as typeof runCell

/** A batch process that died under its batch: no end of it runs. */
const dying: StandBatch = () =>
  Effect.succeed({
    run: () => Effect.die("the fake driver runs no interpreter"),
    died: () => "the batch process died",
    end: Effect.die("a dead batch process's end never runs"),
    contexts: () => undefined,
    kill: Effect.void
  })

/** The deployment's collectors: on {@link PAUSED}, the batch collector `lab`, whose collect says why it failed on its stderr and exits 3. */
const failingLab = async (): Promise<{ readonly layer: Layer.Layer<Collectors.Service>; readonly said: string }> => ({
  layer: await labOf({ collect: `#!/bin/sh\necho "the lab is gone" >&2\nexit 3\n` }),
  said: "collect exited 3: the lab is gone"
})

/** A collect answering `lab-check` passed for every cell its context names. */
const PASSING_COLLECT = `#!/usr/bin/env node
const fs = require("node:fs")
const context = JSON.parse(fs.readFileSync(process.env.SIPRIG_CONTEXT, "utf8"))
for (const cell of context.cells) console.log(JSON.stringify({ verification: "lab-check", cell: cell.id, answer: "pass" }))
`

/**
 * The deployment's collectors: on {@link PAUSED}, the batch collector `lab`
 * running the scripts `steps` gives, answering `lab-check`, set `always` or
 * as `steps.setting` says.
 */
const labOf = async (
  steps: { readonly collect: string; readonly close?: string; readonly setting?: string }
): Promise<Layer.Layer<Collectors.Service>> => {
  const root = fs.mkdtempSync(path.join(ROOT, "rig-"))
  const dir = path.join(root, "collectors", "lab")
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(
    path.join(dir, "collector.toml"),
    `schema = "collector.v1"\nname = "lab"\nscope = "batch"\nanswers = ["lab-check"]\n` +
      (steps.setting === undefined ? "" : `default = "${steps.setting}"\n`) +
      `[run]\ncollect = "./collect"\n` +
      (steps.close === undefined ? "" : `close = "./close"\n`)
  )
  fs.writeFileSync(path.join(dir, "collect"), steps.collect, { mode: 0o755 })
  if (steps.close !== undefined) fs.writeFileSync(path.join(dir, "close"), steps.close, { mode: 0o755 })
  const rig = await Effect.runPromise(RigPath.load({ workspace: root }).pipe(Effect.provide(NodeServices.layer)))
  const lane: Lanes.Loaded = {
    id: PAUSED,
    file: `lanes/${PAUSED}.toml`,
    lane: { schema: "lane.v1", clock: "simulated" },
    parts: new Map(),
    operations: new Map(),
    capabilities: new Set()
  }
  return Collectors.layerOf(rig, [lane])
}

/**
 * A driver like {@link describedDrive} where the cell `<TWO>-a` states no
 * window (no description can be read of it) and `<ONE>-b` crashed, its
 * window stated.
 */
const crashingDrive: typeof runCell = ((cell: Campaign.CampaignCell, context: { runDir: string }) =>
  Effect.flatMap(describedDrive(cell, context as never), (ran) =>
    Effect.sync(() => {
      const id = Campaign.cellCaseId(cell)
      const dir = path.join(context.runDir, Layout.cellDir(cell))
      if (id === `${TWO}-a`) fs.rmSync(path.join(dir, "timing.json"))
      return id === `${ONE}-b` ? { ...ran, summary: { ...ran.summary, passed: false, error: "the interpreter died" } } : ran
    }))) as unknown as typeof runCell

/**
 * The deployment's collectors: on {@link BATCHED}, a real-clock lane, the
 * cell-scope collector `after-call`, whose collect answers it passed at once,
 * or, over a cell of a case ending `-b`, only after 10 s.
 */
const afterCallOf = async (): Promise<Layer.Layer<Collectors.Service>> => {
  const root = fs.mkdtempSync(path.join(ROOT, "rig-"))
  const dir = path.join(root, "collectors", "after-call")
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(
    path.join(dir, "collector.toml"),
    `schema = "collector.v1"\nname = "after-call"\nscope = "cell"\nanswers = ["after-call"]\n[timeouts]\ncollect_s = 30\n[run]\ncollect = "./collect"\n`
  )
  fs.writeFileSync(
    path.join(dir, "collect"),
    `#!/bin/sh\nif grep -q -- '-b",' "$SIPRIG_CONTEXT"; then sleep 10; fi\necho '{"verification":"after-call","answer":"pass"}'\n`,
    { mode: 0o755 }
  )
  const rig = await Effect.runPromise(RigPath.load({ workspace: root }).pipe(Effect.provide(NodeServices.layer)))
  const lane: Lanes.Loaded = {
    id: BATCHED,
    file: `lanes/${BATCHED}.toml`,
    lane: { schema: "lane.v1", clock: "real" },
    parts: new Map(),
    operations: new Map(),
    capabilities: new Set()
  }
  return Collectors.layerOf(rig, [lane])
}

/** A driver like {@link describedDrive} whose cell `<TWO>-a` fails its batch 0.5 s after it started, the others landed by then. */
const failingBatchDrive: typeof runCell = ((cell: Campaign.CampaignCell, context: { runDir: string }) =>
  Campaign.cellCaseId(cell) === `${TWO}-a`
    ? Effect.andThen(Effect.sleep(500), Effect.die(new Error("the driver broke")))
    : describedDrive(cell, context as never)) as unknown as typeof runCell

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

  it("runs the lane's batch collectors over the cells it drove: one failing leaves every landed cell unverified with its message", async () => {
    const lab = await failingLab()
    for (const batchSize of [1000, 1]) {
      const { lines, statuses } = await replay(PAUSED, { drive: describedDrive, collectors: lab.layer, batchSize })
      const answered = lines.filter((line) => line.name === "lab-check")
      expect(answered.map((line) => [line.caseId, line.answer])).toEqual([
        [`${ONE}-a`, "not-checkable"],
        [`${ONE}-b`, "not-checkable"],
        [`${TWO}-a`, "not-checkable"]
      ])
      for (const line of answered) expect(line.message).toContain(lab.said)
      expect(statuses).toEqual(["unverified", "unverified", "unverified"])
    }
  })

  it("hands the collectors a crashed cell that states its window, and answers not checkable, saying why, on a cell no description is read of", async () => {
    const layer = await labOf({ collect: PASSING_COLLECT })
    const { lines } = await replay(PAUSED, { drive: crashingDrive, collectors: layer })
    const answered = lines.filter((line) => line.name === "lab-check")
    expect(answered.map((line) => [line.caseId, line.answer])).toEqual([
      [`${ONE}-a`, "pass"],
      [`${ONE}-b`, "pass"],
      [`${TWO}-a`, "not-checkable"]
    ])
    expect(answered[2]!.message).toMatch(new RegExp(`^the cell ${TWO}#${TWO}-a@key was not described: `))
  })

  it("hands an on-failure batch collector the crashed cell, which states no failing line, and no cell that passed", async () => {
    const layer = await labOf({ collect: PASSING_COLLECT, setting: "on-failure" })
    const { lines } = await replay(PAUSED, { drive: crashingDrive, collectors: layer })
    const answered = lines.filter((line) => line.name === "lab-check")
    expect(answered.map((line) => [line.caseId, line.answer])).toEqual([[`${ONE}-b`, "pass"]])
  })

  it("hands the job's collectors each cell failed as its batch ended: crashed, or failed by a batch collector's line", async () => {
    const failsA = `#!/usr/bin/env node
const fs = require("node:fs")
const context = JSON.parse(fs.readFileSync(process.env.SIPRIG_CONTEXT, "utf8"))
for (const cell of context.cells) {
  console.log(JSON.stringify({ verification: "lab-check", cell: cell.id, answer: cell.case.endsWith("-a") ? "fail" : "pass" }))
}
`
    const layer = await labOf({ collect: failsA })
    const { done } = await replay(PAUSED, { drive: crashingDrive, collectors: layer })
    expect(done.ran.map((cell) => [cell.caseId, cell.failed]).sort()).toEqual([
      [`${ONE}-a`, true],
      [`${ONE}-b`, true],
      [`${TWO}-a`, false]
    ])
  })

  it("hands the job's collectors as failed a cell that landed pending and whose batch's end failed it, as passed one its end passed", async () => {
    const failedOf = async (cdr: string) =>
      (await replay(BATCHED, { host: answering(cdr), drive: pendingDrive })).done.ran.map((cell) => [cell.caseId, cell.failed]).sort()
    expect(await failedOf("fail")).toEqual([[`${ONE}-a`, true], [`${ONE}-b`, true], [`${TWO}-a`, true]])
    expect(await failedOf("pass")).toEqual([[`${ONE}-a`, false], [`${ONE}-b`, false], [`${TWO}-a`, false]])
  })

  it("hands an on-failure batch collector, and the job's collectors, every cell of a batch whose process died, though each landed pending", async () => {
    const layer = await labOf({ collect: PASSING_COLLECT, setting: "on-failure" })
    const { done, lines } = await replay(PAUSED, { host: dying, drive: describedPendingDrive, collectors: layer })
    const answered = lines.filter((line) => line.name === "lab-check")
    expect(answered.map((line) => [line.caseId, line.answer])).toEqual([
      [`${ONE}-a`, "pass"],
      [`${ONE}-b`, "pass"],
      [`${TWO}-a`, "pass"]
    ])
    expect(done.ran.map((cell) => [cell.caseId, cell.failed]).sort()).toEqual([[`${ONE}-a`, true], [`${ONE}-b`, true], [`${TWO}-a`, true]])
  })

  it("marks the batch's runs dirty and the replay dirty where a collector's close left the lab changed", async () => {
    const layer = await labOf({ collect: PASSING_COLLECT, close: `#!/bin/sh\necho "the lab would not reset" >&2\nexit 1\n` })
    const { done, rows } = await replay(PAUSED, { drive: describedDrive, collectors: layer })
    expect(done.dirty).toEqual(["the collector lab left the lab changed"])
    expect(rows.map((row) => [row.capture, row.cleanup])).toEqual([
      [ONE, "the collector lab left the lab changed"],
      [TWO, "the collector lab left the lab changed"]
    ])
  })

  it("keeps what a failed batch's cell-scope collectors answered, and leaves unverified a landed cell whose collectors it cut short", async () => {
    const layer = await afterCallOf()
    const { failed, lines, statuses } = await withStore(
      Effect.gen(function*() {
        const captures = yield* planWork([{ capture: ONE, set: { kind: "full" } }, { capture: TWO, set: { kind: "full" } }], prep, 2)
        const exit = yield* Effect.exit(phase({
          captures,
          options: options(BATCHED),
          prepId: prep,
          drive: failingBatchDrive,
          binaries: () => Effect.void,
          identity: (_lane, load) => Effect.succeed({ ...CURRENT, load }),
          provenance: Effect.succeed([])
        }))
        const sql = yield* SqlClient
        const lines = yield* sql<{ caseId: string; answer: string }>`
          SELECT case_id AS "caseId", answer FROM verifications WHERE name = 'after-call' AND scope = 'cell' ORDER BY case_id`
        const statuses = yield* sql<{ caseId: string; status: string }>`
          SELECT case_id AS "caseId", status FROM cell_statuses
          WHERE run_id IN (SELECT run_id FROM verifications WHERE name = 'after-call') ORDER BY case_id`
        return { failed: Exit.isFailure(exit), lines, statuses }
      }).pipe(Effect.provide(layer))
    )
    expect(failed).toBe(true)
    expect(lines.map((line) => [line.caseId, line.answer])).toEqual([[`${ONE}-a`, "pass"], [`${ONE}-b`, "not-checkable"]])
    expect(statuses.map((row) => [row.caseId, row.status])).toEqual([[`${ONE}-a`, "pass"], [`${ONE}-b`, "unverified"]])
  })

  it("files no row for a batch whose every capture it could not materialise", async () => {
    const { rows, batches } = await replay(PAUSED, { batchSize: 2, breaking: ONE })
    expect(rows.map((row) => [row.capture, row.status])).toEqual([[ONE, "error"], [TWO, "done"]])
    expect(batches.map((row) => row.cells)).toEqual([[`${TWO}#${TWO}-a@key`]])
  })
})
