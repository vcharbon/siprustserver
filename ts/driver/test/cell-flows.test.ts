/**
 * The capture-side flows document a replay cell confronts against.
 *
 * Only the legs the case cites are read and decoded against the contract: the
 * document of a capture of thousands of calls is larger than V8 will hold as
 * one string, and a cell that decoded it whole would cost the capture per
 * cell. The claims under test are that no whole-string read of it is ever
 * made, and that a leg the case never cites is never decoded.
 */
import type { Campaign } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { runCampaign } from "../src/campaign.js"
import { CONFRONTATION_FILE, ERROR_FILE } from "../src/layout.js"
import { pivotDocument, rig, runDir, stubTests } from "./harness.js"

const CASE = pivotDocument("transparent-defect.v3.json")

const dirs: Array<string> = []
afterEach(() => {
  for (const d of dirs.splice(0)) fs.rmSync(d, { recursive: true, force: true })
})

const legOf = (index: number) => ({
  call_id: `call-${index}@10.0.0.1`,
  hops: [{ a: "10.0.0.1:5060", b: "10.0.0.2:5060" }],
  invite: null,
  final_status: 200,
  saw_180: false,
  terminated_by: "BYE",
  tokens: [`token-${index}`],
  msgs: [{
    ts_us: 1_000_000 + index,
    src: "10.0.0.1:5060",
    dst: "10.0.0.2:5060",
    hop: 0,
    retx: false,
    summary: {
      kind: "request",
      method: "INVITE",
      uri: `sip:${index}@10.0.0.2`,
      cseq: { seq: 1, method: "INVITE" },
      from: { uri: `sip:caller-${index}@10.0.0.1`, tag: "f1" },
      to: { uri: `sip:${index}@10.0.0.2`, tag: null }
    },
    raw: `INVITE sip:${index} SIP/2.0\r\n\r\n`
  }]
})

/** A whole flows document, as `sipflow --enrich` writes one, under `dir`. */
const flowsDocument = (dir: string, legs: Array<unknown>): string => {
  const file = path.join(dir, "capture.flows.json")
  fs.writeFileSync(
    file,
    JSON.stringify({
      schema: 5,
      emit_headers: [],
      decode_stats: {
        records: legs.length,
        non_ip: 0,
        non_udp: 0,
        snap_truncated: 0,
        datagrams: legs.length,
        fragments: 0,
        reassembled: 0,
        frag_dropped: 0,
        tail_truncated: 0
      },
      flow_stats: { sip_messages: legs.length, capture_dups: 0, parse_failed: 0, non_sip: 0 },
      legs,
      groups: []
    }, null, 2)
  )
  return file
}

/** One replay cell pointed at `flows`, and every whole-string read it made. */
const confront = async (legs: Array<unknown>) => {
  const dir = runDir("cell-flows")
  dirs.push(dir)
  const flows = flowsDocument(dir, legs)
  const reads: Array<string> = []
  const cell: Campaign.PivotReplayCell = {
    kind: "pivot-replay",
    case: CASE,
    lane: "stub-lane",
    flows
  }
  const run = await Effect.runPromise(
    runCampaign(
      { campaign: "stub", cells: [cell] },
      { runDir: dir, ts: "2026-08-25T00:00:00Z", tests: stubTests(0, true) }
    ).pipe(Effect.provide(rig({ reads })))
  )
  const cellDir = path.join(dir, run.index.cells[0]!.dir)
  return { run, flows, reads, cellDir }
}

/** Legs 0..6: the fixture cites legs 5 and 6. */
const LEGS = Array.from({ length: 7 }, (_, index) => legOf(index))

describe("a replay cell's flows document", () => {
  it("is confronted against without ever being read as one string", async () => {
    const { cellDir, flows, reads, run } = await confront(LEGS)
    expect(run.index.cells[0]!.passed).toBe(true)
    expect(fs.existsSync(path.join(cellDir, CONFRONTATION_FILE))).toBe(true)
    // The pivot document beside it IS read whole, which is what says the watch
    // is wired at all: the flows document is the one file that must not be.
    expect(reads).toContain(CASE)
    expect(reads).not.toContain(flows)
  })

  it("fails the cell on a cited leg the contract refuses, naming that leg", async () => {
    const legs = LEGS.map((leg, index) => (index === 5 ? { ...leg, final_status: "200" } : leg))
    const { cellDir, run } = await confront(legs)
    expect(run.index.cells[0]!.passed).toBe(false)
    const error = fs.readFileSync(path.join(cellDir, ERROR_FILE), "utf8")
    expect(error).toContain("FlowsLegRefused")
    expect(error).toContain("legs[5]")
  })

  it("never decodes a leg the case does not cite", async () => {
    const legs = LEGS.map((leg, index) => (index === 1 ? { ...leg, final_status: "200" } : leg))
    const { cellDir, run } = await confront(legs)
    expect(run.index.cells[0]!.passed).toBe(true)
    expect(fs.existsSync(path.join(cellDir, CONFRONTATION_FILE))).toBe(true)
  })

  it("compares nothing for a coordinate past the document, and still runs the cell", async () => {
    const { cellDir, run } = await confront([legOf(0), legOf(1), legOf(2)])
    expect(run.index.cells[0]!.passed).toBe(true)
    expect(fs.existsSync(path.join(cellDir, CONFRONTATION_FILE))).toBe(true)
  })
})
