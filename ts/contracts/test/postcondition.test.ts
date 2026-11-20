/** The claims a document's postconditions state (ADR-0060 appendix §4). */
import { describe, expect, it } from "vitest"
import { CALL_RECORDS, statedClaims } from "../src/postcondition.js"

describe("the claims postconditions state", () => {
  it("are its claims, a cdr count read as the call-records claim", () => {
    const checks = [{ field: "d", op: "exists" as const }]
    expect(statedClaims({ cdr: { count: 2, checks }, claims: { traces: { lines: 3 } } })).toEqual({
      traces: { lines: 3 },
      [CALL_RECORDS]: { count: 2, checks }
    })
  })

  it("carry a call-records claim as stated, a stated absence claiming nothing", () => {
    expect(statedClaims({ cdr: { absent: "no-records" }, claims: { [CALL_RECORDS]: { count: 1 } } })).toEqual({
      [CALL_RECORDS]: { count: 1 }
    })
    expect(statedClaims({ cdr: { absent: "no-records" } })).toEqual({})
    expect(statedClaims(undefined)).toEqual({})
  })
})
