/**
 * What a selection freezes to: the filter bar's rows as cells, whole captures
 * only where the selection meant them whole, a ticked file as given.
 */
import { NodeServices } from "@effect/platform-node"
import { Calls, Cuts, Population, Runs } from "@sip/store"
import { queueOf } from "@sip/store/memory-queue"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { describe, expect, it } from "vitest"

import {
  describe as describeSelection,
  freeze,
  parseCells,
  preview,
  previewLines,
  Refused,
  resolutionLines,
  TICKED,
  widen
} from "../../src/requests/request.js"
import { Tracker } from "../../src/deployment/index.js"
import type { Paths } from "../../src/paths.js"

const A = "capture_a.pcap.gz"
const B = "capture_b.pcap.gz"

const row = (
  capture: string,
  caseId: string,
  over: Partial<Calls.CallRow> = {}
): Calls.CallRow => ({
  capture,
  caseId,
  kind: "case",
  callKey: `${caseId}@key`,
  runId: 1,
  setKind: "full",
  prepId: null,
  bundlePath: null,
  documentHash: null,
  passed: false,
  skipped: null,
  failedStep: "s1",
  rfcGating: null,
  reasons: [],
  features: [],
  mainError: { kind: "failure", rule: "died", rfc: null, step: null, expected: null, detail: "" },
  finishedAt: null,
  ...over
})

const ROWS: ReadonlyArray<Calls.CallRow> = [
  row(A, `${A}-case1`),
  row(A, `${A}-case2`, { passed: true, failedStep: null, mainError: null }),
  row(A, `${A}-ghost`, { callKey: null }),
  row(B, `${B}-cut1`, {
    kind: "refused",
    callKey: "SDroot@x",
    runId: null,
    setKind: null,
    prepId: 2,
    reasons: ["source-call-incomplete"],
    mainError: { kind: "refused", rule: "source-call-incomplete", rfc: null, step: null, expected: null, detail: "" }
  })
]

const refuse = Effect.die("not in this test")

/** The paused lane's queue is ROWS; every other lane's is empty. */
const calls = Layer.succeed(
  Calls.Service,
  Calls.Service.of({
    queue: (lane, query, limit) => Effect.succeed(queueOf(lane === "paused" ? ROWS : [], query, limit)),
    explain: () => refuse,
    hydrate: () => refuse
  })
)

const cuts = Layer.succeed(
  Cuts.Service,
  Cuts.Service.of({
    open: () => refuse,
    finish: () => refuse,
    exists: () => refuse,
    record: () => refuse,
    standing: () => refuse,
    resolve: () => refuse,
    cases: (_prep, captures) =>
      Effect.succeed(
        new Map(
          captures.filter((capture) => capture === A).map((capture) => [
            capture,
            [
              { caseId: `${capture}-case1`, callKey: `${capture}-case1@key`, caseHash: "h1", egressEndpoint: null },
              { caseId: `${capture}-case2`, callKey: `${capture}-case2@key`, caseHash: "h2", egressEndpoint: null }
            ]
          ])
        )
      ),
    standingPrep: refuse,
    latest: refuse,
    captures: () => refuse,
    caseSpans: () => refuse,
    contentFrom: () => refuse,
    content: () => refuse,
    flowsEnvelope: () => refuse,
    flowsLegs: () => refuse,
    ledger: () => refuse
  })
)

/** The universe form reads these; no test here freezes one, so every member refuses. */
const population = Layer.succeed(
  Population.Service,
  Population.Service.of({
    add: () => refuse,
    list: refuse,
    captures: refuse,
    size: refuse,
    seed: () => refuse,
    ingested: () => refuse
  })
)

const runs = Layer.succeed(
  Runs.Service,
  Runs.Service.of({
    open: () => refuse,
    openMany: () => refuse,
    finish: () => refuse,
    importRun: () => refuse,
    lanes: refuse,
    ranOn: () => refuse,
    standings: () => refuse,
    standing: () => refuse,
    protectedBundles: refuse,
    allBundles: refuse,
    unfinished: refuse,
    abandonable: () => refuse,
    abandon: () => refuse,
    unfinishedOf: () => refuse,
    dirty: () => refuse,
    openOfSweep: () => refuse,
    forget: () => refuse
  })
)

/** Roots no test here reads: every capture is named by the cut store. */
const paths: Paths = {
  corpus: "/nonexistent/corpus",
  sessions: "/nonexistent/sessions",
  census: "/nonexistent/census",
  registry: "/nonexistent/registry.json",
  repoRoot: "/nonexistent"
}

