/**
 * The queue's chip filter as the page holds it and as the URL carries it: one
 * include per group under the group's own key, the excludes repeated under
 * `not-<group>`. A filter written to a query string and read back is the same
 * filter, so a reload and a pasted link show the same slice; a click cycles a
 * chip none → include → exclude → none.
 */
import { describe, expect, it } from "vitest"
import {
  cycle,
  EMPTY,
  filterParams,
  readFilter,
  select,
  stateOf,
  type ChipFilter
} from "../src/queue-filter.ts"
import { href } from "../src/router.ts"

const filter: ChipFilter = {
  include: { rule: "rport-echo", expected: "", refusal: "", outcome: "refused", feature: "" },
  exclude: {
    rule: [],
    expected: ["200 to INVITE"],
    refusal: ["source-call-incomplete", "source-truncated-at-edge"],
    outcome: [],
    feature: []
  }
}

/** What `setQuery` writes, read back the way the page reads its URL on load. */
const roundTrip = (held: ChipFilter): ChipFilter =>
  readFilter(new URLSearchParams(href.triage({ lane: "fake", ...filterParams(held) }).split("?")[1] ?? ""))

describe("the URL", () => {
  it("carries an include under the group's key and the excludes repeated under not-<group>", () => {
    const query = href.triage({ lane: "fake", ...filterParams(filter) })
    const params = new URLSearchParams(query.split("?")[1])
    expect(params.get("rule")).toBe("rport-echo")
    expect(params.get("outcome")).toBe("refused")
    expect(params.has("expected")).toBe(false)
    expect(params.getAll("not-refusal")).toEqual(["source-call-incomplete", "source-truncated-at-edge"])
    expect(params.getAll("not-expected")).toEqual(["200 to INVITE"])
    expect(params.has("not-rule")).toBe(false)
    expect(params.has("not-outcome")).toBe(false)
  })

  it("reads back what it wrote", () => {
    expect(roundTrip(filter)).toEqual(filter)
    expect(roundTrip(EMPTY)).toEqual(EMPTY)
    expect(href.triage(filterParams(EMPTY))).toBe("/triage")
  })

  it("reads a pasted link: a blank value is no include, a repeated value is one exclude", () => {
    const read = readFilter(new URLSearchParams("refusal=&not-refusal=a&not-refusal=a&not-rule=&outcome=failed"))
    expect(read).toEqual({
      ...EMPTY,
      include: { ...EMPTY.include, outcome: "failed" },
      exclude: { ...EMPTY.exclude, refusal: ["a"] }
    })
  })

  it("drops an include its group also excludes: the exclusion wins", () => {
    const read = readFilter(new URLSearchParams("refusal=a&not-refusal=a&rule=b&not-rule=c"))
    expect(read.include).toEqual({ ...EMPTY.include, rule: "b" })
    expect(read.exclude).toEqual({ ...EMPTY.exclude, refusal: ["a"], rule: ["c"] })
  })
})

describe("a click", () => {
  it("cycles a chip none → include → exclude → none", () => {
    const included = cycle(EMPTY, "refusal", "a")
    expect(stateOf(included, "refusal", "a")).toBe("include")
    expect(included.include.refusal).toBe("a")
    const excluded = cycle(included, "refusal", "a")
    expect(stateOf(excluded, "refusal", "a")).toBe("exclude")
    expect(excluded.include.refusal).toBe("")
    expect(excluded.exclude.refusal).toEqual(["a"])
    const cleared = cycle(excluded, "refusal", "a")
    expect(stateOf(cleared, "refusal", "a")).toBe("none")
    expect(cleared).toEqual(EMPTY)
  })

  it("keeps one include per group and any number of excludes", () => {
    const one = cycle(cycle(EMPTY, "refusal", "a"), "refusal", "b")
    expect(one.include.refusal).toBe("b")
    expect(stateOf(one, "refusal", "a")).toBe("none")
    const many = cycle(cycle(cycle(cycle(EMPTY, "refusal", "a"), "refusal", "a"), "refusal", "b"), "refusal", "b")
    expect(many.exclude.refusal).toEqual(["a", "b"])
    expect(many.include.refusal).toBe("")
    // Clearing one exclude leaves the other.
    expect(cycle(many, "refusal", "a").exclude.refusal).toEqual(["b"])
  })

  it("leaves the other groups alone", () => {
    const next = cycle(filter, "outcome", "refused")
    expect(next.include.rule).toBe("rport-echo")
    expect(next.exclude.refusal).toEqual(filter.exclude.refusal)
    expect(stateOf(next, "outcome", "refused")).toBe("exclude")
  })

  it("on a row's chip SETS the include, and the value leaves the excludes", () => {
    const excluded = cycle(cycle(EMPTY, "rule", "a"), "rule", "a")
    const set = select(excluded, "rule", "a")
    expect(set.include.rule).toBe("a")
    expect(set.exclude.rule).toEqual([])
    // Setting is not toggling: a second set keeps the include.
    expect(select(set, "rule", "a")).toEqual(set)
  })
})
