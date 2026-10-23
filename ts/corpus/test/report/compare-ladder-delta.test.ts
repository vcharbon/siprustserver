/**
 * The ladder delta of the compare report over real bundles, drawn by the
 * viewer's own ladder text (`call-ladders.ts`): two fake-lane runs of one call
 * read no delta; a real-lane run of it, whose endpoints minted their own
 * Call-IDs, tags, branches and CSeq bases at their own wall-clock times, reads
 * no delta against the fake run either; a real-lane run that missed an ACK
 * reads one, at that ACK. `--ladder-changed` lists the calls whose ladders
 * moved, an unchanged verdict notwithstanding. The store is a stub.
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
import * as CallLadders from "../../src/report/call-ladders.js"
import * as CallEvidence from "../../src/report/call-evidence.js"
import * as Report from "../../src/report/compare-report.js"

const CAPTURE = "cap.pcap.gz"
const CASE = `${CAPTURE}-case1`
const FAKE = "fake"
const REAL = "real"
const VIEWER = "http://viewer.test:5175"

/** What one run's endpoints minted, and when each message went. */
interface Run {
  readonly callA: string
  readonly callB: string
  readonly tags: readonly [string, string, string, string]
  readonly branches: readonly [string, string, string, string]
  readonly cseqB: number
  readonly times: ReadonlyArray<number>
  readonly ackB: boolean
}

const FAKE_RUN: Run = {
  callA: "A-1d2c3b4a5f6e7d8c9@pivot.invalid",
  callB: "SUT-0001@sut.invalid",
  tags: ["A-1d2c3b4a5f6e7d8c9-tag", "sut-a-0001", "sut-b-0001", "B-1d2c3b4a5f6e7d8c9-tag"],
  branches: ["z9hG4bK-A-1", "z9hG4bK-sut-1", "z9hG4bK-A-2", "z9hG4bK-sut-2"],
  cseqB: 1,
  times: [0, 2000, 6000, 347000, 349000, 467000, 469000],
  ackB: true
}

const REAL_RUN: Run = {
  callA: "A-18de5d94facffeb51@pivot.invalid",
  callB: "f55f7c92f6ad965ef36dd7418b0efe59@192.0.2.1",
  tags: ["A-18de5d94facffeb51-tag", "as9f8e7d6c", "bs1029384756", "B-18de5d94facffeb51-tag"],
  branches: ["z9hG4bK-A-18de5d94-1", "z9hG4bK5e8a7b6c", "z9hG4bK-A-18de5d94-2", "z9hG4bK0f1e2d3c"],
  cseqB: 603448825,
  times: [256, 18151, 48074, 391220, 405418, 524944, 536624],
  ackB: true
}

const sip = (start: string, lines: ReadonlyArray<string>): string =>
  [start, ...lines, "Content-Length: 0", "", ""].join("\r\n")

const head = (branch: string, from: string, to: string | null, callId: string, cseq: string): ReadonlyArray<string> => [
  `Via: SIP/2.0/UDP 192.0.2.10:5060;branch=${branch}`,
  `From: <sip:a@example.invalid>;tag=${from}`,
  `To: <sip:b@example.invalid>${to === null ? "" : `;tag=${to}`}`,
  `Call-ID: ${callId}`,
  `CSeq: ${cseq}`
]

/** Leg A (the harness calls the SUT) and leg B (the SUT calls the harness), as recorded lines. */
const recording = (run: Run): Readonly<Record<string, ReadonlyArray<Record<string, unknown>>>> => {
  const [tagA, sutA, sutB, tagB] = run.tags
  const [brA, brSut, brAck, brSutAck] = run.branches
  const t = run.times
  const legA = [
    { seq: 1, dir: "out", at_us: t[0], step: "s1", raw: sip("INVITE sip:b@sut.invalid SIP/2.0", head(brA, tagA, null, run.callA, "1 INVITE")) },
    { seq: 2, dir: "in", at_us: t[1], step: "s2", raw: sip("SIP/2.0 100 Trying", head(brA, tagA, null, run.callA, "1 INVITE")) },
    { seq: 3, dir: "in", at_us: t[4], step: "s5", raw: sip("SIP/2.0 200 OK", head(brA, tagA, sutA, run.callA, "1 INVITE")) },
    { seq: 4, dir: "out", at_us: t[5], step: "s6", raw: sip("ACK sip:sut@sut.invalid SIP/2.0", head(brAck, tagA, sutA, run.callA, "1 ACK")) }
  ]
  const legB = [
    { seq: 1, dir: "in", at_us: t[2], step: "s3", raw: sip("INVITE sip:b@example.invalid SIP/2.0", head(brSut, sutB, null, run.callB, `${run.cseqB} INVITE`)) },
    { seq: 2, dir: "out", at_us: t[3], step: "s4", raw: sip("SIP/2.0 200 OK", head(brSut, sutB, tagB, run.callB, `${run.cseqB} INVITE`)) },
    ...(run.ackB
      ? [{ seq: 3, dir: "in", at_us: t[6], step: "s7", raw: sip("ACK sip:b@192.0.2.20 SIP/2.0", head(brSutAck, sutB, tagB, run.callB, `${run.cseqB} ACK`)) }]
      : [])
  ]
  return { A: legA, B: legB }
}

