/**
 * The generic rows' overlap golden file (`overlap.golden.json`, ADR-0062
 * item 6): every pair of rows seen naming one probe of the fixtures or of their
 * one-step mutants, with its winner, the earlier row in this package's order.
 * An edit that moves a winner, or makes two rows co-match where none did,
 * fails here naming the pair; a change meant to move one rewrites the file
 * (`vitest run -u`) and shows the move in its diff.
 */
import { mutantsOf } from "@sip/delta-rules/mutants"
import { goldenOf, overlapGraph, type Tally } from "@sip/delta-rules/overlap"
import { matchingRules } from "@sip/delta-rules/rule"
import { describe, expect, it } from "vitest"
import { RFC_RULES } from "../src/sections.js"
import { NO_FACTS, RFC_FIXTURES } from "./fixtures.js"

const graph = overlapGraph(RFC_RULES)

const tally: Tally = new Map()
for (const fixture of RFC_FIXTURES) {
  for (const seen of [...fixture.named, ...fixture.visible]) {
    const facts = seen.facts ?? NO_FACTS
    graph.tallyMatches(tally, "fixtures", matchingRules(RFC_RULES, seen.probe, facts).map((rule) => rule.id))
    for (const mutant of mutantsOf(seen.probe, facts)) {
      graph.tallyMatches(tally, "fixture-mutants", matchingRules(RFC_RULES, mutant.probe, mutant.facts).map((rule) => rule.id))
    }
  }
}

const pairs = goldenOf(graph.edgesOf(tally, []))

describe("the overlap golden file", () => {
  it("holds every pair of rows the fixtures and their mutants show naming one probe, with its winner and sections", async () => {
    const text = `{"pairs": [\n${pairs.map((pair) => `  ${JSON.stringify(pair)}`).join(",\n")}\n]}\n`
    await expect(text).toMatchFileSnapshot("./overlap.golden.json")
  })

  it("names every winner earlier in the rows' order than the row it shadows", () => {
    const order = new Map(RFC_RULES.map((rule, i) => [rule.id, i]))
    expect(pairs.filter((pair) => order.get(pair.winner)! >= order.get(pair.shadowed)!)).toEqual([])
  })
})
