/**
 * The minimal case form: each shape's ladder as the wire carries it, the
 * parties' addresses on the INVITE, the final checks on the caller's final,
 * the claims beside the document, a document that decodes as pivot v3, and
 * each refusal of a case that does not hold.
 */
import { type Flow, Pivot } from "@sip/contracts"
import { CaseSource, type CompiledCell } from "@sip/rig"
import * as Effect from "effect/Effect"
import { fileURLToPath } from "node:url"
import { describe, expect, it } from "vitest"
import { CallFlow, defineBasicCase } from "../src/index.js"

const SOURCE = fileURLToPath(new URL("./fixtures/cases.ts", import.meta.url))

/** Each step as `<leg> <op> <method or status>`, `auto` steps marked. */
const ladderOf = (cell: CompiledCell.CompiledCell): ReadonlyArray<string> =>
  Pivot.pivotSteps(cell.document).map((step) =>
    `${step.leg} ${step.op} ${step.msg.method ?? `${step.msg.status} ${step.msg["cseq-method"]}`}${step.auto === true ? " (auto)" : ""}`
  )

const stepOf = (cell: CompiledCell.CompiledCell, ladder: string): Flow.Step => {
  const at = ladderOf(cell).indexOf(ladder)
  return Pivot.pivotSteps(cell.document)[at]!
}

const PARTIES = { caller: "sip:alice@atlanta.example.com", callee: "sip:bob@biloxi.example.com" } as const

describe("the fixture source", () => {
  it("yields alice calls bob and bob is busy as cells that decode, each with its claims", async () => {
    const cells = await Effect.runPromise(CaseSource.load(SOURCE))
    expect(cells.map((cell) => [cell.id, cell.case, cell.claims])).toEqual([
      ["alice-calls-bob", "alice calls bob", { "call-records": { count: 1 } }],
      ["bob-is-busy", "bob is busy", { "call-records": { count: 1 } }]
    ])
    for (const cell of cells) expect(Pivot.decodePivotSync(JSON.parse(Pivot.emitPivot(cell.document)))).toEqual(cell.document)
  })
})

describe("basic_call", () => {
  const cell = defineBasicCase({ title: "alice calls bob", shape: "basic_call", ...PARTIES, final: [{ field: "status", eq: 200 }] })

  it("rings, answers, ACKs on both legs, and the caller hangs up", () => {
    expect(ladderOf(cell)).toEqual([
      "A send INVITE",
      "A expect 100 INVITE (auto)",
      "B expect INVITE",
      "B send 180 INVITE",
      "A expect 180 INVITE",
      "B send 200 INVITE",
      "A expect 200 INVITE",
      "A send ACK (auto)",
      "B expect ACK (auto)",
      "A send BYE",
      "B expect BYE",
      "B send 200 BYE",
      "A expect 200 BYE"
    ])
  })

  it("addresses the INVITE to the callee from the caller, its offer in the body, and the answer on the 200", () => {
    const invite = stepOf(cell, "A send INVITE")
    expect([invite.msg.ruri, invite.msg.from, invite.msg.to]).toEqual([
      { frozen: PARTIES.callee },
      { frozen: PARTIES.caller },
      { frozen: PARTIES.callee }
    ])
    expect(invite.msg.body).toEqual({ ref: "resources/offer.sdp", rewrite: ["c=addr", "m=port"] })
    expect(stepOf(cell, "B send 200 INVITE").msg.body).toEqual({ ref: "resources/answer.sdp", rewrite: ["c=addr", "m=port"] })
    expect(Object.keys(cell.resources).sort()).toEqual(["resources/answer.sdp", "resources/offer.sdp"])
  })

  it("asserts the final checks on the caller's 200, marks the dialog from the 200 on and the ACK as confirming it", () => {
    expect(stepOf(cell, "A expect 200 INVITE").checks).toEqual([{ field: "status", op: "eq", value: "200" }])
    const marks = Pivot.pivotSteps(cell.document).map((step) => [step.in_dialog === true, step.confirms_dialog === true])
    expect(marks.slice(6)).toEqual([
      [false, false],
      [true, true],
      [true, true],
      [true, false],
      [true, false],
      [true, false],
      [true, false]
    ])
  })

  it("lets the callee hang up where its parameters say so", () => {
    const ended = defineBasicCase({ title: "bob hangs up", shape: "basic_call", params: { ends: "callee" }, ...PARTIES, final: [] })
    expect(ladderOf(ended).slice(-4)).toEqual(["B send BYE", "A expect BYE", "A send 200 BYE", "B expect 200 BYE"])
  })
})

