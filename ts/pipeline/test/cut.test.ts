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
    const cut = cutCalls(cancelRaceFlows(), sutSet(), "race.pcap", derivesOnePrefix)
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

  it("lets the caller name the case, so a regeneration keeps the id", () => {
    const cut = cutCalls(cancelRaceFlows(), sutSet(), "race.pcap", derivesOnePrefix, (legs) =>
      legs.join("-") === "0-1" ? "known-case-id" : undefined
    )
    expect(cut.calls[0]!.id).toBe("known-case-id")
  })

  it("excludes a call the SUT only ever dialled, loudly", () => {
    const cut = cutCalls(orphanBLegFlows(), sutSet(), "orphan.pcap", derivesOnePrefix)
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
    const cut = cutCalls(twoIngressOneLegFlows(), sutSet(), "two.pcap", derivesOnePrefix)
    expect(cut.refused).toEqual([])
    expect(cut.calls).toHaveLength(2)
    // Each call takes the outbound dialog the SUT opened after its own ingress.
    expect(cut.calls[0]!.uac).toEqual({ leg: 0, hop: 0 })
    expect(cut.calls[0]!.uas).toEqual([{ leg: 1, hop: 0 }])
    expect(cut.calls[1]!.uac).toEqual({ leg: 0, hop: 1 })
    expect(cut.calls[1]!.uas).toEqual([{ leg: 1, hop: 1 }])
    expect(cut.calls.map((c) => c.id)).toEqual(["two.pcap-cut1", "two.pcap-cut2"])
    expect(cut.notes).toHaveLength(1)
    expect(cut.notes[0]).toContain("2 dialogs that ARRIVED at")
    expect(cut.notes[0]).toContain("10.0.0.1:5060")
  })

  it("pairs an outbound dialog to the ingress at its own SUT socket, not to the nearest in time", () => {
    // The first call's b-leg is dialled at t=200, after the second call arrived
    // at t=100: on time alone it would land on the second ingress.
    const cut = cutCalls(interleavedIngressFlows(), sutSet(), "interleaved.pcap", derivesOnePrefix)
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
    const cut = cutCalls(flows, sutSet(), "tail.pcap", derivesOnePrefix)
    expect(cut.calls).toHaveLength(2)
    expect(cut.calls[0]!.uac).toEqual({ leg: 0, hop: 0 })
    expect(cut.calls[1]!.uac).toEqual({ leg: 0, hop: 1 })
  })

  it("does not let a split family inherit the one id the corpus knows the leg set by", () => {
    const cut = cutCalls(twoIngressOneLegFlows(), sutSet(), "two.pcap", derivesOnePrefix, () => "known-case-id")
    expect(cut.calls.map((c) => c.id)).toEqual(["two.pcap-cut1", "two.pcap-cut2"])
  })

  it("records a family that reaches the SUT and opens no dialog at it", () => {
    const cut = cutCalls(midDialogOnlyFlows(), sutSet(), "middialog.pcap", derivesOnePrefix)
    expect(cut.calls).toEqual([])
    expect(cut.notCalls).toBe(0)
    expect(cut.refused).toHaveLength(1)
    const refusal = cut.refused[0]!
    expect(refusal.reason).toBe(NO_DIALOG_AT_BOUNDARY)
    // No case was ever on the table, so it takes no `-cutN` slot: that counter
    // is the CASE namespace and spending one here renumbers every case after it.
    expect(refusal.caseId).toBe("middialog.pcap-legs0")
    expect(refusal.disposition).toBe("not-proposed")
    expect(refusal.legs).toEqual([0])
  })

  it("refuses a family the SUT hairpins, rather than reading its second transit as a fork", () => {
    const cut = cutCalls(hairpinLoopbackFlows(), sutSet(), "hairpin.pcap", derivesOnePrefix)
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
    // A case WAS proposable, so the refusal spends the slot it would have taken.
    expect(refusal.caseId).toBe("hairpin.pcap-cut1")
    expect(refusal.disposition).toBe("unreachable")
  })

  it("does not spend the name a case is about to inherit on the family beside it", () => {
    // A selection name is CLAIMED when it is read, so a refusal that names a
    // family before knowing it refuses one renames the case that follows.
    const names = new Map([["0,1", "known-case1"], ["2,3,4", "known-case2"]])
    const taken = new Set<string>()
    const inheritId = (legs: ReadonlyArray<number>): string | undefined => {
      const id = names.get([...legs].join(","))
      if (id === undefined || taken.has(id)) return undefined
      taken.add(id)
      return id
    }
    const cut = cutCalls(hairpinBesideAPlainCallFlows(), sutSet(), "both.pcap", derivesOnePrefix, inheritId)
    expect(cut.calls.map((c) => c.id)).toEqual(["known-case1"])
    expect(cut.refused.map((r) => r.caseId)).toEqual(["known-case2"])
  })

  it("counts a keepalive family rather than recording one row per exchange", () => {
    const cut = cutCalls(keepaliveOnlyFlows(), sutSet(), "keepalive.pcap", derivesOnePrefix)
    expect(cut.calls).toEqual([])
    expect(cut.refused).toEqual([])
    expect(cut.notCalls).toBe(1)
  })

  it("cuts nothing at all from a call that never touches the SUT", () => {
    const cut = cutCalls(foreignLegFlows(), sutSet(), "foreign.pcap", derivesOnePrefix)
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
