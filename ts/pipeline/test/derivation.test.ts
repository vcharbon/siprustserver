/**
 * The Call-ID derivations a cut joins legs by: none by default, and the
 * counter prefix the upstream B2BUA mints a dialled leg's Call-ID with
 * (`<n>-<originator's Call-ID>`).
 */
import { describe, expect, it } from "vitest"
import { counterPrefixed, neverDerives } from "../src/derivation.js"

describe("the Call-ID derivations", () => {
  it("joins nothing by default", () => {
    expect(neverDerives("a@example.com", "1-a@example.com")).toBe(false)
  })

  it("joins a Call-ID prefixed with a counter and a dash to its base, and nothing else ending with it", () => {
    expect(counterPrefixed("a@example.com", "1-a@example.com")).toBe(true)
    expect(counterPrefixed("a@example.com", "12-a@example.com")).toBe(true)
    expect(counterPrefixed("a@example.com", "x1-a@example.com")).toBe(false)
    expect(counterPrefixed("a@example.com", "-a@example.com")).toBe(false)
    expect(counterPrefixed("a@example.com", "1a@example.com")).toBe(false)
  })
})
