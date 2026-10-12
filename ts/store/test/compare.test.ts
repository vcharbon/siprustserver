/**
 * What a before/after may be read as: one class per call both sides ran, and
 * the ruling that keeps a code change from being reported as harness noise,
 * and vice versa.
 */
import { describe, expect, it } from "vitest"
import type { Cell, Side } from "../src/compare.js"
import { blessClass, classOf, codeReading, compare, headline, NOWHERE_ELSE, unexplained } from "../src/compare.js"
import type { MainError, Provenance } from "../src/model.js"

const code = (app: string | null, core: string | null): Provenance => [
  { name: "app", sha: "abc", dirty: true, tree: app },
  { name: "core", sha: "def", dirty: false, tree: core }
]

const SAME = code("t1", "u1")

const died = (step: string, expected: string): MainError => ({
  kind: "died",
  rule: "expect-timed-out",
  rfc: null,
  step,
  expected,
  detail: ""
})

const cell = (over: Partial<Cell> & { readonly caseId: string }): Cell => ({
  capture: "cap.pcap.gz",
  runId: 1,
  lane: "fake",
  env: "main-checkout",
  branch: "main",
  laneHash: "lane1",
  prepId: 1,
  bundlePath: "/w/cap/run1",
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  documentHash: "doc1",
  mainError: null,
  deltas: { accepted: 3, knownBug: 0, unlisted: 0, unknown: 0 },
  code: SAME,
  ...over
})

const side = (cells: ReadonlyArray<Cell>): Side => ({
  selector: "@run:1",
  runs: 1,
  sweeps: [],
  cells,
  elsewhere: NOWHERE_ELSE
})

const red = (caseId: string, over: Partial<Cell> = {}): Cell =>
  cell({ caseId, passed: false, verdict: "failed", failedStep: "s6", mainError: died("s6", "200 to INVITE"), ...over })

describe("compare", () => {
  it("puts a flip on an unchanged document and an unchanged tree on the harness", () => {
    const result = compare(side([red("c")]), side([cell({ caseId: "c" })]))

    expect(result.moves.map((move) => [move.caseId, move.class, move.code, move.document])).toEqual([
      ["c", "red→green", "same", "same"]
    ])
    expect(unexplained(result)).toBe(1)
    expect(headline(result)).toContain("that is the harness")
  })

  it("refuses to attribute anything when the two sides ran different trees", () => {
    const result = compare(side([red("c")]), side([cell({ caseId: "c", code: code("t2", "u1") })]))

    expect(result.moves[0]!.code).toBe("different")
    expect(unexplained(result)).toBe(0)
    expect(headline(result)).toContain("DIFFERENT code")
  })

  it("reads a missing fingerprint as unknown, never as the same tree", () => {
    const result = compare(side([red("c", { code: code(null, "u1") })]), side([cell({ caseId: "c" })]))

    expect(result.moves[0]!.code).toBe("unknown")
  })

  it("rules on the code component by component, whatever order each side lists them in", () => {
    const swapped = [...SAME].reverse()
    expect(codeReading(SAME, swapped)).toBe("same")
    expect(codeReading(SAME, [SAME[0]!])).toBe("different")
    expect(codeReading(SAME, [SAME[0]!, { name: "plugin", sha: "def", dirty: false, tree: "u1" }])).toBe("different")
    expect(codeReading(SAME, [])).toBe("unknown")
    expect(codeReading([], [])).toBe("unknown")
  })

  it("splits a flip whose document moved from the harness's population", () => {
    const result = compare(side([red("c")]), side([cell({ caseId: "c", documentHash: "doc2" })]))

    expect(result.moves[0]!.document).toBe("changed")
    expect(result.counts["red→green"]).toEqual({ sameDocument: 0, documentChanged: 1 })
    expect(unexplained(result)).toBe(0)
  })

  it("classes a call whose lane block changed as skipped→ran, neither pass nor failure", () => {
    const result = compare(
      side([cell({ caseId: "c", passed: false, skipped: "blocked:number-unclassified" })]),
      side([cell({ caseId: "c" })])
    )

    expect(result.moves[0]!.class).toBe("skipped→ran")
    expect(result.counts["red→green"]).toEqual({ sameDocument: 0, documentChanged: 0 })
  })

  it("compares only the calls both sides ran and counts the rest per side", () => {
    const result = compare(
      side([cell({ caseId: "gone" }), cell({ caseId: "both" })]),
      side([cell({ caseId: "both" }), cell({ caseId: "new" })])
    )

    expect(result.compared).toBe(1)
    expect(result.onlyBefore).toEqual({ captures: 0, calls: 1, keys: ["cap.pcap.gz/gone"] })
    expect(result.onlyAfter).toEqual({ captures: 0, calls: 1, keys: ["cap.pcap.gz/new"] })
  })

  it("keys a call by capture and case, never by case id alone", () => {
    const result = compare(
      side([cell({ caseId: "x-auto", capture: "a.pcap.gz" })]),
      side([cell({ caseId: "x-auto", capture: "b.pcap.gz" })])
    )

    expect(result.compared).toBe(0)
  })
})

describe("a call's class", () => {
  it("is red→red-moved where a failure moved to another step", () => {
    expect(classOf(red("c"), red("c", { failedStep: "s24", mainError: died("s24", "200 to INVITE") }))).toBe(
      "red→red-moved"
    )
  })

  it("is red→red-moved where the main error changed at the same step", () => {
    expect(classOf(red("c"), red("c", { mainError: died("s6", "180 to INVITE") }))).toBe("red→red-moved")
  })

  it("is unchanged where a failure stayed where it was", () => {
    expect(classOf(red("c"), red("c"))).toBe("unchanged")
  })

  it("is known-bug→clean where a green bought by a known-bug bless no longer needs it", () => {
    const blessed = cell({ caseId: "c", deltas: { accepted: 45, knownBug: 1, unlisted: 0, unknown: 0 } })
    const clean = cell({ caseId: "c", deltas: { accepted: 60, knownBug: 0, unlisted: 0, unknown: 0 } })

    expect(classOf(blessed, clean)).toBe("known-bug→clean")
    expect(classOf(clean, blessed)).toBe("clean→known-bug")
  })

  it("reads the bless class off the confrontation counts, unknown first", () => {
    expect(blessClass(cell({ caseId: "c", deltas: { accepted: 1, knownBug: 1, unlisted: 1, unknown: 1 } }))).toBe(
      "unknown"
    )
    expect(blessClass(cell({ caseId: "c", deltas: { accepted: 1, knownBug: 1, unlisted: 1, unknown: 0 } }))).toBe(
      "unlisted"
    )
    expect(blessClass(cell({ caseId: "c", deltas: null }))).toBe("unmeasured")
  })

  it("is ran→skipped where the lane came to block the call", () => {
    expect(classOf(cell({ caseId: "c" }), cell({ caseId: "c", passed: false, skipped: "blocked:x" }))).toBe(
      "ran→skipped"
    )
  })
})
