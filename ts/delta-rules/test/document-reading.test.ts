/**
 * A cell's document as a gate reads it: its call facts read off the
 * classifier's steps, each step found by id, and the entry predicates a gate
 * is written with.
 */
import type { Classifier } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import {
  after,
  documentReadingOf,
  idsOf,
  isFinalTo,
  isRequestOf,
  isResponseTo,
  isSuccessTo,
  releaseOf
} from "../src/document-reading.js"

const step = (id: string, leg: string, op: "send" | "expect", over: Partial<Classifier.DocumentStep>): Classifier.DocumentStep =>
  ({ id, leg, op, auto: false, optional: false, inDialog: false, ...over })

/** Alice's call to Bob through the system, Bob hanging up: leg A faces Alice, leg B faces Bob. */
const STEPS: ReadonlyArray<Classifier.DocumentStep> = [
  step("s1", "A", "send", { method: "INVITE", sdp: true }),
  step("s2", "B", "expect", { method: "INVITE", sdp: true }),
  step("s3", "B", "send", { status: 180, cseqMethod: "INVITE" }),
  step("s4", "B", "send", { status: 200, cseqMethod: "INVITE", sdp: true }),
  step("s5", "A", "expect", { status: 200, cseqMethod: "INVITE", sdp: true }),
  step("s6", "B", "expect", { method: "ACK", inDialog: true }),
  step("s7", "B", "send", { method: "BYE", inDialog: true }),
  step("s8", "B", "expect", { status: 200, cseqMethod: "BYE", inDialog: true }),
  step("s9", "A", "expect", { method: "BYE", inDialog: true })
]

const READING = documentReadingOf({ unackedFinals: [], calls: 1, steps: STEPS, attempts: [] })

const entry = (id: string) => READING.facts.entryOf(id)!

describe("a document's reading", () => {
  it("states each leg's message steps in document order, the system's own told from the party's", () => {
    const b = READING.facts.legs.get("B")!
    expect(b.entries.map((e) => [e.step.id, e.bySystem])).toEqual([
      ["s2", true],
      ["s3", false],
      ["s4", false],
      ["s6", true],
      ["s7", false],
      ["s8", true]
    ])
  })

  it("finds the classifier's step by id, with what it states of a body", () => {
    expect(READING.stepOf("s4")?.sdp).toBe(true)
    expect(READING.stepOf("s3")?.sdp).toBeUndefined()
    expect(READING.stepOf("s99")).toBeUndefined()
  })

  it("reads each entry's method as its request's own or its response's CSeq method", () => {
    expect([entry("s2").method, entry("s4").method, entry("s8").method]).toEqual(["INVITE", "INVITE", "BYE"])
  })
})

describe("the entry predicates", () => {
  it("tell a request of a method from a response to that method's transaction", () => {
    expect(isRequestOf(entry("s7"), "BYE")).toBe(true)
    expect(isRequestOf(entry("s8"), "BYE")).toBe(false)
    expect(isResponseTo(entry("s8"), "BYE")).toBe(true)
    expect(isResponseTo(entry("s7"), "BYE")).toBe(false)
  })

  it("read a final as 200 or above and a success as a 2xx", () => {
    expect(isFinalTo(entry("s3"), "INVITE")).toBe(false)
    expect(isFinalTo(entry("s4"), "INVITE")).toBe(true)
    expect(isSuccessTo(entry("s4"), "INVITE")).toBe(true)
    expect(isSuccessTo(entry("s8"), "INVITE")).toBe(false)
  })

  it("name the ids of entries", () => {
    expect([...idsOf([entry("s1"), entry("s9")])]).toEqual(["s1", "s9"])
  })
})

describe("a leg's release", () => {
  it("is its first BYE, whichever side sent it, and the entries after it follow in order", () => {
    const b = READING.facts.legs.get("B")!
    const release = releaseOf(b)!
    expect(release.step.id).toBe("s7")
    expect(after(b, release).map((e) => e.step.id)).toEqual(["s8"])
    expect(releaseOf(READING.facts.legs.get("A")!)?.step.id).toBe("s9")
  })

  it("is none on a leg that states no BYE", () => {
    const reading = documentReadingOf({ unackedFinals: [], calls: 1, steps: STEPS.slice(0, 5), attempts: [] })
    expect(releaseOf(reading.facts.legs.get("B")!)).toBeUndefined()
  })
})
