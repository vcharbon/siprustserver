/**
 * The URL forms: a ladder comparison keeps `/compare/<side>/<side>`; a run-set
 * comparison is `/compare?a&b`, and two selectors in the path read as it; a
 * report is `/report?run`, and a selector in the path reads as it. Every form
 * a ticket already cites keeps its meaning.
 */
import { describe, expect, it } from "vitest"
import { href, isSelector, parseRoute } from "../src/router.ts"

describe("the compare and report routes", () => {
  it("keeps the ladder alignment of two case or run sides", () => {
    expect(parseRoute("/compare/case:a~b/run:runs~x")).toEqual({
      kind: "compare",
      left: { kind: "case", id: "a~b" },
      right: { kind: "run", id: "runs~x" }
    })
    expect(parseRoute("/compare/simple.v3/other.v3").kind).toBe("compare")
  })

  it("reads two selectors in the path as the run-set comparison", () => {
    expect(parseRoute(`/compare/${encodeURIComponent("@develop")}/${encodeURIComponent("@env:dev-1-x")}`)).toEqual({
      kind: "compare-runs",
      path: { a: "@develop", b: "@env:dev-1-x" }
    })
    expect(parseRoute("/compare/run12/345")).toEqual({ kind: "compare-runs", path: { a: "run12", b: "345" } })
    expect(parseRoute("/compare")).toEqual({ kind: "compare-runs" })
  })

  it("reads a report by query or by path", () => {
    expect(parseRoute("/report")).toEqual({ kind: "report" })
    expect(parseRoute(`/report/${encodeURIComponent("@run:1100000")}`)).toEqual({ kind: "report", run: "@run:1100000" })
    expect(parseRoute("/report/@sweep:4/@lane:fake")).toEqual({ kind: "report", run: "@sweep:4/@lane:fake" })
  })

  it("tells a selector from a ladder side", () => {
    expect(["@develop", "@run:5", "run5", "5", "%40env%3Ax"].map(isSelector)).toEqual([true, true, true, true, true])
    expect(["case:x", "run:runs~a", "simple.v3", "runner"].map(isSelector)).toEqual([false, false, false, false])
  })

  it("writes the canonical forms, keeping every other existing form", () => {
    expect(href.compareRuns({ a: "@develop", b: "@env:x", all: "1" })).toBe("/compare?a=%40develop&b=%40env%3Ax&all=1")
    expect(href.report({ run: "@run:7", lane: "fake" })).toBe("/report?run=%40run%3A7&lane=fake")
    expect(href.triage({ lane: "fake", scope: "red", rule: "r", expected: "e", q: "x" }))
      .toBe("/triage?lane=fake&scope=red&rule=r&expected=e&q=x")
    expect(href.triageStudy("c.pcap", { lane: "fake", case: "c-case1" })).toBe("/triage/c.pcap?lane=fake&case=c-case1")
  })
})
