/**
 * The source fault beside a main error: a call whose main error is the SUT's
 * failure and which also carries a source fault says both, the source fault as
 * a marker of its own; a call that failed on a source fault alone says it once,
 * as its main error.
 */
import { describe, expect, it } from "vitest"
import type { TriageMainError } from "../src/api.ts"
import { sourceBeside, sourceFaultText } from "../src/source-fault.ts"

const died: TriageMainError = {
  kind: "died",
  rule: null,
  rfc: null,
  step: "s18",
  expected: "481 to PRACK",
  detail: "gated on response 481; 200 arrived"
}

const source = { rule: "prack-2xx-or-481", rfc: "RFC 3262 §3", detail: "uas1: answered 481 — RFC 3262 §3" }

describe("sourceBeside", () => {
  it("reads the source fault a main error stands beside", () => {
    expect(sourceBeside({ ...died, source })).toEqual(source)
  })

  it("reads none where the main error carries none, or nothing was measured", () => {
    expect(sourceBeside(died)).toBeNull()
    expect(sourceBeside(null)).toBeNull()
  })
})

describe("sourceFaultText", () => {
  it("names the rule and its citation", () => {
    expect(sourceFaultText(source)).toBe("source fault · prack-2xx-or-481 · RFC 3262 §3")
    expect(sourceFaultText({ ...source, rfc: null })).toBe("source fault · prack-2xx-or-481")
  })
})
