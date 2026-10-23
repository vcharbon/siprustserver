/**
 * The cut: which captured legs are one call of the system under test, and which
 * hop of each leg a simulated peer binds at.
 */
import { describe, expect, it } from "vitest"
import {
  boundaryHops,
  callFamilies,
  caseCallIds,
  correlate,
  cutCalls,
  hopOwners,
  HAIRPIN_LOOPBACK,
  MISSING_UPSTREAM_LEG,
  NO_DIALOG_AT_BOUNDARY,
  vantageIsInbound
} from "../src/cut.js"
import {
  CALLEE_CALL_ID,
  CALLER_CALL_ID,
  cancelRaceFlows,
  derivesNothing,
  derivesOnePrefix,
  foreignLegFlows,
  hairpinBesideAPlainCallFlows,
  hairpinLoopbackFlows,
  interleavedIngressFlows,
  keepaliveOnlyFlows,
  midDialogOnlyFlows,
  nameByCallIds,
  orphanBLegFlows,
  secondIngressTailFlows,
  sutSet,
  twoIngressOneLegFlows
} from "./fixtures.js"

describe("boundary hops", () => {
  it("is a hop with exactly one side in the SUT set", () => {
    const flows = cancelRaceFlows()
    expect(boundaryHops(flows.legs[0]!, sutSet())).toEqual([0])
    expect(boundaryHops(flows.legs[1]!, sutSet())).toEqual([0])
  })

  it("is no hop at all between two systems that are both peers", () => {
    const flows = foreignLegFlows()
    expect(boundaryHops(flows.legs[0]!, sutSet())).toEqual([])
  })
})

describe("call families", () => {
  it("joins two legs only where the derivation says one minted the other", () => {
    const flows = cancelRaceFlows()
    expect(callFamilies(flows, derivesOnePrefix)).toEqual([[0, 1]])
  })

  it("leaves them apart under the neutral derivation, which derives nothing", () => {
    const flows = cancelRaceFlows()
    expect(callFamilies(flows, derivesNothing)).toEqual([[0], [1]])
  })

  it("does not read the document's own call groups", () => {
    // The fixture's group joins legs 0 and 1. Under the neutral derivation the
    // cut still keeps them apart: a group is the extractor's heuristic, and the
    // derivation is the one joiner.
    const flows = cancelRaceFlows()
    expect(flows.groups[0]!.legs).toEqual([0, 1])
    expect(callFamilies(flows, derivesNothing)).not.toEqual([[0, 1]])
  })
})

