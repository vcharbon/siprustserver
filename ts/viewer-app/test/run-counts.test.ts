/**
 * The one line under a study's title: how many calls the capture holds, and
 * how the run ruled on each. The refused calls are the exclusion records the
 * cut wrote; the rest are the standing run's cases, green, red or skipped.
 */
import type { ExclusionRecord } from "@sip/viewer-core"
import { describe, expect, it } from "vitest"
import { runCounts } from "../src/run-counts.ts"

const refusal = (caseId: string): ExclusionRecord => ({
  caseId,
  reason: "missing-upstream-leg",
  callIds: [],
  legs: [],
  sut: [],
  groups: [],
  evidence: [],
  line: `${caseId} EXCLUDED missing-upstream-leg`
})

const standing = {
  cases: 3,
  failed: 1,
  caseResults: [
    { caseId: "c-1", passed: true, failedStep: null, skipped: null },
    { caseId: "c-2", passed: false, failedStep: "s9", skipped: null },
    { caseId: "c-3", passed: false, failedStep: null, skipped: "blocked:claim-ambiguous" }
  ]
}

describe("runCounts", () => {
  it("counts the run's cases beside the cut's refusals: calls are both together", () => {
    expect(runCounts(standing, [refusal("r-1"), refusal("r-2")])).toEqual({
      calls: 5,
      refused: 2,
      green: 1,
      red: 1,
      skipped: 1
    })
  })

  it("is the refusals alone for a capture never run", () => {
    expect(runCounts(undefined, [refusal("r-1"), refusal("r-2")])).toEqual({
      calls: 2,
      refused: 2,
      green: 0,
      red: 0,
      skipped: 0
    })
  })

  it("counts the standing cut's cases as calls for a capture cut but never run", () => {
    expect(runCounts(undefined, [refusal("r-1")], 2)).toEqual({
      calls: 3,
      refused: 1,
      green: 0,
      red: 0,
      skipped: 0
    })
  })

  it("reads the calls off the run, not the cut, once a run stands", () => {
    expect(runCounts(standing, undefined, 7).calls).toBe(3)
  })

  it("refuses nothing where the cut wrote no exclusion document", () => {
    expect(runCounts(standing, undefined)).toEqual({
      calls: 3,
      refused: 0,
      green: 1,
      red: 1,
      skipped: 1
    })
  })
})