/** The tracker, never holding a ticket: no registry record of these tests waits on one. */
const noTickets = Layer.merge(Tracker.layer, NodeServices.layer)

const frozen = (input: Parameters<typeof freeze>[3], lane = "paused") =>
  Effect.runPromise(
    freeze(paths, lane, 2, input).pipe(Effect.provide(Layer.mergeAll(calls, cuts, population, runs, noTickets)))
  )

describe("freezing a selection", () => {
  it("names the filter's rows by key, skips a row no cut keys, and freezes a chip filter as-is", async () => {
    const out = await frozen({ kind: "filter", query: { scope: "failed" } })
    if (out instanceof Refused) throw new Error(out.message)
    expect(out.cells).toEqual([{ capture: A, callKey: `${A}-case1@key` }])
    expect(out.unkeyed).toEqual([{ capture: A, caseId: `${A}-ghost` }])
    expect(out.selection).toEqual({ kind: "filter", scope: "failed", whole: [] })
  })

  it("reads the bar's words on --on-lane's queue and keeps the request's lane as the target", async () => {
    const out = await frozen({ kind: "filter", query: { scope: "failed" }, lane: "paused" }, "real")
    if (out instanceof Refused) throw new Error(out.message)
    expect(out.cells).toEqual([{ capture: A, callKey: `${A}-case1@key` }])
    expect(out.selection).toEqual({ kind: "filter", lane: "paused", scope: "failed", whole: [] })
    expect(describeSelection(out.selection)).toEqual("--on-lane paused --scope failed")
  })

  it("reads a filter naming no lane on the request's own queue", async () => {
    const own = await frozen({ kind: "filter", query: { scope: "failed" } }, "real")
    if (own instanceof Refused) throw new Error(own.message)
    expect(own.cells).toEqual([])
    expect(own.selection).toEqual({ kind: "filter", scope: "failed", whole: [] })
    const blank = await frozen({ kind: "filter", query: { scope: "failed" }, lane: "" })
    if (blank instanceof Refused) throw new Error(blank.message)
    expect(blank.cells).toEqual([{ capture: A, callKey: `${A}-case1@key` }])
    expect(blank.selection).toEqual({ kind: "filter", scope: "failed", whole: [] })
    // The bar's lane is the request's own: the sentence has nothing to add.
    const same = await frozen({ kind: "filter", query: { scope: "failed" }, lane: "paused" })
    if (same instanceof Refused) throw new Error(same.message)
    expect(same.selection).toEqual({ kind: "filter", scope: "failed", whole: [] })
  })

  it("means whole captures at scope all with no chip, refused calls included", async () => {
    const out = await frozen({ kind: "filter", query: { scope: "all", capture: "capture_" } })
    if (out instanceof Refused) throw new Error(out.message)
    expect(out.cells.map((cell) => cell.callKey)).toEqual([`${A}-case1@key`, `${A}-case2@key`, "SDroot@x"])
    expect(out.selection).toEqual({ kind: "filter", scope: "all", q: "capture_", whole: [A, B] })
  })

  it("freezes a named capture from its cut, whole, and refuses one no cut holds", async () => {
    const out = await frozen({ kind: "captures", captures: [A] })
    if (out instanceof Refused) throw new Error(out.message)
    expect(out.cells).toEqual([{ capture: A, callKey: `${A}-case1@key` }, { capture: A, callKey: `${A}-case2@key` }])
    expect(out.selection).toEqual({ kind: "captures", captures: [A], whole: [A] })
    const missing = await frozen({ kind: "captures", captures: [A, "capture_typo.pcap.gz"] })
    expect(missing).toBeInstanceOf(Refused)
    expect((missing as Refused).message).toContain("capture_typo.pcap.gz")
  })
})

