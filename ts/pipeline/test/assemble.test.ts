/**
 * Assembly under a policy: what the capture states is built, and what one
 * PLATFORM does is asked for — including the answer "nothing", which is what the
 * neutral policy gives.
 */
import { Pivot, Tokens } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { assemble } from "../src/assemble.js"
import type { CaseSpec } from "../src/case-spec.js"
import { decideCases } from "../src/caseset.js"
import type { CaseRule } from "../src/case-rules.js"
import { neutralPolicy, policyWith, UNCLASSIFIED, type Refusal } from "../src/policy.js"
import { abandonedFlows, cancelRaceFlows, derivesNothing, derivesOnePrefix, plan, refusedFlows, sutSet } from "./fixtures.js"

const CAPTURE = "race.pcap"

const SPEC: CaseSpec = {
  id: "race-1",
  uac: { leg: 0, hop: 0 },
  uas: [{ leg: 1, hop: 0 }],
  defects: [],
  cutLegs: [0, 1]
}

const built = (policy = policyWith({ derives: derivesOnePrefix })) =>
  assemble({
    flows: cancelRaceFlows(),
    capture: CAPTURE,
    spec: SPEC,
    sut: sutSet(),
    plan: plan(),
    policy
  })

describe("a document assembled under the neutral policy", () => {
  it("round-trips through the pivot contract it claims to be", () => {
    const { pivot } = built(neutralPolicy)
    expect(Pivot.versionMatches(pivot)).toBe(true)
    expect(Pivot.decodePivotSync(JSON.parse(Pivot.emitPivot(pivot)) as unknown)).toEqual(pivot)
  })

  it("states nothing it was not told: no family, no lanes, no declared failure", () => {
    const { pivot } = built(neutralPolicy)
    expect(pivot.case.family).toBe(UNCLASSIFIED)
    expect(pivot.case.lanes).toEqual({})
    expect(pivot.must_fail).toBeUndefined()
    expect(pivot.calls[0]!.relay18x).toBeUndefined()
    expect(pivot.calls[0]!.attempts.every((a) => a.cause === undefined)).toBe(true)
  })

  it("still states everything the capture itself carries", () => {
    const { pivot } = built(neutralPolicy)
    expect(pivot.case.origin).toBe("capture")
    expect(pivot.case.source?.capture).toBe(CAPTURE)
    expect(pivot.timing.capture_span_ms).toBeGreaterThan(0)
    // A capture says nothing about billing, so the gap is STATED, not guessed.
    expect(pivot.postconditions?.cdr).toEqual({ absent: "capture-carries-no-cdr" })
  })
})

describe("what the policy decides", () => {
  it("names the family and the lanes", () => {
    const { pivot } = built(
      policyWith({
        derives: derivesOnePrefix,
        familyOf: () => "transparent",
        lanes: () => ({
          "some-lane": Tokens.laneVerdictToken({ _tag: "ok" }),
          "other-lane": Tokens.laneVerdictToken({
            _tag: "blocked",
            reason: "number-unclassified"
          })
        })
      })
    )
    expect(pivot.case.family).toBe("transparent")
    expect(pivot.case.lanes).toEqual({
      "some-lane": "ok",
      "other-lane": "blocked:number-unclassified"
    })
  })

  it("puts the handover cause and its evidence on the attempt it belongs to", () => {
    const { pivot } = built(
      policyWith({
        derives: derivesOnePrefix,
        chain: () => ({
          causes: [
            {
              from: "called[0][0]",
              cause: Tokens.causeToken({ _tag: "busy" }),
              evidence: ["the callee answered 486"]
            }
          ],
          flags: [{ kind: "chain-read", detail: "one handover" }]
        })
      })
    )
    const attempt = pivot.calls[0]!.attempts.find((a) => a.branch === 0 && a.position === 0)
    expect(attempt?.cause).toBe("busy")
    expect(attempt?.cause_evidence).toEqual(["the callee answered 486"])
    expect(pivot.case.annotations?.flags?.some((f) => f.kind === "chain-read")).toBe(true)
  })

  it("decides which calls the case is a case of, through `derives` alone", () => {
    // One vantage, on the caller's leg. Whether the b-leg's call belongs to this
    // case is the derivation's answer and nothing else's — and it decides
    // whether a registry entry naming that call is this case's to answer.
    const callerOnly: CaseSpec = { id: "race-1", uac: SPEC.uac, uas: [], defects: [], cutLegs: [0] }
    const allowed = {
      captures: {
        [CAPTURE]: [
          {
            rule: "no-200-after-cancel" as const,
            originator: "10.0.0.2:5060",
            call_ids: ["1-caller-leg-call-id"] as [string, ...Array<string>],
            note: "the far callee answered after taking the CANCEL"
          }
        ]
      }
    }
    const one = (derives: typeof derivesNothing) =>
      assemble({
        flows: cancelRaceFlows(),
        capture: CAPTURE,
        spec: callerOnly,
        sut: sutSet(),
        plan: plan(),
        policy: policyWith({ derives }),
        allowed
      })
    // The b-leg call IS this case's, so the entry is owed an anchor and warns.
    expect(one(derivesOnePrefix).warnings).toHaveLength(1)
    // It is a different call entirely, so the entry is not this case's to answer.
    expect(one(derivesNothing).warnings).toEqual([])
  })

  it("adapts the flow, and what it changed rides the document", () => {
    const { pivot } = built(
      policyWith({
        derives: derivesOnePrefix,
        adaptFlow: (_flows, flow) => ({
          ...flow,
          flags: [...flow.flags, { kind: "lane-delta", detail: "the ACK is answered locally" }]
        })
      })
    )
    expect(pivot.case.annotations?.flags?.some((f) => f.kind === "lane-delta")).toBe(true)
  })
})

