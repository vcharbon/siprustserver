/**
 * The run facts a rule reads: each leg's received, sent and opening datagrams,
 * joined to a probe through the document step it rides.
 */
import type { Classifier } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import { legRunFacts, openedBefore, openerValue, runFactsOf, sentBefore } from "../src/run-facts.js"

const step = (id: string, leg: string): Classifier.DocumentStep =>
  ({ id, leg, op: "send", auto: false, optional: false, inDialog: false })

const DOCUMENT: Classifier.DocumentShape = {
  unackedFinals: [],
  calls: 1,
  steps: [step("s1", "A"), step("s2", "B"), step("s3", "B"), step("s4", "B"), step("s5", "A")],
  attempts: []
}

const ORIGIN = "o=bob 2808844564 2808844564 IN IP4 198.51.100.20"

const RUN: Classifier.RunShape = {
  messages: 9,
  legs: 2,
  legsTornDownBySystem: 0,
  sentRSeqs: new Map([["B", [4711, 4712]]]),
  openers: new Map([["A", [{ name: "f", value: "<sip:+15555550100@example.com>;tag=1" }]]]),
  receptions: new Map([
    ["A", [{ step: "s1", method: "INVITE", cseqMethod: "INVITE", origin: "o=alice 1 1 IN IP4 192.0.2.10" }]],
    ["B", [{ step: "s2", method: "INVITE", cseqMethod: "INVITE", origin: ORIGIN }, { step: "s4", method: "ACK", cseqMethod: "ACK" }]]
  ]),
  sent: new Map([
    ["B", [
      { step: "s3", status: 183, cseqMethod: "INVITE" },
      { status: 100, cseqMethod: "INVITE" },
      { step: "s4", status: 200, cseqMethod: "INVITE" },
      { step: "s9", status: 200, cseqMethod: "BYE" }
    ]]
  ])
}

const FACTS = runFactsOf(RUN, DOCUMENT)

describe("a leg's run facts", () => {
  it("join a step to the leg it rides, with everything the leg's actor received, sent and opened with", () => {
    const b = legRunFacts(FACTS, "s3")!
    expect(b.leg).toBe("B")
    expect(b.received.map((r) => r.step)).toEqual(["s2", "s4"])
    expect(b.sent).toHaveLength(4)
    expect(b.sentRSeqs).toEqual([4711, 4712])
    expect(b.opener).toEqual([])
  })

  it("are empty lists on a leg that recorded nothing, and none for a step the document does not state", () => {
    expect(legRunFacts(FACTS, "s1")).toMatchObject({ leg: "A", sent: [], sentRSeqs: [] })
    expect(legRunFacts(FACTS, "s99")).toBeUndefined()
  })
})

describe("the dialog-creating INVITE", () => {
  it("states a header's value under its canonical name, compact forms read in full", () => {
    expect(openerValue(FACTS, "s5", "from")).toBe("<sip:+15555550100@example.com>;tag=1")
    expect(openerValue(FACTS, "s5", "to")).toBeUndefined()
    expect(openerValue(FACTS, "s2", "from")).toBeUndefined()
  })
})

describe("the description that opens a leg's session", () => {
  it("is the first the leg's actor received, where it is not the step's own", () => {
    expect(openedBefore(FACTS, "s4")?.origin).toBe(ORIGIN)
    expect(openedBefore(FACTS, "s2")).toBeUndefined()
  })
})

describe("what a leg sent before a step", () => {
  it("is every datagram attributed to an earlier step of the document, unattributed and unknown steps left out", () => {
    expect(sentBefore(FACTS, "s4").map((r) => r.status)).toEqual([183])
    expect(sentBefore(FACTS, "s3")).toEqual([])
    expect(sentBefore(FACTS, "s99")).toEqual([])
  })
})