describe("widening and previewing", () => {
  const services = Layer.mergeAll(calls, cuts, population, runs, noTickets)

  it("keeps a chip filter's excludes and outcome on the selection, which is then not whole", async () => {
    const out = await frozen({
      kind: "filter",
      query: { scope: "all", outcome: "failed", exclude: { rule: ["died"], expected: [] } }
    })
    if (out instanceof Refused) throw new Error(out.message)
    expect(out.selection).toEqual({ kind: "filter", scope: "all", outcome: "failed", exclude: { rule: ["died"] }, whole: [] })
    expect(describeSelection(out.selection)).toEqual("--scope all --outcome failed --not-rule died")
  })

  it("takes ticked cells as they are, recorded as the ticked file", async () => {
    const out = await frozen({ kind: "ticked", cells: [{ capture: B, callKey: "SDroot@x" }] })
    if (out instanceof Refused) throw new Error(out.message)
    expect(out.cells).toEqual([{ capture: B, callKey: "SDroot@x" }])
    expect(out.selection).toEqual({ kind: "cells", file: TICKED, whole: [] })
  })

  it("widens a filter to a capture whole: its cut's keys join the cells once, the capture joins whole", async () => {
    const out = await Effect.runPromise(
      freeze(paths, "paused", 2, { kind: "filter", query: { scope: "failed" } }).pipe(
        Effect.flatMap((bare) => (bare instanceof Refused ? Effect.succeed(bare) : widen(bare, 2, [A]))),
        Effect.provide(services)
      )
    )
    if (out instanceof Refused) throw new Error(out.message)
    expect(out.cells).toEqual([{ capture: A, callKey: `${A}-case1@key` }, { capture: A, callKey: `${A}-case2@key` }])
    expect(out.selection).toEqual({ kind: "filter", scope: "failed", whole: [A] })
  })

  it("previews each capture full or cases against the cut, whatever the lane ran", async () => {
    const shown = await Effect.runPromise(
      preview(2, {
        selection: { kind: "cells", file: TICKED, whole: [] },
        cells: [{ capture: A, callKey: `${A}-case1@key` }, { capture: B, callKey: "SDroot@x" }],
        unkeyed: []
      }).pipe(Effect.provide(services))
    )
    expect(shown).toEqual({
      prepId: 2,
      cells: 2,
      captures: [
        { capture: A, cells: 1, cut: 2, kind: "cases" },
        { capture: B, cells: 1, cut: 0, kind: "cases" }
      ],
      whole: [],
      unkeyed: 0
    })
    expect(previewLines(shown, "paused")[0]).toEqual(
      "2 cell(s) over 2 capture(s) on paused against prep 2 — 0 full, 2 as cases"
    )
    const whole = await Effect.runPromise(
      preview(2, {
        selection: { kind: "captures", captures: [A], whole: [A] },
        cells: [{ capture: A, callKey: `${A}-case1@key` }, { capture: A, callKey: `${A}-case2@key` }],
        unkeyed: []
      }).pipe(Effect.provide(services))
    )
    expect(whole.captures).toEqual([{ capture: A, cells: 2, cut: 2, kind: "full" }])
  })
})

describe("a file of cells", () => {
  it("reads capture and key per line, skipping blanks and comments", () => {
    expect(parseCells(`# ticked\n${A}\tSDa@x\n\n${B} SDb@x#ingress1\n`, "t.txt")).toEqual([
      { capture: A, callKey: "SDa@x" },
      { capture: B, callKey: "SDb@x#ingress1" }
    ])
  })

  it("refuses a line short of two words, naming it", () => {
    const out = parseCells(`${A} SDa@x\nlonely\n`, "t.txt")
    expect(out).toBeInstanceOf(Refused)
    expect((out as Refused).message).toEqual('t.txt:2: expected "<capture> <call key>", got "lonely"')
  })
})

describe("the lines", () => {
  it("states a selection as it was typed", () => {
    expect(describeSelection({ kind: "filter", scope: "failed", rule: "died", whole: [] })).toEqual("--scope failed --rule died")
    expect(describeSelection({ kind: "universe", universe: "standing", maxSpan: 120, whole: [] })).toEqual("--standing --max-span 120")
  })

  it("prints a resolution per capture with its gone and added keys", () => {
    expect(resolutionLines({
      requestId: 3,
      prepId: 2,
      lane: "paused",
      knobs: { concurrency: 8, batchSize: 1000, memoryMax: "4G", startsPerSecond: 0, cleanup: true },
      captures: [{
        capture: A,
        kind: "full",
        planned: [{ capture: A, callKey: "K1", origin: "selected", caseId: "c1", caseHash: "h", egressEndpoint: null }],
        gone: [{ capture: A, callKey: "K2", origin: "selected", gone: "refused", reasons: ["source-call-incomplete"] }],
        added: ["K3"]
      }]
    })).toEqual([
      "request 3 against prep 2 on paused: 1 capture(s) — 1 full, 0 as cases; 1 planned, 1 gone, 1 added by the prep",
      `  ${A}  full   1 planned  1 gone  1 added`,
      "    gone K2: refused source-call-incomplete",
      "    added K3"
    ])
  })
})
