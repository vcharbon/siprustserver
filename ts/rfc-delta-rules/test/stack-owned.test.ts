/**
 * The RFC rows: each names the difference its clause makes the stack's own and
 * leaves the same-shape defect visible, and no two of them match one probe.
 */
import { matchingRules, section, WHY_CAP } from "@sip/delta-rules/rule"
import { kindsOf } from "@sip/delta-rules/rule-on"
import { describe, expect, it } from "vitest"
import type { RfcFacts } from "../src/facts.js"
import { RFC_ROWS } from "../src/stack-owned.js"
import { NO_FACTS, RFC_FIXTURES, type Seen } from "./fixtures.js"

const RULES = section<RfcFacts, "rfc">("rfc", RFC_ROWS)

const namers = (seen: Seen): ReadonlyArray<string> => matchingRules(RULES, seen.probe, seen.facts ?? NO_FACTS).map((rule) => rule.id)

describe("the RFC rows", () => {
  it("each have a fixture, one per row", () => {
    expect(RFC_FIXTURES.map((f) => f.id)).toEqual(RFC_ROWS.map((row) => row.id))
  })

  it("each name their own difference, and no other row names it", () => {
    const named = RFC_FIXTURES.flatMap((f) => f.named.map((seen) => ({ id: f.id, by: namers(seen) })))
    expect(named.filter(({ id, by }) => by.length !== 1 || by[0] !== id)).toEqual([])
  })

  it("leave every same-shape defect visible", () => {
    const taken = RFC_FIXTURES.flatMap((f) => f.visible.map((seen) => ({ id: f.id, defect: seen.defect, by: namers(seen) })))
    expect(taken.filter(({ by }) => by.length > 0)).toEqual([])
  })

  it("key on disjoint header families, so no two rows match one probe", () => {
    const families = RFC_ROWS.map((row) => {
      expect(kindsOf(row.on), row.id).toEqual(["header"])
      expect(row.on.family?.every((name) => !name.endsWith("-")), row.id).toBe(true)
      return row.on.family ?? []
    })
    const all = families.flat()
    expect(new Set(all).size).toBe(all.length)
  })

  it("each cite their clause in `rfc` and in `why`, within the why cap, with no section and no lane note", () => {
    const uncited = RFC_ROWS.filter((row) =>
      row.rfc === undefined || !/RFC \d+ §/.test(row.rfc) || !/RFC \d+ §/.test(row.why) || row.why.length > WHY_CAP ||
      row.lanes !== undefined || "section" in row
    )
    expect(uncited.map((row) => row.id)).toEqual([])
  })
})
