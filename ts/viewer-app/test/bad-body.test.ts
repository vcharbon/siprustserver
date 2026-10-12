/**
 * The `+ bad body` marker: a call whose main error names another cause while it
 * carries a failing body row says so beside that error; a
 * `bad-body` main error, a call with none, and an unmeasured call do not.
 */
import { describe, expect, it } from "vitest"
import type { TriageMainError } from "../src/api.ts"
import { alsoBadBody } from "../src/bad-body.ts"

const error = (kind: TriageMainError["kind"]): TriageMainError => ({
  kind,
  rule: kind === "bad-body" ? "bad-body" : null,
  rfc: null,
  step: "s9",
  expected: null,
  detail: ""
})

describe("alsoBadBody", () => {
  it("marks another main error on a call carrying a failing body row", () => {
    expect(alsoBadBody(error("died"), 2)).toBe(true)
    expect(alsoBadBody(error("rfc"), 1)).toBe(true)
    expect(alsoBadBody(error("failure"), 1)).toBe(true)
  })

  it("marks nothing where the main error is bad-body, the call carries none, or nothing was measured", () => {
    expect(alsoBadBody(error("bad-body"), 1)).toBe(false)
    expect(alsoBadBody(error("died"), 0)).toBe(false)
    expect(alsoBadBody(error("died"), undefined)).toBe(false)
    expect(alsoBadBody(null, 3)).toBe(false)
  })
})