const roots: Array<string> = []
afterEach(() => {
  for (const root of roots.splice(0)) fs.rmSync(root, { recursive: true, force: true })
})

/** A bundle holding one cell of CASE on `lane`, recorded as `run` went. */
const bundle = (runId: number, lane: string, run: Run): string => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "compare-ladder-delta-"))
  roots.push(root)
  const bundlePath = path.join(root, `run${runId}`)
  const dir = CallEvidence.cellDirOf(bundlePath, CASE, lane)
  fs.mkdirSync(path.join(dir, "recording"), { recursive: true })
  for (const [leg, lines] of Object.entries(recording(run))) {
    fs.writeFileSync(path.join(dir, "recording", `${leg}.jsonl`), lines.map((line) => JSON.stringify(line)).join("\n") + "\n")
  }
  return bundlePath
}

const PROVENANCE: Model.Provenance = [{ name: "app", sha: "a".repeat(40), dirty: false, tree: "t1" }]

const cell = (runId: number, lane: string, run: Run): Compare.Cell => ({
  capture: CAPTURE,
  caseId: CASE,
  runId,
  lane,
  env: "main-checkout",
  branch: "main",
  laneHash: lane,
  prepId: null,
  bundlePath: bundle(runId, lane, run),
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  documentHash: "doc1",
  mainError: null,
  deltas: null,
  code: PROVENANCE
})

const side = (selector: string, cells: ReadonlyArray<Compare.Cell>): Compare.Side => ({
  selector,
  runs: cells.length,
  sweeps: [],
  cells,
  elsewhere: Compare.NOWHERE_ELSE
})

const store = Layer.succeed(
  CaseDocuments.Service,
  CaseDocuments.Service.of({
    documents: () => Effect.succeed(new Map()),
    flowsOf: () => Effect.succeed(new Map()),
    fieldOfCases: () => Effect.succeed(new Map())
  })
)

const services = Layer.mergeAll(
  store,
  CallLadders.layer.pipe(Layer.provide(Layer.succeed(Cuts.Service, {} as Cuts.Interface))),
  Layer.succeed(Cuts.Service, {} as Cuts.Interface),
  NodeServices.layer
)

const compare = (before: Compare.Cell, after: Compare.Cell, over: Partial<Report.ReportOptions> = {}) =>
  Effect.runPromise(
    Report.document(Compare.compare(side("@run:1", [before]), side("@run:2", [after])), {
      all: true,
      ladders: 10,
      only: 100,
      viewer: VIEWER,
      ladderChanged: false,
      ...over
    }).pipe(Effect.provide(services))
  )

describe("the compare report's ladder delta", () => {
  it("reads no delta across two fake-lane runs of one call", async () => {
    const doc = await compare(cell(1, FAKE, FAKE_RUN), cell(2, FAKE, FAKE_RUN))
    expect(CompareReport.decode(JSON.parse(JSON.stringify(doc)))).toEqual(doc)
    expect(doc.calls[0]?.delta).toEqual({ firstDifference: null, differingLines: 0 })
  })

  it("reads no delta between a fake-lane run and a real-lane run whose endpoints minted their own ids", async () => {
    const doc = await compare(cell(1, REAL, REAL_RUN), cell(2, FAKE, FAKE_RUN))
    expect(doc.calls[0]?.before.ladder).not.toEqual(doc.calls[0]?.after.ladder)
    expect(doc.calls[0]?.delta).toEqual({ firstDifference: null, differingLines: 0 })
  })

  it("reads the missing ACK of a real-lane run as the first difference, under an unchanged verdict", async () => {
    const doc = await compare(cell(1, FAKE, FAKE_RUN), cell(2, REAL, { ...REAL_RUN, ackB: false }))
    const call = doc.calls[0]!
    expect(call.class).toBe("unchanged")
    expect(call.delta?.differingLines).toBe(1)
    expect(call.delta?.firstDifference?.line).toBe(6)
    expect(call.delta?.firstDifference?.before).toContain("| ACK |")
    expect(call.delta?.firstDifference?.after).toBeNull()
    const text = Report.render(doc, 20)
    expect(text).toContain("ladder delta: 1 line(s) differ; first at m6")
  })

  it("lists, under --ladder-changed, only the calls whose ladders moved", async () => {
    const same = await compare(cell(1, REAL, REAL_RUN), cell(2, FAKE, FAKE_RUN), { all: false, ladderChanged: true })
    expect(same.calls).toEqual([])
    const moved = await compare(cell(1, FAKE, FAKE_RUN), cell(2, REAL, { ...REAL_RUN, ackB: false }), {
      all: false,
      ladderChanged: true
    })
    expect(moved.calls.map((call) => [call.class, call.delta?.differingLines])).toEqual([["unchanged", 1]])
  })
})
