/**
 * The selector grammar: every form a command names runs by, and
 * the refusal of anything else, naming the forms.
 */
import { describe, expect, it } from "vitest"
import * as Selector from "../src/selector.js"

const terms = (text: string) => {
  const parsed = Selector.parse(text)
  return Selector.isRefused(parsed) ? parsed.refused : parsed.terms
}

describe("Selector.parse", () => {
  it("reads every term", () => {
    expect(terms("@current")).toEqual([{ _tag: "current" }])
    expect(terms("@env:dev-519-x")).toEqual([{ _tag: "env", env: "dev-519-x" }])
    expect(terms("@run:12")).toEqual([{ _tag: "run", id: 12 }])
    expect(terms("12")).toEqual([{ _tag: "run", id: 12 }])
    expect(terms("run12")).toEqual([{ _tag: "run", id: 12 }])
    expect(terms("@sha:AB12CD3")).toEqual([{ _tag: "sha", prefix: "ab12cd3" }])
    expect(terms("@lane:fake")).toEqual([{ _tag: "lane", lane: "fake" }])
    expect(terms("@sweep:7")).toEqual([{ _tag: "sweep", id: 7 }])
    expect(terms("@job:42")).toEqual([{ _tag: "job", id: 42 }])
  })

  it("reads a sweep and a job by their bare kind-and-number forms too", () => {
    expect(terms("sweep7")).toEqual([{ _tag: "sweep", id: 7 }])
    expect(terms("job42")).toEqual([{ _tag: "job", id: 42 }])
    expect(Selector.FORMS).toContain("@sweep:N (or sweepN)")
    expect(Selector.FORMS).toContain("@job:N (or jobN)")
  })

  it("joins terms with /, every one of which a run meets", () => {
    expect(terms("@env:dev-519/@lane:fake")).toEqual([
      { _tag: "env", env: "dev-519" },
      { _tag: "lane", lane: "fake" }
    ])
  })

  it("refuses what is no term, naming the forms", () => {
    expect(terms("current")).toContain(Selector.FORMS)
    expect(terms("@sha:xyz")).toContain("is no sha")
    expect(terms("@sha:ab12cd")).toContain("at least 7 hex digits")
    expect(terms("@run:x")).toContain("is no run id")
    expect(terms("")).toContain(Selector.FORMS)
  })

  it("reads the current term under the deployment's own spelling, and refuses the default one there", () => {
    const spelling: Selector.Spelling = { current: "@main" }
    const parsed = Selector.parse("@main/@lane:fake", spelling)
    expect(Selector.isRefused(parsed) ? parsed.refused : parsed.terms).toEqual([
      { _tag: "current" },
      { _tag: "lane", lane: "fake" }
    ])
    const refused = Selector.parse("@current", spelling)
    expect(Selector.isRefused(refused) ? refused.refused : "").toContain(Selector.formsOf(spelling))
    expect(Selector.formsOf(spelling).startsWith("@main, ")).toBe(true)
  })

  it("narrows a selector to a lane unless it pins one already", () => {
    const current = Selector.parse("@current") as Selector.Selector
    expect(Selector.onLane(current, "fake").text).toBe("@current/@lane:fake")
    const run = Selector.parse("run3") as Selector.Selector
    expect(Selector.onLane(run, "fake")).toBe(run)
  })
})