describe("cutting a capture", () => {
  it("anchors the caller vantage on the inbound dialog-creating INVITE", () => {
    const cut = cutCalls(cancelRaceFlows(), sutSet(), "race.pcap", derivesOnePrefix, nameByCallIds)
    expect(cut.refused).toEqual([])
    expect(cut.calls).toHaveLength(1)
    const call = cut.calls[0]!
    expect(call.uac).toEqual({ leg: 0, hop: 0 })
    expect(call.uas).toEqual([{ leg: 1, hop: 0 }])
    expect(call.legs).toEqual([0, 1])
    expect(call.callIds).toEqual([CALLER_CALL_ID, CALLEE_CALL_ID])
  })

  it("reads the caller side off the anchor's own direction, not its position", () => {
    const flows = cancelRaceFlows()
    const sut = sutSet()
    expect(vantageIsInbound(flows, sut, { leg: 0, hop: 0 })).toBe(true)
    expect(vantageIsInbound(flows, sut, { leg: 1, hop: 0 })).toBe(false)
  })

  it("lets the caller name the case from the call's identity, so a regeneration keeps the id", () => {
    const cut = cutCalls(cancelRaceFlows(), sutSet(), "race.pcap", derivesOnePrefix, (call) =>
      call.callIds.includes(CALLER_CALL_ID) ? "known-case-id" : "unknown"
    )
    expect(cut.calls[0]!.id).toBe("known-case-id")
  })

  it("excludes a call the SUT only ever dialled, loudly", () => {
    const cut = cutCalls(orphanBLegFlows(), sutSet(), "orphan.pcap", derivesOnePrefix, nameByCallIds)
    expect(cut.calls).toEqual([])
    expect(cut.refused).toHaveLength(1)
    const artifact = cut.refused[0]!
    expect(artifact.reason).toBe(MISSING_UPSTREAM_LEG)
    expect(artifact.callIds).toEqual([CALLEE_CALL_ID])
    expect(artifact.line).toContain(MISSING_UPSTREAM_LEG)
    // No inbound anchor means no UAC vantage, so no document can exist for it:
    // the refusal states that rather than leaving the falsifier a hole.
    expect(artifact.disposition).toBe("unreachable")
  })

  it("cuts one call per dialog that ARRIVED at the SUT, not one call with two attempts", () => {
    const cut = cutCalls(twoIngressOneLegFlows(), sutSet(), "two.pcap", derivesOnePrefix, nameByCallIds)
    expect(cut.refused).toEqual([])
    expect(cut.calls).toHaveLength(2)
    // Each call takes the outbound dialog the SUT opened after its own ingress.
    expect(cut.calls[0]!.uac).toEqual({ leg: 0, hop: 0 })
    expect(cut.calls[0]!.uas).toEqual([{ leg: 1, hop: 0 }])
    expect(cut.calls[1]!.uac).toEqual({ leg: 0, hop: 1 })
    expect(cut.calls[1]!.uas).toEqual([{ leg: 1, hop: 1 }])
    expect(cut.calls.map((c) => c.ingressOrdinal)).toEqual([0, 1])
    expect(cut.notes).toHaveLength(1)
    expect(cut.notes[0]).toContain("2 dialogs that ARRIVED at")
    expect(cut.notes[0]).toContain("10.0.0.1:5060")
  })

  it("pairs an outbound dialog to the ingress at its own SUT socket, not to the nearest in time", () => {
    // The first call's b-leg is dialled at t=200, after the second call arrived
    // at t=100: on time alone it would land on the second ingress.
    const cut = cutCalls(interleavedIngressFlows(), sutSet(), "interleaved.pcap", derivesOnePrefix, nameByCallIds)
    expect(cut.calls).toHaveLength(2)
    expect(cut.calls[0]!.uac).toEqual({ leg: 0, hop: 0 })
    expect(cut.calls[0]!.uas).toEqual([{ leg: 1, hop: 0 }])
    expect(cut.calls[1]!.uac).toEqual({ leg: 0, hop: 1 })
    expect(cut.calls[1]!.uas).toEqual([{ leg: 1, hop: 1 }])
  })

  it("gives an unanchored boundary hop to the ingress at its own SUT socket", () => {
    // The tail of the SECOND ingress arrives from a second peer address, so it
    // anchors nothing; both ingresses ran the same way, so only the socket says
    // which one it continues.
    const flows = secondIngressTailFlows()
    expect(hopOwners(flows, 0, sutSet()).get(2)).toBe(1)
    const cut = cutCalls(flows, sutSet(), "tail.pcap", derivesOnePrefix, nameByCallIds)
    expect(cut.calls).toHaveLength(2)
    expect(cut.calls[0]!.uac).toEqual({ leg: 0, hop: 0 })
    expect(cut.calls[1]!.uac).toEqual({ leg: 0, hop: 1 })
  })

  it("records a family that reaches the SUT and opens no dialog at it", () => {
    const cut = cutCalls(midDialogOnlyFlows(), sutSet(), "middialog.pcap", derivesOnePrefix, nameByCallIds)
    expect(cut.calls).toEqual([])
    expect(cut.notCalls).toBe(0)
    expect(cut.refused).toHaveLength(1)
    const refusal = cut.refused[0]!
    expect(refusal.reason).toBe(NO_DIALOG_AT_BOUNDARY)
    expect(refusal.disposition).toBe("not-proposed")
    expect(refusal.legs).toEqual([0])
  })

  it("refuses a family the SUT hairpins, rather than reading its second transit as a fork", () => {
    const cut = cutCalls(hairpinLoopbackFlows(), sutSet(), "hairpin.pcap", derivesOnePrefix, nameByCallIds)
    expect(cut.calls).toEqual([])
    expect(cut.refused).toHaveLength(1)
    const refusal = cut.refused[0]!
    expect(refusal.reason).toBe(HAIRPIN_LOOPBACK)
    expect(refusal.legs).toEqual([0, 1, 2])
    // The looped leg, named once: the hop it opens a dialog on in each direction.
    expect(refusal.evidence).toEqual([
      {
        leg: 1,
        hop: 0,
        detail:
          `Call-ID '${CALLEE_CALL_ID}' opens a dialog in each direction at 10.0.0.1:5060 <-> 10.0.0.2:5060`
      }
    ])
    expect(refusal.disposition).toBe("unreachable")
  })

  it("counts a keepalive family rather than recording one row per exchange", () => {
    const cut = cutCalls(keepaliveOnlyFlows(), sutSet(), "keepalive.pcap", derivesOnePrefix, nameByCallIds)
    expect(cut.calls).toEqual([])
    expect(cut.refused).toEqual([])
    expect(cut.notCalls).toBe(1)
  })

  it("names each call by its identity, so a document listing its families in another order keeps every id", () => {
    const flows = hairpinBesideAPlainCallFlows()
    const reordered = { ...flows, legs: [...flows.legs.slice(2), ...flows.legs.slice(0, 2)] }
    const idsOf = (doc: typeof flows) => {
      const cut = cutCalls(doc, sutSet(), "both.pcap", derivesOnePrefix, nameByCallIds)
      return {
        calls: cut.calls.map((c) => c.id),
        refused: cut.refused.map((r) => r.caseId)
      }
    }
    const plain = "n:plain-call-call-id+1-plain-call-call-id"
    const hairpin = `n:${CALLER_CALL_ID}+${CALLEE_CALL_ID}+1-${CALLEE_CALL_ID}`
    expect(idsOf(flows)).toEqual({ calls: [plain], refused: [hairpin] })
    expect(idsOf(reordered)).toEqual({ calls: [plain], refused: [hairpin] })
  })

  it("names each ingress of a split family apart, by its ordinal", () => {
    const cut = cutCalls(twoIngressOneLegFlows(), sutSet(), "two.pcap", derivesOnePrefix, nameByCallIds)
    const family = `n:${CALLER_CALL_ID}+${CALLEE_CALL_ID}`
    expect(cut.calls.map((c) => c.id)).toEqual([`${family}#0`, `${family}#1`])
  })

  it("hands the namer each split call's own ingress — when and between which sockets it arrived — and the family's size", () => {
    const seen: Array<unknown> = []
    cutCalls(twoIngressOneLegFlows(), sutSet(), "two.pcap", derivesOnePrefix, (call) => {
      seen.push({ ingress: call.ingress, ingresses: call.ingresses })
      return nameByCallIds(call)
    })
    expect(seen).toEqual([
      { ingress: { at_us: 0, src: "10.0.0.9:5060", dst: "10.0.0.1:5060" }, ingresses: 2 },
      { ingress: { at_us: 1_040_000, src: "10.0.0.9:5061", dst: "10.0.0.1:5061" }, ingresses: 2 }
    ])
  })

  it("names a family that opens no dialog at the SUT through the namer as well, never off its legs", () => {
    const cut = cutCalls(midDialogOnlyFlows(), sutSet(), "middialog.pcap", derivesOnePrefix, nameByCallIds)
    expect(cut.refused.map((r) => r.caseId)).toEqual([`n:${CALLER_CALL_ID}`])
  })

  it("cuts nothing at all from a call that never touches the SUT", () => {
    const cut = cutCalls(foreignLegFlows(), sutSet(), "foreign.pcap", derivesOnePrefix, nameByCallIds)
    expect(cut).toEqual({ calls: [], refused: [], notCalls: 0, notes: [] })
  })
})

describe("the calls a case is a case OF", () => {
  it("is the whole correlated family restricted to the legs that touch the SUT", () => {
    const flows = cancelRaceFlows()
    expect(caseCallIds(flows, sutSet(), [0], correlate(flows, derivesOnePrefix))).toEqual([
      CALLER_CALL_ID,
      CALLEE_CALL_ID
    ])
  })

  it("is one call where nothing derives, even with both legs cut", () => {
    const flows = cancelRaceFlows()
    expect(caseCallIds(flows, sutSet(), [0], correlate(flows, derivesNothing))).toEqual([
      CALLER_CALL_ID
    ])
  })
})
