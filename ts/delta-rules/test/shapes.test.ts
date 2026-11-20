/**
 * Shape probes read: a stray and who answered it, an extra or missing message,
 * a status substitution against the statuses a row states, the class bucket a
 * final falls in, and the steps a shape probe is held to.
 */
import type { Probe } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import {
  atOrUnattributed,
  autoAnswered,
  finalClassBucket,
  inStatuses,
  message,
  statusSubstitution,
  stray,
  substituted
} from "../src/shapes.js"
import { shape } from "./probes.js"

const asShape = (p: Probe.Probe): Probe.ShapeProbe => p as Probe.ShapeProbe

const answeredBye = (status: number, captured: number | undefined): Probe.ShapeProbe =>
  asShape(shape({ shape: "status-substitution", captured, observed: status }, { kind: "response", status, cseqMethod: "BYE" }))

describe("a stray", () => {
  it("is a request of the method named that the flow never scripted, serviced or extra", () => {
    expect(stray(asShape(shape({ shape: "serviced-stray", method: "OPTIONS", action: "answered" })), "OPTIONS")).toBe(true)
    expect(stray(asShape(shape({ shape: "extra-message", method: "OPTIONS" })), "OPTIONS")).toBe(true)
    expect(stray(asShape(shape({ shape: "extra-message", method: "INFO" })), "OPTIONS")).toBe(false)
    expect(stray(asShape(shape({ shape: "missing-message", method: "OPTIONS" })), "OPTIONS")).toBe(false)
  })

  it("is answered on its own where the scripted endpoint reacted automatically", () => {
    expect(autoAnswered(asShape(shape({ shape: "serviced-stray", method: "PRACK", action: "auto-reacted" })), "PRACK")).toBe(true)
    expect(autoAnswered(asShape(shape({ shape: "serviced-stray", method: "PRACK", action: "answered" })), "PRACK")).toBe(false)
    expect(autoAnswered(asShape(shape({ shape: "extra-message", method: "PRACK" })), "PRACK")).toBe(false)
  })
})

describe("an extra or missing message", () => {
  it("names the message exactly, a response by the interpreter's name for it", () => {
    const missing = asShape(shape({ shape: "missing-message", method: "200-to-CANCEL" }))
    expect(message(missing, "missing-message", "200-to-CANCEL")).toBe(true)
    expect(message(missing, "extra-message", "200-to-CANCEL")).toBe(false)
    expect(message(missing, "missing-message", "CANCEL")).toBe(false)
  })
})

describe("a status substitution", () => {
  it("is read off its shape probe, nothing off another shape", () => {
    expect(statusSubstitution(answeredBye(481, 200))).toEqual({ shape: "status-substitution", captured: 200, observed: 481 })
    expect(statusSubstitution(asShape(shape({ shape: "extra-message", method: "BYE" })))).toBeUndefined()
  })

  it("states a status set as one status, a list, or a class", () => {
    expect(inStatuses(481, 481)).toBe(true)
    expect(inStatuses(487, [481, 487])).toBe(true)
    expect(inStatuses(503, "5xx")).toBe(true)
    expect(inStatuses(603, "5xx")).toBe(false)
    expect(inStatuses(undefined, "2xx")).toBe(false)
  })

  it("matches a row by its observed status, its captured one and its CSeq method where stated", () => {
    expect(substituted(answeredBye(481, 200), { observed: 481 })).toBe(true)
    expect(substituted(answeredBye(481, 200), { observed: "4xx", captured: 200, cseq: "BYE" })).toBe(true)
    expect(substituted(answeredBye(481, 200), { observed: 481, captured: 408 })).toBe(false)
    expect(substituted(answeredBye(481, 200), { observed: 481, cseq: "INVITE" })).toBe(false)
    expect(substituted(answeredBye(481, undefined), { observed: 481, captured: "2xx" })).toBe(false)
  })

  it("matches a row held to the initial INVITE only there", () => {
    const initial = asShape(shape({ shape: "status-substitution", captured: 486, observed: 480 }, { kind: "initial-invite" }))
    expect(substituted(initial, { observed: 480, at: "initial-invite" })).toBe(true)
    expect(substituted(answeredBye(480, 486), { observed: 480, at: "initial-invite" })).toBe(false)
  })
})

describe("a final's class bucket", () => {
  it("puts 2xx and 3xx apart and 4xx with 5xx, nothing outside 200 to 599", () => {
    expect([200, 299, 302, 404, 503, 180, 603].map(finalClassBucket)).toEqual([2, 2, 3, 4, 4, undefined, undefined])
  })
})

describe("the steps a shape probe is held to", () => {
  it("holds a probe at one of the steps, or an unattributed one where any step is given", () => {
    const at = (step: string) => asShape(shape({ shape: "extra-message", method: "BYE" }, undefined, step))
    expect(atOrUnattributed(at("s4"), new Set(["s4"]))).toBe(true)
    expect(atOrUnattributed(at("s5"), new Set(["s4"]))).toBe(false)
    expect(atOrUnattributed(at(""), new Set(["s4"]))).toBe(true)
    expect(atOrUnattributed(at(""), new Set())).toBe(false)
  })
})
