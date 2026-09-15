/**
 * Stamping the allowed-errors registry onto a case: which step carries the
 * violating message at THIS vantage, and who the rule charges for it.
 */
import type { AllowedErrors, Flows } from "@sip/contracts"
import { Violation } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { caseCallIds, correlate } from "../src/cut.js"
import { synthesize } from "../src/flowsteps.js"
import type { Vantage } from "../src/selection.js"
import { build } from "../src/topology.js"
import { stampRfcViolations, unanchoredFlag } from "../src/violations.js"
import {
  CALLEE_CALL_ID,
  CALLER_CALL_ID,
  cancelRaceFlows,
  derivesOnePrefix,
  plan,
  RELIABLE_RSEQ,
  SOCKETS,
  sutAnswersAfterCancelFlows,
  secondAnswerFlows,
  sutSet,
  unackedProvisionalFlows
} from "./fixtures.js"

const CAPTURE = "race.pcap"

/** The status the step with this id expects — what a stamped anchor names. */
const statusOfStep = (flows: Flows.FlowsDoc, id: string): number | undefined => {
  const sut = sutSet()
  const layout = build(flows, [{ leg: 0, hop: 0 }], sut, plan(), derivesOnePrefix)
  const flow = synthesize(flows, layout, plan())
  return flow.steps.find((s) => s.id === id)?.msg.status
}

const registryOf = (
  ...violations: ReadonlyArray<AllowedErrors.AllowedViolation>
): AllowedErrors.AllowedErrors => ({ captures: { [CAPTURE]: violations } })

const stamp = (
  flows: Flows.FlowsDoc,
  vantages: ReadonlyArray<Vantage>,
  registry: AllowedErrors.AllowedErrors | undefined
) => {
  const sut = sutSet()
  const layout = build(flows, vantages, sut, plan(), derivesOnePrefix)
  const flow = synthesize(flows, layout, plan())
  const legs = [...new Set(vantages.map((v) => v.leg))]
  return stampRfcViolations({
    registry,
    capture: CAPTURE,
    flows,
    callIds: caseCallIds(flows, sut, legs, correlate(flows, derivesOnePrefix)),
    sources: flow.sources,
    steps: flow.steps,
    legs: layout.legs
  })
}

describe("a registry nothing lists", () => {
  it("stamps nothing and warns about nothing", () => {
    const out = stamp(cancelRaceFlows(), [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }], undefined)
    expect(out).toEqual({ violations: [], warnings: [] })
  })
})

describe("no-200-after-cancel", () => {
  const entry: AllowedErrors.AllowedViolation = {
    rule: "no-200-after-cancel",
    originator: SOCKETS.callee,
    call_ids: [CALLEE_CALL_ID],
    note: "the far callee answered after taking the CANCEL"
  }

  it("anchors on the 2xx the originator SENT, and charges the actor that sends it", () => {
    const out = stamp(cancelRaceFlows(), [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }], registryOf(entry))
    expect(out.warnings).toEqual([])
    expect(out.violations).toHaveLength(1)
    const violation = out.violations[0]!
    expect(violation.rule).toBe("no-200-after-cancel")
    expect(violation.emitter).not.toBe(Violation.SUT_EMITTER)
    expect(Violation.violationSutEmitted(violation)).toBe(false)
  })

  it("charges the SUT where the platform is the party that answered", () => {
    const sutEntry: AllowedErrors.AllowedViolation = {
      rule: "no-200-after-cancel",
      originator: SOCKETS.sut,
      call_ids: [CALLER_CALL_ID],
      note: "the platform answered after taking the CANCEL"
    }
    const out = stamp(sutAnswersAfterCancelFlows(), [{ leg: 0, hop: 0 }], registryOf(sutEntry))
    expect(out.warnings).toEqual([])
    expect(out.violations).toHaveLength(1)
    expect(Violation.violationSutEmitted(out.violations[0]!)).toBe(true)
  })

  it("ignores an entry naming a call this case was not cut from", () => {
    const foreign: AllowedErrors.AllowedViolation = { ...entry, call_ids: ["some-other-call-id"] }
    const out = stamp(cancelRaceFlows(), [{ leg: 0, hop: 0 }], registryOf(foreign))
    expect(out).toEqual({ violations: [], warnings: [] })
  })

  it("WARNS on an entry the case matches but no step at this vantage carries", () => {
    // Cut from the caller alone. The violating 200 crossed the b-leg, which this
    // case has no vantage on — so the entry matches the call and anchors on
    // nothing, and staying silent would read as an absent violation.
    const out = stamp(cancelRaceFlows(), [{ leg: 0, hop: 0 }], registryOf(entry))
    expect(out.violations).toEqual([])
    expect(out.warnings).toHaveLength(1)
    expect(out.warnings[0]).toContain("is NOT stamped")
    expect(out.warnings[0]).toContain(CALLEE_CALL_ID)
    expect(unanchoredFlag(out.warnings[0]!).kind).toBe("rfc-violation-unanchored")
  })
})

