import type { Bundle, Flow, Pivot } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { confront } from "../src/confront.js"
import { directionOf, legPlaces, legRoles } from "../src/leg-role.js"

type Calls = Pivot.PivotV3["calls"]

const attempt = (leg: string, branch: number, position: number, joined?: "refer" | "info" | "mrf") => ({
  leg,
  branch,
  position,
  callee: { identity: `called-${branch}-${position}` },
  ...(joined === undefined ? {} : { joined_by: { kind: joined, step: "s1" } })
})

describe("a leg's role in its call", () => {
  const calls: Calls = [{
    id: "c1",
    caller_leg: "A",
    attempts: [
      attempt("C", 0, 1),
      attempt("B", 0, 0),
      attempt("M", 0, 0, "mrf"),
      attempt("R", 1, 0, "refer"),
      attempt("I", 2, 0, "info"),
      attempt("F", 3, 0)
    ]
  }] as unknown as Calls

  it("names the caller, the first attempt of the serial hunt dialled, every later one a reroute, a parallel fork, and each join by its kind", () => {
    expect(Object.fromEntries(legRoles(calls))).toEqual({
      A: "caller",
      B: "dialled",
      C: "reroute",
      F: "fork",
      M: "resource",
      R: "transferee-refer",
      I: "transferee-info"
    })
  })

  it("travels toward the caller on the caller's leg and toward the callee on every other named leg", () => {
    expect(directionOf("caller")).toBe("to-caller")
    expect(directionOf("dialled")).toBe("to-callee")
    expect(directionOf("resource")).toBe("to-callee")
    expect(directionOf("other")).toBe("other")
  })

  it("places a leg no call names as other, both ways", () => {
    expect(legPlaces(calls)("Z")).toEqual({ role: "other", direction: "other" })
    expect(legPlaces(calls)("C")).toEqual({ role: "reroute", direction: "to-callee" })
  })
})

describe("a confronted probe", () => {
  const crlf = (lines: ReadonlyArray<string>): string => `${lines.join("\r\n")}\r\n\r\n`
  const step = (id: string, leg: string): Flow.Step => ({
    id,
    leg,
    op: "expect",
    msg: { method: "BYE", headers: [], "headers-present": [] },
    delay: { ms: 0, from: "trigger" as Flow.Delay["from"], compressible: true, timer_linked: false },
    observed: { leg: 0, msg: 0, at_us: 0 }
  } as Flow.Step)
  const pivot: Pivot.PivotV3 = {
    pivot_version: 3,
    case: { id: "c", title: "t", family: "f", variant: "repro", origin: "capture", lanes: {} },
    identities: [],
    calls: [{ id: "c1", caller_leg: "A", attempts: [attempt("B", 0, 0)] }],
    endpoints: [],
    actors: [],
    legs: [],
    flow: [step("s1", "A"), step("s2", "B")],
    timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
  } as unknown as Pivot.PivotV3
  const captured = crlf(["BYE sip:x@h SIP/2.0", "To: <sip:a@h>;tag=t", "CSeq: 2 BYE", "Reason: Q.850;cause=16"])
  const replayed = crlf(["BYE sip:x@h SIP/2.0", "To: <sip:a@h>;tag=t", "CSeq: 2 BYE"])
  const recordings = new Map([
    ["A", [{ seq: 1, dir: "in", at_us: 1000, step: "s1", raw: replayed }]],
    ["B", [{ seq: 1, dir: "in", at_us: 1200, step: "s2", raw: replayed }]]
  ]) as unknown as ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
  const verdict = { case: "c", lane: "fake", status: "passed", failures: [] } as unknown as Bundle.RunVerdict

  it("carries the role of the leg it was observed on and the way the message travelled, beside the document's call count", () => {
    const { probes, context } = confront({
      pivot,
      verdict,
      recordings,
      captured: new Map([[0, { msgs: [{ raw: captured }] }]]) as never
    })
    expect(context.document.calls).toBe(1)
    const places = probes.map((p) => [p.step, p.probe.leg])
    expect(places).toEqual([
      ["s1", { role: "caller", direction: "to-caller" }],
      ["s2", { role: "dialled", direction: "to-callee" }]
    ])
  })
})
