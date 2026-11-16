/**
 * The engine: a built rule never matches outside its key, and the first rule
 * in section and position order names a probe several rules match.
 */
import { describe, expect, it } from "vitest"
import { substitution } from "../src/families.js"
import { LANE_NOTE_CAP, matchingRules, matchRule, section, shapeIssues, WHY_CAP } from "../src/rule.js"
import { admits } from "../src/rule-on.js"
import { headerProbe, shape } from "./probes.js"

interface Facts {
  readonly gatedSteps: ReadonlySet<string>
}

const NONE: Facts = { gatedSteps: new Set() }

const bye = { kind: "request" as const, method: "BYE", inDialog: true }

describe("a rule's match key", () => {
  it("keeps a built rule off every probe it refuses, whatever the predicate holds", () => {
    const [viaOnBye] = section<Facts, "first">("first", [{
      id: "via-on-bye",
      on: { kind: "header", family: ["via"], scope: ["BYE"] },
      why: "",
      header: () => true
    }])
    const onBye = (name: string) => headerProbe({ name, scope: bye, captured: ["a"], replayed: ["b"] })
    const onInvite = headerProbe({ name: "Via", scope: { kind: "initial-invite" }, captured: ["a"], replayed: ["b"] })
    expect(viaOnBye!.matches(onBye("Via"), NONE)).toBe(true)
    expect(viaOnBye!.matches(onBye("To"), NONE)).toBe(false)
    expect(viaOnBye!.matches(onInvite, NONE)).toBe(false)
  })

  it("reads a method exactly, as RFC 3261 §7.1 makes it case-sensitive", () => {
    const on = { kind: "header" as const, family: ["reason"], scope: ["CANCEL"] }
    const reasonOn = (method: string) =>
      headerProbe({ name: "Reason", scope: { kind: "request", method, inDialog: true }, captured: ["a"], replayed: ["b"] })
    expect(admits(on, reasonOn("CANCEL"))).toBe(true)
    expect(admits(on, reasonOn("cancel"))).toBe(false)
  })

  it("reads a family entry ending in `-` as a prefix, and a shape by its kind", () => {
    const prefixed = { kind: "header" as const, family: ["x-example-"] }
    expect(admits(prefixed, headerProbe({ name: "X-Example-Zone", captured: ["a"] }))).toBe(true)
    expect(admits(prefixed, headerProbe({ name: "X-Other", captured: ["a"] }))).toBe(false)
    const glare = shape({ shape: "status-substitution", captured: 491, observed: 200 }, {
      kind: "response",
      status: 200,
      cseqMethod: "INVITE"
    })
    expect(admits({ kind: "shape", family: ["status-substitution"] }, glare)).toBe(true)
    expect(admits({ kind: "shape", family: ["retransmission"] }, glare)).toBe(false)
  })
})

describe("first match", () => {
  const rules = [
    ...section<Facts, "first" | "second">("first", [
      { id: "via-replayed", on: { kind: "header", family: ["via"] }, why: "", header: (h) => h.replayed.length > 0 }
    ]),
    ...section<Facts, "first" | "second">("second", [
      { id: "any-header", on: { kind: "header" }, why: "", header: () => true },
      { id: "via-any", on: { kind: "header", family: ["via"] }, why: "", header: () => true }
    ])
  ]

  it("takes the earliest rule by section, then position", () => {
    const via = headerProbe({ name: "Via", captured: ["a"], replayed: ["b"] })
    expect(matchRule(rules, via, NONE)?.id).toBe("via-replayed")
    expect(matchRule(rules, headerProbe({ name: "Via", captured: ["a"] }), NONE)?.id).toBe("any-header")
  })

  it("lists every rule that names a probe, the winner first and the shadowed in order", () => {
    const via = headerProbe({ name: "Via", captured: ["a"], replayed: ["b"] })
    expect(matchingRules(rules, via, NONE).map((r) => r.id)).toEqual(["via-replayed", "any-header", "via-any"])
  })

  it("names no rule for a probe none matches", () => {
    expect(matchRule(rules, shape({ shape: "retransmission", cseqMethod: "INVITE", captured: 2, replayed: 1 }), NONE)).toBeUndefined()
  })
})

describe("a rule list's shape", () => {
  type S = "first" | "second" | "third"
  const SECTIONS: ReadonlyArray<S> = ["first", "second", "third"]
  const row = (id: string, over: { why?: string; lanes?: Record<string, string> } = {}) =>
    ({ id, on: { kind: "header" as const }, why: over.why ?? "", ...(over.lanes ? { lanes: over.lanes } : {}), header: () => true })
  const of = (name: S, ...ids: ReadonlyArray<string>) => section<Facts, S>(name, ids.map((id) => row(id)))

  it("is sound where each section is contiguous, in declared order, and every id minted once", () => {
    expect(shapeIssues([...of("first", "a", "b"), ...of("third", "c")], SECTIONS)).toEqual([])
  })

  it("refuses a section split by another, so the page grouped by section would misstate match order", () => {
    expect(shapeIssues([...of("first", "a"), ...of("second", "b"), ...of("first", "c")], SECTIONS)).toEqual([
      "section first is split",
      "section first runs after second"
    ])
  })

  it("refuses sections out of declared order, and a section not declared", () => {
    expect(shapeIssues([...of("third", "a"), ...of("first", "b")], SECTIONS)).toEqual(["section first runs after third"])
    expect(shapeIssues(section<Facts, string>("fourth", [row("a")]), SECTIONS)).toEqual(["section fourth is not declared"])
  })

  it("refuses an id minted twice, a why over the cap and a lane note over the cap", () => {
    const rules = [
      ...of("first", "a", "a"),
      ...section<Facts, S>("second", [row("long", { why: "x".repeat(WHY_CAP + 1), lanes: { lab: "y".repeat(LANE_NOTE_CAP + 1) } })])
    ]
    expect(shapeIssues(rules, SECTIONS)).toEqual([
      "id a is minted twice",
      `long: why of ${WHY_CAP + 1} characters`,
      `long: lab note of ${LANE_NOTE_CAP + 1} characters`
    ])
  })
})

describe("a gated status substitution", () => {
  const [gated] = section<Facts, "first">("first", [
    substitution<Facts>({ id: "481-past-release", gate: "gatedSteps", why: "" }, { observed: 481, cseq: "BYE" })
  ])
  const at = (step: string) =>
    shape({ shape: "status-substitution", captured: 200, observed: 481 }, { kind: "response", status: 481, cseqMethod: "BYE" }, step)

  it("matches only at a step its gate holds", () => {
    expect(gated!.matches(at("s4"), { gatedSteps: new Set(["s4"]) })).toBe(true)
    expect(gated!.matches(at("s5"), { gatedSteps: new Set(["s4"]) })).toBe(false)
    expect(gated!.gate).toBe("gatedSteps")
  })
})
