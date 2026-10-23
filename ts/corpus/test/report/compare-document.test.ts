/**
 * The documents a selector names, as the command and the viewer both read
 * them: a selector the grammar or the store refuses is refused with its
 * reason; a set naming no run is refused, unless it is one cited run whose
 * summary a citation kept, which is reported off that summary; a set the
 * store holds is reported off its rows. The store is a stub.
 */
import { NodeServices } from "@effect/platform-node"
import { CaseDocuments, Compare, Cuts, RunSets, RunSummaries, StoreBinding } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { describe, expect, it } from "vitest"
import * as CompareDocument from "../../src/report/compare-document.js"

const CAPTURE = "cap.pcap.gz"
const VIEWER = "http://viewer.test"

const cell = (runId: number, passed: boolean): Compare.Cell => ({
  capture: CAPTURE,
  caseId: `${CAPTURE}-case1`,
  runId,
  lane: "fake",
  env: "main-checkout",
  branch: "main",
  laneHash: "l1",
  prepId: 1,
  bundlePath: `/nowhere/run${runId}`,
  passed,
  verdict: passed ? "ok" : "failed",
  skipped: null,
  failedStep: passed ? null : "s4",
  documentHash: "d1",
  mainError: null,
  deltas: null,
  code: []
})

const sideOf = (selector: string, runs: number, cells: ReadonlyArray<Compare.Cell>): Compare.Side => ({
  selector,
  runs,
  sweeps: [],
  cells,
  elsewhere: Compare.NOWHERE_ELSE
})

type RunSelector = Parameters<RunSets.Interface["runs"]>[0]

/** The run a selector names by id, whatever its spelling. */
const runOf = (selector: RunSelector): number | undefined => {
  const term = selector.terms.find((one) => one._tag === "run")
  return term?._tag === "run" ? term.id : undefined
}

/** Run 5 holds one failing call, run 6 the same call passing; nothing else is a run. */
const SETS = Layer.succeed(
  RunSets.Service,
  RunSets.Service.of({
    refusal: (selector) =>
      Effect.succeed(
        selector.terms.some((one) => one._tag === "sha")
          ? "@sha:abcdef0 names 2 commits — abcdef01, abcdef02"
          : null
      ),
    runs: (selector) =>
      Effect.succeed(runOf(selector) === 5
        ? [{
          id: 5,
          capture: CAPTURE,
          lane: "fake",
          setKind: "full",
          status: "done",
          env: "main-checkout",
          branch: "main",
          code: [],
          sweepId: null,
          jobId: null,
          startedAt: "2026-10-01T10:00:00.000Z",
          finishedAt: "2026-10-01T10:01:00.000Z",
          cases: 1,
          failed: 1,
          cleanup: null
        }]
        : []),
    pair: (before, after) => {
      const of = (selector: RunSelector) =>
        runOf(selector) === 5
          ? sideOf(selector.text, 1, [cell(5, false)])
          : runOf(selector) === 6
          ? sideOf(selector.text, 1, [cell(6, true)])
          : sideOf(selector.text, 0, [])
      return Effect.succeed({ before: of(before), after: of(after) })
    }
  })
)

/** Run 7 is gone; a citation kept its summary. */
const SUMMARIES = Layer.succeed(
  RunSummaries.Service,
  RunSummaries.Service.of({
    of: (runId) =>
      Effect.succeed(runId !== 7 ? null : {
        run: {
          runId: 7,
          capture: CAPTURE,
          lane: "fake",
          setKind: "cases",
          status: "done",
          detail: null,
          cases: 1,
          failed: 0,
          env: "main-checkout",
          branch: "main",
          code: [],
          sweepId: null,
          jobId: null,
          prepId: 1,
          startedAt: "2026-09-01T10:00:00.000Z",
          finishedAt: null,
          citedBy: ["tracker/1"]
        },
        cells: [{
          caseId: `${CAPTURE}-case1`,
          capture: CAPTURE,
          lane: "fake",
          callKey: "k1",
          passed: true,
          verdict: "ok",
          skipped: null,
          failedStep: null,
          mainError: null,
          code: [],
          env: "main-checkout",
          branch: "main"
        }]
      })
  })
)

