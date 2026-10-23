/**
 * The compare report (`sip.compare/1`): one object per call that moved, each
 * cell with its links and — where its bundle is on disk — its replay ladder as
 * the text API renders it and the known-bug rules its confrontation cited; and
 * the text that renders the document. The store is a stub: the documents and
 * flows a reference ladder reads are absent, which the document says as null.
 */
import { NodeServices } from "@effect/platform-node"
import { CompareReport } from "@sip/contracts"
import { CaseDocuments, Compare, Cuts, type Model } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as CallEvidence from "../../src/report/call-evidence.js"
import * as Report from "../../src/report/compare-report.js"

const CAPTURE = "cap.pcap.gz"
const LANE = "fake"
const CASE = `${CAPTURE}-case1`

const VIEWER = "http://viewer.test:5175"

const code = (tree: string): Model.Provenance => [
  { name: "app", sha: "a".repeat(40), dirty: false, tree },
  { name: "core", sha: "b".repeat(40), dirty: false, tree: "u1" }
]

const roots: Array<string> = []
afterEach(() => {
  for (const root of roots.splice(0)) fs.rmSync(root, { recursive: true, force: true })
})

/** A bundle holding one cell of CASE: a recording of one INVITE, and a confrontation citing a known bug. */
const bundle = (): string => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "compare-report-"))
  roots.push(root)
  const dir = CallEvidence.cellDirOf(path.join(root, "run2"), CASE, LANE)
  fs.mkdirSync(path.join(dir, "recording"), { recursive: true })
  fs.writeFileSync(
    path.join(dir, "recording", "A.jsonl"),
    JSON.stringify({
      seq: 1,
      dir: "out",
      at_us: 1000,
      step: "s1",
      raw: "INVITE sip:b@example.invalid SIP/2.0\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    }) + "\n"
  )
  fs.writeFileSync(
    path.join(dir, "confrontation.ndjson"),
    [
      { class: "known-bug", rule: "via-received-param-ignored", ticket: "tracker/1" },
      { class: "known-bug", rule: "via-received-param-ignored", ticket: "tracker/1" },
      { class: "accepted", rule: "via-stack-owned" }
    ].map((row) => JSON.stringify(row)).join("\n") + "\n"
  )
  return path.join(root, "run2")
}

const cell = (over: Partial<Compare.Cell>): Compare.Cell => ({
  capture: CAPTURE,
  caseId: CASE,
  runId: 1,
  lane: LANE,
  env: "main-checkout",
  branch: "main",
  laneHash: "lane1",
  prepId: null,
  bundlePath: "/nowhere/run1",
  passed: false,
  verdict: "failed",
  skipped: null,
  failedStep: "s6",
  documentHash: "doc1",
  mainError: { kind: "died", rule: "expect-timed-out", rfc: null, step: "s6", expected: "200 to INVITE", detail: "" },
  deltas: { accepted: 3, knownBug: 0, unlisted: 0, unknown: 0 },
  code: code("t1"),
  ...over
})

const side = (selector: string, cells: ReadonlyArray<Compare.Cell>): Compare.Side => ({
  selector,
  runs: cells.length,
  sweeps: [{ id: 7, universe: "named", selection: CAPTURE, captures: 1 }],
  cells,
  elsewhere: Compare.NOWHERE_ELSE
})

/** The store as the report reads it: no case document and no flows held. */
const store = Layer.succeed(
  CaseDocuments.Service,
  CaseDocuments.Service.of({
    documents: () => Effect.succeed(new Map()),
    flowsOf: () => Effect.succeed(new Map()),
    fieldOfCases: () => Effect.succeed(new Map())
  })
)

/** The cut store, never asked: with no flows held, no envelope is read. */
const cuts = Layer.succeed(Cuts.Service, {} as Cuts.Interface)

/** Ladders drawn as the title, then each recorded line: what the report carries is what it was drawn. */
const ladders = Layer.succeed(
  Report.Ladders,
  Report.Ladders.of({
    replay: (recorded, title) => [title, ...Object.values(recorded.recording).flatMap((text) => text.split("\n"))],
    reference: () => Effect.succeed(null)
  })
)

const documentOf = (comparison: Compare.Comparison, options: Report.ReportOptions) =>
  Effect.runPromise(
    Report.document(comparison, options).pipe(
      Effect.provide(Layer.mergeAll(store, cuts, ladders, NodeServices.layer))
    )
  )

describe("the compare document", () => {
  it("lists the call that moved with both cells, its ladder where the bundle stands, and null where it is gone", async () => {
    const after = cell({
      runId: 2,
      env: "dev-7-x",
      branch: "feat/7-x",
      bundlePath: bundle(),
      passed: true,
      verdict: "ok",
      failedStep: null,
      mainError: null,
      deltas: { accepted: 45, knownBug: 2, unlisted: 0, unknown: 0 },
      code: code("t2")
    })
    const comparison = Compare.compare(side("@current/@lane:fake", [cell({})]), side("@env:dev-7-x", [after]))
    const doc = await documentOf(comparison, { all: false, ladders: 10, only: 100, viewer: VIEWER, ladderChanged: false })

    expect(CompareReport.decode(JSON.parse(JSON.stringify(doc)))).toEqual(doc)
    expect(doc.schema_version).toBe("sip.compare/1")
    expect(doc.after.components).toEqual([{ name: "app", shas: ["a".repeat(40)] }, { name: "core", shas: ["b".repeat(40)] }])
    expect(doc.counts["red→green"]).toEqual({ sameDocument: 1, documentChanged: 0 })
    expect(doc.calls).toHaveLength(1)
    const call = doc.calls[0]!
    expect([call.alias, call.class, call.code, call.document]).toEqual([CASE, "red→green", "different", "same"])
    expect(call.before.ladder).toBeNull()
    expect(call.before.knownBugs).toBeNull()
    expect(call.before.mainError?.expected).toBe("200 to INVITE")
    expect(call.after.ladder?.[0]).toBe(`run2 ${CASE}`)
    expect(call.after.ladder?.some((line) => line.includes("INVITE"))).toBe(true)
    expect(call.after.knownBugs).toEqual([{ rule: "via-received-param-ignored", ticket: "tracker/1", records: 2}])
    expect(call.after.bless).toBe("known-bug")
    expect(call.ref).toBeNull()
    // Only one of the two ladders was read: the call states no delta.
    expect(call.delta).toBeNull()
    expect(call.after.links.text).toBe(
      `${VIEWER}/api/triage/${CAPTURE}/runs/run2/text?lane=${LANE}&case=${encodeURIComponent(CASE)}`
    )
    expect(call.after.links.study).toContain(encodeURIComponent(`run2/${CASE}__pivot-replay__${LANE}`))
    expect(doc.before.sweeps).toEqual([{ id: 7, universe: "named", selection: CAPTURE, captures: 1 }])
  })

  it("leaves an unchanged call out unless every call is asked for, and reads no ladder past the budget", async () => {
    const comparison = Compare.compare(side("run1", [cell({})]), side("run2", [cell({ runId: 2 })]))
    expect((await documentOf(comparison, { all: false, ladders: 10, only: 100, viewer: VIEWER, ladderChanged: false })).calls).toEqual([])
    const all = await documentOf(comparison, { all: true, ladders: 0, only: 100, viewer: VIEWER, ladderChanged: false })
    expect(all.calls.map((call) => [call.class, call.before.ladder, call.ref])).toEqual([["unchanged", null, null]])
  })

  it("renders as text: the headline, each side, the counts, then each call with its cells and ladders", async () => {
    const comparison = Compare.compare(
      side("@current", [cell({})]),
      side("@env:dev-7-x", [cell({ runId: 2, failedStep: "s24", code: code("t2") })])
    )
    const text = Report.render(await documentOf(comparison, { all: false, ladders: 10, only: 100, viewer: VIEWER, ladderChanged: false }), 20)
    const lines = text.split("\n")
    expect(lines[0]).toContain("compare @current → @env:dev-7-x: the two sides ran DIFFERENT code")
    expect(text).toContain("sweeps 7 (cap.pcap.gz)")
    expect(text).toContain("red→red-moved    1 (same document 1, document changed 0)")
    expect(text).toContain(`== red→red-moved  ${CAPTURE}  ${CASE}`)
    expect(text).toContain("after  run2 (main-checkout@main) failed at s24 · died expect-timed-out at s6 waiting 200 to INVITE")
    expect(text).toContain("-- reference ladder: not read")
    // The before cell's bundle is gone: no ladder delta is read.
    expect(text).toContain("  ladder delta: not read")
  })
})