describe("the step builder", () => {
  it("states the early dialog a step rides, and marks the leg's dialog from the 2xx answering on one", () => {
    const flow = new CallFlow()
    flow.send("B", { status: 183, "cseq-method": "INVITE" }, { early: "f1" })
    flow.expect("A", { status: 183, "cseq-method": "INVITE" }, { early: "r1" })
    flow.send("B", { status: 200, "cseq-method": "INVITE" }, { early: "f1" })
    flow.expect("B", { method: "ACK", cseq: 1 }, { early: "f1", auto: true })
    flow.send("A", { method: "BYE" })
    expect(flow.steps.map((step) => [step.leg, step.early ?? null, step.in_dialog === true, step.confirms_dialog === true]))
      .toEqual([
        ["B", "f1", false, false],
        ["A", "r1", false, false],
        ["B", "f1", false, false],
        ["B", "f1", true, true],
        ["A", null, false, false]
      ])
  })
})

describe("invite_reject", () => {
  it("rejects the INVITE 486 by default, the system ACKing it on B and the caller ACKing its final", () => {
    const cell = defineBasicCase({ title: "bob is busy", shape: "invite_reject", ...PARTIES, final: [{ field: "status", eq: 486 }] })
    expect(ladderOf(cell)).toEqual([
      "A send INVITE",
      "A expect 100 INVITE (auto)",
      "B expect INVITE",
      "B send 486 INVITE",
      "B expect ACK (auto)",
      "A expect 486 INVITE",
      "A send ACK (auto)"
    ])
    expect(stepOf(cell, "B send 486 INVITE").msg.reason).toBe("Busy Here")
    expect(stepOf(cell, "A expect 486 INVITE").checks).toEqual([{ field: "status", op: "eq", value: "486" }])
    expect(Pivot.pivotSteps(cell.document).some((step) => step.in_dialog === true || step.confirms_dialog === true)).toBe(false)
  })

  it("plays the status its parameters state, with its RFC 3261 reason phrase", () => {
    const cell = defineBasicCase({ title: "bob declines", shape: "invite_reject", params: { status: 603 }, ...PARTIES, final: [] })
    expect(stepOf(cell, "B send 603 INVITE").msg.reason).toBe("Decline")
  })
})

describe("a case that does not hold", () => {
  const refusal = (input: unknown): string => {
    try {
      defineBasicCase(input as Parameters<typeof defineBasicCase>[0])
    } catch (error) {
      return error instanceof Error ? error.message : String(error)
    }
    return "accepted"
  }
  const base = { title: "t", shape: "basic_call", ...PARTIES, final: [] }

  it("is refused for a key the form does not name", () => {
    expect(refusal({ ...base, lanes: {} })).toContain("basic case \"t\":")
  })

  it("is refused for an address that is no SIP URI naming a user", () => {
    expect(refusal({ ...base, callee: "tel:+15550100" })).toContain("a sip: or sips: URI naming a user at a host")
    expect(refusal({ ...base, caller: "sip:atlanta.example.com" })).toContain("a sip: or sips: URI naming a user at a host")
  })

  it("is refused for a reject status that needs a header beside it, naming the parameter", () => {
    expect(refusal({ ...base, shape: "invite_reject", params: { status: 401 } })).toContain(`at ["params"]["status"]`)
  })

  it("is refused for a reject status a proxy relays as another (503 as 500)", () => {
    expect(refusal({ ...base, shape: "invite_reject", params: { status: 503 } })).toContain(`at ["params"]["status"]`)
  })

  it("is refused for a final status its shape does not end with", () => {
    expect(refusal({ ...base, final: [{ field: "status", eq: 486 }] })).toContain(
      "final: expects status 486, and basic_call ends the INVITE with 200"
    )
  })

  it("is refused for a title naming no cell", () => {
    expect(refusal({ ...base, title: "!!" })).toContain("its title holds no letter or digit to name its cell by")
  })

  it("is accepted otherwise, its cell named by its title", () => {
    expect(defineBasicCase({ title: "Alice  calls Bob!", shape: "basic_call", ...PARTIES, final: [] }).id).toBe("alice-calls-bob")
  })
})
