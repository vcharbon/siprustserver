/**
 * The overlap graph: the pairs one match set gives, their winner by match
 * order, the lanes where the order decides a class, and the golden form a
 * deployment checks in.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { describe, expect, it } from "vitest"
import type { LanePolicy } from "../src/lane-lists.js"
import { FIXTURE_SOURCES, goldenOf, overlapFile, overlapGraph, restrictedTo, type Tally } from "../src/overlap.js"

const RULES = [
  { id: "a", section: "first" },
  { id: "b", section: "first" },
  { id: "c", section: "second" }
] as const

const graph = overlapGraph<"first" | "second">(RULES)

const lane = (name: string, accepted: ReadonlyArray<string>): LanePolicy => ({
  lane: name,
  accepted: new Map(accepted.map((rule) => [rule, { rule }])),
  knownBug: new Map()
})

describe("the overlap graph", () => {
  it("gives every pair of a match set, the earlier rule winning, ids no rule defines left out", () => {
    expect(graph.pairsOf(["a", "x", "b", "c"])).toEqual([["a", "b"], ["a", "c"], ["b", "c"]])
  })

  it("counts each pair by source and orders the edges by winner then shadowed", () => {
    const tally: Tally = new Map()
    graph.tallyMatches(tally, "archive", ["b", "c"], 3)
    graph.tallyMatches(tally, "fixtures", ["a", "c"])
    graph.tallyMatches(tally, "fixtures", ["b", "c"])
    const edges = graph.edgesOf(tally, [lane("lab", ["a", "c"]), lane("other", ["a", "b", "c"])])
    expect(edges).toEqual([
      { winner: "a", shadowed: "c", sections: ["first", "second"], counts: { fixtures: 1 }, differOn: [] },
      { winner: "b", shadowed: "c", sections: ["first", "second"], counts: { archive: 3, fixtures: 1 }, differOn: ["lab"] }
    ])
  })

  it("keeps the pairs and their sources in its golden form, and restricts it to the fixture sources", () => {
    const tally: Tally = new Map()
    graph.tallyMatches(tally, "archive", ["a", "b"])
    graph.tallyMatches(tally, "fixture-mutants", ["b", "c"])
    graph.tallyMatches(tally, "archive", ["b", "c"])
    const golden = goldenOf(graph.edgesOf(tally, []))
    expect(golden).toEqual([
      { winner: "a", shadowed: "b", sections: ["first", "first"], sources: ["archive"] },
      { winner: "b", shadowed: "c", sections: ["first", "second"], sources: ["archive", "fixture-mutants"] }
    ])
    expect(restrictedTo(golden, FIXTURE_SOURCES)).toEqual([
      { winner: "b", shadowed: "c", sections: ["first", "second"], sources: ["fixture-mutants"] }
    ])
  })

  it("decodes a golden file over the deployment's sections and refuses another section", () => {
    const schema = overlapFile(["first", "second"])
    const pair = { winner: "a", shadowed: "c", sources: ["fixtures"] }
    const decode = (sections: ReadonlyArray<string>) =>
      Effect.runSync(Effect.exit(Schema.decodeUnknownEffect(schema)({ pairs: [{ ...pair, sections }] })))
    expect(decode(["first", "second"])._tag).toBe("Success")
    expect(decode(["first", "third"])._tag).toBe("Failure")
  })
})
