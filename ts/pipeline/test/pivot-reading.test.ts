/**
 * A pivot's steps read as a captured call is read: each leg's role off the
 * document's calls, and each `expect` step's situation and pair.
 */
import type { Flow } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import * as DocumentFacts from "../src/document-facts.js"
import type { CallsView } from "../src/leg-role.js"
import { type PivotView, readSteps, readStepsOf } from "../src/pivot-reading.js"

const step = (id: string, leg: string, op: "send" | "expect", msg: Record<string, unknown>, inDialog = false) => ({
  id,
  leg,
  op,
  msg,
  ...(inDialog ? { in_dialog: true } : {})
})

/** Hand-made steps stand for a pivot: the reading only uses the fields they carry. */
const asPivot = (view: unknown): PivotView => view as PivotView

/** A calls; B is dialled first, C hunted next, D a media resource on another branch. */
const pivot = asPivot({
  calls: [{
    caller_leg: "A",
    attempts: [
      { leg: "C", branch: 0, position: 1 },
      { leg: "B", branch: 0, position: 0 },
      { leg: "D", branch: 1, position: 0, joined_by: { kind: "mrf" } }
    ]
  }],
  flow: [
    step("s1", "A", "send", { method: "INVITE" }),
    step("s2", "B", "expect", { method: "INVITE" }),
    step("s3", "B", "send", { status: 183, "cseq-method": "INVITE" }),
    step("s4", "A", "expect", { status: 183, "cseq-method": "INVITE" }),
    step("s5", "A", "send", { method: "PRACK" }),
    step("s6", "B", "expect", { method: "PRACK" })
  ]
})

describe("pivot steps", () => {
  it("gives the caller, the first hunted attempt, later attempts and joined legs their roles", () => {
    const { roles } = readSteps(pivot)
    expect(roles.get("s1")).toBe("caller")
    expect(roles.get("s2")).toBe("dialled")
    const later = [step("x", "C", "expect", { method: "INVITE" }), step("y", "D", "expect", { method: "INVITE" })]
    const more = readSteps(asPivot({ ...pivot, flow: later }))
    expect(more.roles.get("x")).toBe("reroute")
    expect(more.roles.get("y")).toBe("resource")
  })

  it("pairs each launched INVITE with its own call's caller leg", () => {
    const twoCalls = asPivot({
      calls: [
        { caller_leg: "A", attempts: [{ leg: "B", branch: 0, position: 0 }] },
        { caller_leg: "C", attempts: [{ leg: "D", branch: 0, position: 0 }] }
      ],
      flow: [
        step("s1", "A", "send", { method: "INVITE" }),
        step("s2", "C", "send", { method: "INVITE" }),
        step("s3", "B", "expect", { method: "INVITE" }),
        step("s4", "D", "expect", { method: "INVITE" })
      ]
    })
    const { placed } = readSteps(twoCalls)
    expect(placed.get("s3")).toEqual({ situation: "INV", pair: 0 })
    expect(placed.get("s4")).toEqual({ situation: "INV", pair: 1 })
  })

  it("reads the steps of the document's own reading as it reads the pivot", () => {
    const view = pivot as { readonly flow: ReadonlyArray<Flow.Step>; readonly calls: CallsView }
    const steps = DocumentFacts.ofSteps(view.flow).steps

    expect(steps).toBe(view.flow)
    expect(readStepsOf(steps, view.calls)).toEqual(readSteps(pivot))
  })

  it("places expect steps only, and reads an unflagged PRACK as an early-dialog request", () => {
    const { placed } = readSteps(pivot)
    expect(placed.has("s1")).toBe(false)
    expect(placed.get("s2")).toEqual({ situation: "INV", pair: 0 })
    expect(placed.get("s4")).toEqual({ situation: "P18", pair: 2 })
    expect(placed.get("s6")).toEqual({ situation: "DQ:PRACK", pair: 4 })
  })
})
