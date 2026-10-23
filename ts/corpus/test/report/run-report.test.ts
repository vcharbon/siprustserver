/**
 * The run report (`sip.run-report/1`): one run set's calls as the compare
 * report's own cells, read off the run rows or off the summary a citation kept
 * of a run whose row is gone, and the text that renders it.
 */
import { RunReport } from "@sip/contracts"
import { Compare, type Model, type RunSets, type RunSummaries } from "@sip/store"
import { describe, expect, it } from "vitest"
import * as Report from "../../src/report/run-report.js"

const CAPTURE = "cap.pcap.gz"
const LANE = "fake"
const VIEWER = "http://viewer.test:5175"

const code: Model.Provenance = [{ name: "app", sha: "a".repeat(40), dirty: false, tree: "t1" }]

const cell = (caseId: string, over: Partial<Compare.Cell> = {}): Compare.Cell => ({
  capture: CAPTURE,
  caseId,
  runId: 5,
  lane: LANE,
  env: "main-checkout",
  branch: "main",
  laneHash: "lane1",
  prepId: 2,
  bundlePath: "/nowhere/run5",
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  documentHash: "doc1",
  mainError: null,
  deltas: { accepted: 1, knownBug: 0, unlisted: 0, unknown: 0 },
  code,
  ...over
})

const run: RunSets.RunRow = {
  id: 5,
  capture: CAPTURE,
  lane: LANE,
  setKind: "full",
  status: "done",
  env: "main-checkout",
  branch: "main",
  code,
  sweepId: 11,
  jobId: null,
  startedAt: "2026-10-01T10:00:00.000Z",
  finishedAt: "2026-10-01T10:02:00.000Z",
  cases: 3,
  failed: 1,
  cleanup: null
}

const side = (cells: ReadonlyArray<Compare.Cell>): Compare.Side => ({
  selector: "@run:5",
  runs: 1,
  sweeps: [{ id: 11, universe: "named", selection: CAPTURE, captures: 1 }],
  cells,
  elsewhere: Compare.NOWHERE_ELSE
})

const failing = cell(`${CAPTURE}-case2`, {
  passed: false,
  verdict: "failed",
  failedStep: "s6",
  mainError: { kind: "died", rule: "expect-timed-out", rfc: null, step: "s6", expected: "200 to INVITE", detail: "" }
})

const summarised: RunSummaries.Summarised = {
  run: {
    runId: 4321,
    capture: CAPTURE,
    lane: LANE,
    setKind: "cases",
    status: "done",
    detail: null,
    cases: 1,
    failed: 1,
    env: "main-checkout",
    branch: "main",
    code,
    sweepId: 7,
    jobId: null,
    prepId: 2,
    startedAt: "2026-09-20T08:00:00.000Z",
    finishedAt: null,
    citedBy: ["tracker/12"]
  },
  cells: [{
    caseId: `${CAPTURE}-case2`,
    capture: CAPTURE,
    lane: LANE,
    callKey: "k2",
    passed: false,
    verdict: "failed",
    skipped: null,
    failedStep: "s9",
    mainError: { kind: "failure", rule: "unexpected-final", rfc: null, step: "s9", expected: null, detail: "" },
    code,
    env: "main-checkout",
    branch: "main"
  }]
}

describe("the run report of a run set", () => {
  const doc = Report.fromRuns({
    selector: "@run:5",
    runs: [run],
    total: 1,
    side: side([cell(`${CAPTURE}-case1`), failing, cell(`${CAPTURE}-case3`, { skipped: "lane-refuses", passed: false })]),
    viewer: VIEWER
  })

  it("is a document of the format, every call a compare report cell with its links", () => {
    expect(RunReport.decode(JSON.parse(JSON.stringify(doc)))).toEqual(doc)
    expect(doc.source).toBe("runs")
    expect(doc.counts).toEqual({ calls: 3, passed: 1, failed: 1, skipped: 1 })
    expect(doc.headline).toBe("3 calls: 1 passed, 1 failed, 1 skipped")
    expect(doc.runs[0]).toMatchObject({ id: 5, finishedAt: "2026-10-01T10:02:00.000Z", citedBy: [] })
    const second = doc.calls[1]!
    expect([second.caseId, second.cell.failedStep, second.cell.mainError?.expected]).toEqual([
      `${CAPTURE}-case2`,
      "s6",
      "200 to INVITE"
    ])
    expect(second.cell.links.text).toBe(
      `${VIEWER}/api/triage/${CAPTURE}/runs/run5/text?lane=${LANE}&case=${encodeURIComponent(`${CAPTURE}-case2`)}`
    )
    expect(second.cell.ladder).toBeNull()
  })

  it("renders as text: the headline, each run, then each call", () => {
    const text = Report.render(doc, 20)
    expect(text.split("\n")[0]).toBe("report @run:5: 3 calls: 1 passed, 1 failed, 1 skipped")
    expect(text).toContain(`  run5 ${CAPTURE} on ${LANE}: full, done; env main-checkout@main`)
    expect(text).toContain(`== ${CAPTURE}-case2`)
    expect(text).toContain("run5 (main-checkout@main) failed at s6 · died expect-timed-out at s6 waiting 200 to INVITE")
  })
})

describe("the run report of a summarised run", () => {
  const doc = Report.fromSummary("@run:4321", summarised, VIEWER)

  it("reads the summary's cells, says its source, and names who cites the run", () => {
    expect(RunReport.decode(JSON.parse(JSON.stringify(doc)))).toEqual(doc)
    expect(doc.source).toBe("summaries")
    expect(doc.runs).toEqual([expect.objectContaining({ id: 4321, setKind: "cases", citedBy: ["tracker/12"] })])
    expect(doc.calls.map((call) => [call.caseId, call.cell.run, call.cell.failedStep, call.cell.bless])).toEqual([
      [`${CAPTURE}-case2`, 4321, "s9", "unmeasured"]
    ])
    expect(doc.counts).toEqual({ calls: 1, passed: 0, failed: 1, skipped: 0 })
  })

  it("says in its text that the row is gone and the summary is what stands", () => {
    const text = Report.render(doc, 20)
    expect(text).toContain("run 4321's row is gone; the summary a citation kept (tracker/12) stands for it")
    expect(text).toContain("failure unexpected-final at s9")
  })
})
