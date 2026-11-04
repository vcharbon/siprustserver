/** A retried cell's attempts in the case history: one line each, the kept one marked; none for one attempt. */
import { describe, expect, it } from "vitest"
import type { CampaignResult } from "../src/api.ts"
import { attemptLines } from "../src/campaign-attempts.ts"

const result = (attempts?: CampaignResult["attempts"]): CampaignResult => ({
  caseId: "fam/one",
  outcome: "pass",
  cell: "one~3",
  log: null,
  detail: null,
  checks: [],
  ...(attempts === undefined ? {} : { attempts })
})

describe("a cell's attempts", () => {
  it("lists each attempt with its outcome, its failed checks, the kept one and its bundle", () => {
    expect(attemptLines(result([
      { cell: "one", outcome: "failure", failed: 2, kept: false },
      { cell: null, outcome: "error", failed: 0, kept: false },
      { cell: "one~3", outcome: "pass", failed: 0, kept: true }
    ]))).toEqual([
      "attempt 1/3 · failure (2 failed) · one",
      "attempt 2/3 · error · no bundle",
      "attempt 3/3 · pass · kept · one~3"
    ])
  })

  it("lists none for a cell replayed once", () => {
    expect(attemptLines(result())).toEqual([])
  })
})
