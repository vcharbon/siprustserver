/**
 * The generic rows in their order: each rules the probes its fixture names
 * (first match, no earlier row taking them) and leaves its same-shape probes
 * visible; the sections hold the engine's shape; every row cites its clause or
 * ADR and speaks of the capture and the replay, never in a deployment's voice.
 */
import { matchRule, shapeIssues, WHY_CAP } from "@sip/delta-rules/rule"
import { describe, expect, it } from "vitest"
import { RFC_ROWS, RFC_RULES, RFC_SECTIONS } from "../src/sections.js"
import { NO_FACTS, RFC_FIXTURES, type Seen } from "./fixtures.js"

const firstOf = (seen: Seen): string | undefined => matchRule(RFC_RULES, seen.probe, seen.facts ?? NO_FACTS)?.id

const byId = new Map(RFC_RULES.map((rule) => [rule.id, rule]))

/** A clause of an RFC, or the ADR stating this repository's own behaviour. */
const CITE = /RFC \d+ §|ADR-\d{4}/

/** First-person or deployment words: a row states what the capture and the replay show. */
const VOICE = /\b(we|us|our|ours|prod)\b/i

describe("the generic rows", () => {
  it("each have a fixture, one per row, in row order", () => {
    expect(RFC_FIXTURES.map((f) => f.id)).toEqual(RFC_ROWS.map((row) => row.id))
  })

  it("each rule the probes their fixture names: no earlier row takes one", () => {
    const named = RFC_FIXTURES.flatMap((f) => f.named.map((seen) => ({ id: f.id, first: firstOf(seen) })))
    expect(named.filter(({ id, first }) => first !== id)).toEqual([])
  })

  it("each leave the same-shape probes their fixture lists visible", () => {
    const taken = RFC_FIXTURES.flatMap((f) =>
      f.visible.filter((seen) => byId.get(f.id)?.matches(seen.probe, seen.facts ?? NO_FACTS)).map((seen) => `${f.id}: ${seen.defect}`)
    )
    expect(taken).toEqual([])
  })

  it("each leave a visible probe to the row it states, or to none: a widened row cannot swallow it", () => {
    const left = RFC_FIXTURES.flatMap((f) => f.visible.map((seen) => `${f.id}: ${seen.defect} -> ${String(firstOf(seen))}`))
    const stated = RFC_FIXTURES.flatMap((f) => f.visible.map((seen) => `${f.id}: ${seen.defect} -> ${String(seen.leftTo)}`))
    expect(left).toEqual(stated)
  })

  it("sit in sections that hold the engine's shape: each declared, contiguous and in order, ids minted once, whys within the cap", () => {
    expect(shapeIssues(RFC_RULES, RFC_SECTIONS.map((s) => s.name))).toEqual([])
  })

  it("each cite their clause or ADR in `rfc` and in `why`, within the why cap, with no section and no lane note", () => {
    const uncited = RFC_ROWS.filter((row) =>
      row.rfc === undefined || !CITE.test(row.rfc) || !CITE.test(row.why) || row.why.length > WHY_CAP ||
      row.lanes !== undefined || "section" in row
    )
    expect(uncited.map((row) => row.id)).toEqual([])
  })

  it("each word their ruling as what the capture and the replay show", () => {
    expect(RFC_ROWS.filter((row) => VOICE.test(row.why)).map((row) => row.id)).toEqual([])
  })
})