describe("deciding a whole capture", () => {
  /** One canned refusal as the case-tier rule that states it. */
  const rulesOf = (refusals: ReadonlyArray<Refusal>): ReadonlyArray<CaseRule> =>
    refusals.map((r) => ({
      id: r.reason,
      subject: r.reason.startsWith("sut-") ? "sut" : "scope",
      disposition: r.disposition,
      refuses: () => ({
        line: r.line,
        ...(r.charged === undefined ? {} : { charged: r.charged })
      })
    }))

  const decide = (
    refusals: ReadonlyArray<Refusal>,
    declare = neutralPolicy.declare,
    quarantine = false
  ) =>
    decideCases({
      flows: cancelRaceFlows(),
      capture: CAPTURE,
      specs: [SPEC],
      sut: sutSet(),
      plan: plan(),
      policy: policyWith({ derives: derivesOnePrefix, refuse: rulesOf(refusals), declare }),
      quarantine
    })

  it("generates the case where nothing refuses it", () => {
    const out = decide([])
    expect(out.refused).toEqual([])
    expect(out.outcomes[0]!.built).toBeDefined()
  })

  it("never assembles a case refused on a rule no declaration can answer", () => {
    const refusal: Refusal = {
      caseId: SPEC.id,
      capture: CAPTURE,
      reason: "scope-mechanism-not-statable",
      disposition: "refuses",
      line: "refused: the mechanism is out of scope"
    }
    const out = decide([refusal])
    expect(out.outcomes[0]!.built).toBeUndefined()
    expect(out.refused).toEqual([refusal])
  })

  it("withdraws a deferred refusal where the document declares every charged hit", () => {
    const charged = {
      rule: "no-ack-to-dialog-creating-2xx",
      callId: "1-caller-leg-call-id",
      emitter: "10.0.0.1:5060",
      leg: 1
    }
    const out = decide(
      [
        {
          caseId: SPEC.id,
          capture: CAPTURE,
          reason: "sut-violates:no-ack-to-dialog-creating-2xx",
          line: "refused unless declared",
          disposition: "defers",
          charged: [charged]
        }
      ],
      () => ({
        entries: [
          { failure: "unexpected-ack", step: "s1", derived_from: "no-ack-to-dialog-creating-2xx" }
        ],
        flags: [],
        coverage: [{ ...charged, anchor: "s1" }]
      })
    )
    expect(out.refused).toEqual([])
    expect(out.outcomes[0]!.built?.pivot.must_fail).toHaveLength(1)
  })

  it("carries no quarantined document unless the caller asked for one", () => {
    const refusal: Refusal = {
      caseId: SPEC.id,
      capture: CAPTURE,
      reason: "scope-mechanism-not-statable",
      disposition: "refuses",
      line: "refused: the mechanism is out of scope"
    }
    const out = decide([refusal])
    expect(out.outcomes[0]!.quarantined).toBeUndefined()
    expect(out.outcomes[0]!.quarantineError).toBeUndefined()
  })

  it("assembles a refused case under quarantine without generating it", () => {
    const refusal: Refusal = {
      caseId: SPEC.id,
      capture: CAPTURE,
      reason: "scope-mechanism-not-statable",
      disposition: "refuses",
      line: "refused: the mechanism is out of scope"
    }
    const out = decide([refusal], neutralPolicy.declare, true)
    // The refusal STANDS: quarantine falsifies a rule, it never moves the corpus.
    expect(out.refused).toEqual([refusal])
    expect(out.outcomes[0]!.built).toBeUndefined()
    expect(out.outcomes[0]!.quarantined?.pivot.case.id).toBe(SPEC.id)
  })

  it("quarantines the document a POST-document refusal was decided on", () => {
    const refusal: Refusal = {
      caseId: SPEC.id,
      capture: CAPTURE,
      reason: "sut-violates:no-ack-to-dialog-creating-2xx",
      line: "refused unless declared",
      disposition: "defers",
      charged: [
        {
          rule: "no-ack-to-dialog-creating-2xx",
          callId: "1-caller-leg-call-id",
          emitter: "10.0.0.1:5060",
          leg: 1
        }
      ]
    }
    const out = decide([refusal], neutralPolicy.declare, true)
    expect(out.refused).toHaveLength(1)
    expect(out.outcomes[0]!.built).toBeUndefined()
    // The same bytes the generated case would have carried, not a second build.
    expect(out.outcomes[0]!.quarantined?.pivot).toEqual(built().pivot)
  })

  it("quarantines nothing for a case nothing refused", () => {
    const out = decide([], neutralPolicy.declare, true)
    expect(out.outcomes[0]!.built).toBeDefined()
    expect(out.outcomes[0]!.quarantined).toBeUndefined()
  })

  it("keeps the refusal standing where a charged hit anchors on nothing", () => {
    const charged = {
      rule: "no-ack-to-dialog-creating-2xx",
      callId: "1-caller-leg-call-id",
      emitter: "10.0.0.1:5060",
      leg: 1
    }
    const out = decide(
      [
        {
          caseId: SPEC.id,
          capture: CAPTURE,
          reason: "sut-violates:no-ack-to-dialog-creating-2xx",
          line: "refused unless declared",
          disposition: "defers",
          charged: [charged]
        }
      ],
      () => ({
        entries: [],
        flags: [],
        coverage: [{ ...charged, refusal: "no step carries that 2xx at this vantage" }]
      })
    )
    expect(out.refused).toHaveLength(1)
    expect(out.outcomes[0]!.built).toBeUndefined()
  })
})

