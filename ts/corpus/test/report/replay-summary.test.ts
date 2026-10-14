/**
 * The replay summary: one line per cell this invocation ran — its first
 * failing step and main error — beside the call's current run, failures first.
 */
import type { Compare } from "@sip/store"
import { describe, expect, it } from "vitest"
import * as ReplaySummary from "../../src/report/replay-summary.js"

const C = "trace.pcap.gz"

const cell = (caseId: string, runId: number, over: Partial<Compare.Cell> = {}): Compare.Cell => ({
  capture: C,
  caseId,
  runId,
  lane: "fake",
  env: "task-1",
  branch: "chore/task-branch",
  laneHash: "l1",
  prepId: 100,
  bundlePath: `/w/${C}/run${runId}`,
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  documentHash: "d1",
  mainError: null,
  deltas: null,
  code: [],
  ...over
})

const failed = (caseId: string, runId: number, step: string): Compare.Cell =>
  cell(caseId, runId, {
    passed: false,
    verdict: "failed",
    failedStep: step,
    mainError: { kind: "died", rule: "expect-timed-out", rfc: null, step, expected: "200 to INVITE", detail: "" }
  })

describe("ReplaySummary.lines", () => {
  it("prints each cell's first failing step and main error beside the current run, failures first", () => {
    const own = [cell(`${C}-case1`, 9), failed(`${C}-case3`, 9, "s24"), cell(`${C}-case7`, 9)]
    const current = new Map([
      [`${C}/${C}-case1`, failed(`${C}-case1`, 5, "s6")],
      [`${C}/${C}-case3`, failed(`${C}-case3`, 5, "s24")]
    ])
    const lines = ReplaySummary.lines(own, current, "@current")
    expect(lines[0]).toBe("[summary] 3 cell(s): 1 failed, 2 passed; against @current: 1 moved, 1 unchanged, 1 never ran there")
    expect(lines[1]).toContain(`${C}-case3`)
    expect(lines[1]).toContain("failed at s24 · died expect-timed-out at s24 waiting 200 to INVITE")
    expect(lines[1]).toContain("@current run5 failed at s24")
    expect(lines[2]).toContain(`${C}-case1`)
    expect(lines[2]).toContain("passed")
    expect(lines[2]).toContain("@current run5 failed at s6")
    expect(lines[3]).toContain(`${C}-case7`)
    expect(lines[3]).toContain("@current: never ran")
  })

  it("prints a source fault beside the SUT's failure", () => {
    const source = { rule: "prack-2xx-or-481", rfc: "RFC 3262 §3", detail: "uas1: answered 481" }
    const both = failed(`${C}-case4`, 9, "s18")
    const besideIt = cell(`${C}-case4`, 9, { ...both, mainError: { ...both.mainError!, source } })
    const lines = ReplaySummary.lines([besideIt], new Map(), "@current")
    expect(lines[1]).toContain("died expect-timed-out at s18 waiting 200 to INVITE · beside it source-fault prack-2xx-or-481")
  })

  it("says a skipped cell is skipped, with the lane block", () => {
    const lines = ReplaySummary.lines([cell(`${C}-case2`, 9, { passed: false, skipped: "blocked:x" })], new Map(), "@current")
    expect(lines[0]).toContain("1 skipped")
    expect(lines[1]).toContain("skipped blocked:x")
  })
})