describe("second-answer-repeats-the-first", () => {
  const entry: AllowedErrors.AllowedViolation = {
    rule: "second-answer-repeats-the-first",
    originator: SOCKETS.sut,
    call_ids: [CALLER_CALL_ID],
    note: "the platform answered the dialog twice, with two transport plans"
  }

  it("anchors on the SECOND binding answer, never the first the peer acts on", () => {
    const out = stamp(secondAnswerFlows(true), [{ leg: 0, hop: 0 }], registryOf(entry))
    expect(out.warnings).toEqual([])
    expect(out.violations).toHaveLength(1)
    const violation = out.violations[0]!
    expect(violation.rule).toBe("second-answer-repeats-the-first")
    expect(Violation.violationSutEmitted(violation)).toBe(true)
    // The 200 is the anchor: the reliable 183 before it is the answer itself.
    expect(statusOfStep(secondAnswerFlows(true), violation.step)).toBe(200)
  })

  it("WARNS where the first description was early media, which binds nothing", () => {
    const out = stamp(secondAnswerFlows(false), [{ leg: 0, hop: 0 }], registryOf(entry))
    expect(out.violations).toEqual([])
    expect(out.warnings).toHaveLength(1)
    expect(out.warnings[0]).toContain("that second answer")
  })
})

describe("unacked-reliable-provisional", () => {
  const entry: AllowedErrors.AllowedViolation = {
    rule: "unacked-reliable-provisional",
    originator: SOCKETS.caller,
    call_ids: [CALLER_CALL_ID],
    note: "the caller never PRACKed the reliable 183"
  }

  it("anchors on the reliable provisional the originator TOOK", () => {
    const out = stamp(unackedProvisionalFlows(), [{ leg: 0, hop: 0 }], registryOf(entry))
    expect(out.warnings).toEqual([])
    expect(out.violations).toHaveLength(1)
    // The rule charges whoever TOOK the provisional and owed the PRACK. Here
    // that is the caller, which this case plays, so the platform is not charged.
    expect(Violation.violationSutEmitted(out.violations[0]!)).toBe(false)
  })

  it("does not anchor on a provisional that carries RSeq without Require: 100rel", () => {
    // RFC 3262 §3 makes reliability the marker PAIR. A numbered provisional
    // nobody asked to be acknowledged breaks no rule.
    const flows = unackedProvisionalFlows()
    const leg = flows.legs[0]!
    const rewritten: Flows.FlowsDoc = {
      ...flows,
      legs: [
        {
          ...leg,
          msgs: leg.msgs.map((m) =>
            "raw" in m && m.raw.includes("Require: 100rel")
              ? { ...m, raw: m.raw.replace(`Require: 100rel\r\n`, "") }
              : m
          )
        },
        flows.legs[1]!
      ]
    }
    expect(rewritten.legs[0]!.msgs.some((m) => "raw" in m && m.raw.includes(`RSeq: ${RELIABLE_RSEQ}`))).toBe(true)
    const out = stamp(rewritten, [{ leg: 0, hop: 0 }], registryOf(entry))
    expect(out.violations).toEqual([])
    expect(out.warnings).toHaveLength(1)
  })
})