describe("a call the platform refused", () => {
  const REFUSED_SPEC: CaseSpec = {
    id: "refused-1",
    uac: { leg: 0, hop: 0 },
    uas: [],
    defects: [],
    cutLegs: [0]
  }

  const refused = () =>
    assemble({
      flows: refusedFlows(),
      capture: "refused.pcap",
      spec: REFUSED_SPEC,
      sut: sutSet(),
      plan: plan(),
      policy: neutralPolicy
    })

  it("dials nobody and states the decision that dialled nobody", () => {
    const { pivot } = refused()
    const call = pivot.calls[0]!
    expect(call.attempts).toEqual([])
    const final = pivot.flow.find((step) => "msg" in step && step.msg.status === 480)!
    expect(call.refused?.step).toBe(final.id)
    expect(call.refused?.evidence).toContain("480")
  })

  it("round-trips through the pivot contract it claims to be", () => {
    const { pivot } = refused()
    expect(Pivot.decodePivotSync(JSON.parse(Pivot.emitPivot(pivot)) as unknown)).toEqual(pivot)
  })
})

describe("a call the caller abandoned", () => {
  const ABANDONED_SPEC: CaseSpec = {
    id: "abandoned-1",
    uac: { leg: 0, hop: 0 },
    uas: [],
    defects: [],
    cutLegs: [0]
  }

  const abandoned = () =>
    assemble({
      flows: abandonedFlows(),
      capture: "abandoned.pcap",
      spec: ABANDONED_SPEC,
      sut: sutSet(),
      plan: plan(),
      policy: neutralPolicy
    })

  it("states the CANCEL the caller sent, and no refusal", () => {
    const { pivot } = abandoned()
    const call = pivot.calls[0]!
    expect(call.attempts).toEqual([])
    expect(call.refused).toBeUndefined()
    const cancel = pivot.flow.find(
      (step) => "msg" in step && step.msg.method === "CANCEL"
    )!
    expect(call.abandoned?.step).toBe(cancel.id)
    expect(call.abandoned?.evidence).toContain("cancelled its own INVITE")
  })

  it("never reads the 487 answering that CANCEL as the platform's decision", () => {
    const { pivot } = abandoned()
    const terminated = pivot.flow.find((step) => "msg" in step && step.msg.status === 487)!
    expect(pivot.calls[0]!.refused?.step).not.toBe(terminated.id)
  })

  it("round-trips through the pivot contract it claims to be", () => {
    const { pivot } = abandoned()
    expect(Pivot.decodePivotSync(JSON.parse(Pivot.emitPivot(pivot)) as unknown)).toEqual(pivot)
  })
})
