/**
 * The source fault a call carries: information, shown as a marker of its own
 * beside whatever the row says — the SUT's main error on a failing call, the
 * `passed` tag on a passing one — and never a main error.
 */
import { describe, expect, it } from "vitest"
import type { TriageCallRow } from "../src/api.ts"
import { sourceFaultText, sourceOf } from "../src/source-fault.ts"

const source = { rule: "prack-2xx-or-481", rfc: "RFC 3262 §3", detail: "uas1: answered 481 — RFC 3262 §3" }

const row = (over: Partial<TriageCallRow>): TriageCallRow => ({ kind: "case", ...over }) as TriageCallRow

describe("sourceOf", () => {
  it("reads the source fault of a call, passing or failing", () => {
    expect(sourceOf(row({ passed: true, sourceFault: source }))).toEqual(source)
    expect(sourceOf(row({ passed: false, sourceFault: source }))).toEqual(source)
  })

  it("reads none where the call carries none, or the cut refused it", () => {
    expect(sourceOf(row({ passed: true }))).toBeNull()
    expect(sourceOf(row({ kind: "refused", sourceFault: source }))).toBeNull()
  })
})

describe("sourceFaultText", () => {
  it("names the rule and its citation", () => {
    expect(sourceFaultText(source)).toBe("source fault · prack-2xx-or-481 · RFC 3262 §3")
    expect(sourceFaultText({ ...source, rfc: null })).toBe("source fault · prack-2xx-or-481")
  })
})