const STORE = Layer.mergeAll(
  SETS,
  SUMMARIES,
  Layer.succeed(
    CaseDocuments.Service,
    CaseDocuments.Service.of({
      documents: () => Effect.succeed(new Map()),
      flowsOf: () => Effect.succeed(new Map()),
      fieldOfCases: () => Effect.succeed(new Map())
    })
  ),
  Layer.succeed(Cuts.Service, {} as Cuts.Interface),
  StoreBinding.layer,
  NodeServices.layer
)

const run = <A, E>(effect: Effect.Effect<A, E, CompareDocument.Services | NodeServices.NodeServices>) =>
  Effect.runPromise(effect.pipe(Effect.provide(STORE)))

const compare = (before: string, after: string) =>
  run(CompareDocument.compare({ before, after, lane: "fake", all: true, ladders: 0, only: 10, viewer: VIEWER }))

const report = (selector: string) => run(CompareDocument.report({ selector, lane: "fake", viewer: VIEWER, runs: 20 }))

describe("the compare document", () => {
  it("is the comparison of the two sides the store reads", async () => {
    const answer = await compare("@run:5", "run6")
    expect(answer._tag).toBe("document")
    if (answer._tag !== "document") return
    expect(answer.document.calls.map((call) => call.class)).toEqual(["red→green"])
  })

  it("refuses a selector the grammar cannot read, the store refuses, or that names no run, saying why", async () => {
    expect(await compare("@nope:1", "@run:6")).toEqual({ _tag: "refused", reason: expect.stringContaining("is no selector term") })
    expect(await compare("@sha:abcdef0", "@run:6")).toEqual({
      _tag: "refused",
      reason: "@sha:abcdef0 names 2 commits — abcdef01, abcdef02"
    })
    expect(await compare("@run:5", "@run:99")).toEqual({ _tag: "refused", reason: "@run:99 names no run" })
  })
})

describe("the run report document", () => {
  it("reports a set the store holds off its rows", async () => {
    const answer = await report("@run:5")
    expect(answer._tag).toBe("document")
    if (answer._tag !== "document") return
    expect([answer.document.source, answer.document.headline, answer.document.runs.map((one) => one.id)]).toEqual([
      "runs",
      "1 call: 0 passed, 1 failed, 0 skipped",
      [5]
    ])
  })

  it("reports a cited run the wipe dropped off its kept summary, not as a run that never was", async () => {
    const answer = await report("run7")
    expect(answer._tag).toBe("document")
    if (answer._tag !== "document") return
    expect(answer.document.source).toBe("summaries")
    expect(answer.document.runs[0]?.citedBy).toEqual(["tracker/1"])
    expect(answer.document.calls.map((call) => call.cell.passed)).toEqual([true])
  })

  it("refuses a run neither the rows nor a summary hold", async () => {
    expect(await report("@run:8")).toEqual({ _tag: "refused", reason: "@run:8 names no run" })
  })
})

describe("a deployment's spelling of the current term", () => {
  it("is what a side's selector reads, and a refusal names it among the forms", async () => {
    const spelled = StoreBinding.layerWith({
      spelling: { current: "@main" },
      grammar: { isCapture: () => false },
      migrate: Effect.succeed([])
    })
    const refused = await Effect.runPromise(
      CompareDocument.compare({ before: "@current", after: "run6", lane: "fake", all: true, ladders: 0, only: 10, viewer: VIEWER })
        .pipe(Effect.provide(Layer.merge(STORE, spelled)))
    )
    expect(refused).toEqual({ _tag: "refused", reason: expect.stringContaining("one of @main,") })
  })
})
