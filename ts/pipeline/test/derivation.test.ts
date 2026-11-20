/**
 * The Call-ID derivations a cut joins legs by: none by default, and the
 * counter prefix the upstream B2BUA mints a dialled leg's Call-ID with
 * (`<n>-<originator's Call-ID>`), and the suffix form under a length floor.
 */
import { describe, expect, it } from "vitest"
import { counterPrefixed, neverDerives, suffixed } from "../src/derivation.js"

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

  it("joins any longer Call-ID ending with a base at or over the floor", () => {
    const derives = suffixed(8)
    expect(derives("a@example.com", "x1-a@example.com")).toBe(true)
    expect(derives("a@example.com", "a@example.com")).toBe(false)
    expect(derives("1234567", "x-1234567")).toBe(false)
    expect(derives("12345678", "x-12345678")).toBe(true)
  })
})
