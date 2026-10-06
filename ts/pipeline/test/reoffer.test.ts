/**
 * A vantage holding a second dialog-opening INVITE from the same originator:
 * the case-tier refusal the pipeline states on its own account, decided on the
 * capture and the cut's vantages.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import type { CaseSpec } from "../src/case-spec.js"
import { decideCases } from "../src/caseset.js"
import { cutCalls } from "../src/cut.js"
import { policyWith } from "../src/policy.js"
import {
  CALLER_CALL_ID,
  CALLEE_CALL_ID,
  derivesOnePrefix,
  newFromTagRetryFlows,
  plan,
  proxyRetriedCopyFlows,
  redirectRetryFlows,
  reofferAfterAnsweredFlows,
  reofferAfterCancelFlows,
  reofferWhileRingingFlows,
  repeatedNotFoundFlows,
  sameCseqOffersFlows,
  secondIngressTailFlows,
  SOCKETS,
  sutRedirectRetryFlows,
  stripVias,
  sutSet,
  taggedInviteAfterFinalFlows,
  thriceOfferedFlows,
  twoIngressOneLegFlows,
  twoTagOffersFlows,
  withMsg,
  withoutVia
} from "./fixtures.js"

const REOFFERED = "scope-identity-reoffered"
const CALLER_TAG = `from-${CALLER_CALL_ID}`

/** The cut's cases of a capture, decided under a policy that refuses nothing of its own. */
const decide = (flows: Flows.FlowsDoc, capture = "capture.pcap") => {
  const cut = cutCalls(flows, sutSet(), capture, derivesOnePrefix)
  const specs: ReadonlyArray<CaseSpec> = cut.calls.map((c) => ({
    id: c.id,
    uac: c.uac,
    uas: [...c.uas],
    defects: [],
    cutLegs: c.legs
  }))
  return decideCases({
    flows,
    capture,
    specs,
    sut: sutSet(),
    plan: plan(),
    policy: policyWith({ derives: derivesOnePrefix })
  })
}

const reoffers = (flows: Flows.FlowsDoc) =>
  decide(flows).refused.filter((r) => r.reason === REOFFERED)

/** The one re-offer refusal of a capture, which must exist. */
const reoffer = (flows: Flows.FlowsDoc) => {
  const found = reoffers(flows)
  expect(found).toHaveLength(1)
  return found[0]!
}

describe("a second dialog-opening INVITE from the vantage's originator", () => {
  it("is refused on the same CSeq after the first one's CANCEL and 487", () => {
    const decided = decide(reofferAfterCancelFlows())
    expect(decided.outcomes).toHaveLength(1)
    const refusal = decided.refused.find((r) => r.reason === REOFFERED)
    expect(refusal?.disposition).toBe("refuses")
    expect(decided.outcomes[0]!.built).toBeUndefined()
    expect(refusal?.line).toContain(
      `re-offers INVITE CSeq 1 (msg 7, From tag '${CALLER_TAG}') under a new branch, 2000 ms ` +
        `after the 487 that ended INVITE CSeq 1 (msg 0, From tag '${CALLER_TAG}')`
    )
    expect(refusal?.evidence).toEqual([
      expect.objectContaining({ leg: 0, hop: 0, msg: 7, callId: CALLER_CALL_ID })
    ])
  })

  it("is refused on CSeq+1 after a 302 (the RFC 3261 §8.1.3.4 redirect retry)", () => {
    expect(reoffer(redirectRetryFlows()).line).toContain(
      "INVITE CSeq 2 (msg 3, From tag"
    )
    expect(reoffer(redirectRetryFlows()).line).toContain("20 ms after the 302 that ended INVITE CSeq 1 (msg 0")
  })

  it("is refused under a new From tag, and names both tags", () => {
    expect(reoffer(newFromTagRetryFlows()).line).toContain(
      "re-offers INVITE CSeq 2 (msg 3, From tag 'second-tag') under a new branch, 1280 ms after " +
        "the 404 that ended INVITE CSeq 1 (msg 0, From tag 'first-tag')"
    )
  })

  it("is refused while the first INVITE has drawn only a provisional", () => {
    expect(reoffer(reofferWhileRingingFlows()).line).toContain(
      `INVITE CSeq 2 (msg 2, From tag '${CALLER_TAG}') under a new branch, 400 ms after ` +
        `INVITE CSeq 1 (msg 0, From tag '${CALLER_TAG}'), which drew no final`
    )
  })

  it("is refused after the first dialog was answered and torn down", () => {
    expect(reoffer(reofferAfterAnsweredFlows()).line).toContain(
      "INVITE CSeq 3 (msg 5, From tag"
    )
    expect(reoffer(reofferAfterAnsweredFlows()).line).toContain(
      "3500 ms after the 200 that answered INVITE CSeq 1 (msg 0"
    )
  })

  it("names, for each re-offer, the opener just before it", () => {
    const refusal = reoffer(thriceOfferedFlows())
    expect(refusal.evidence?.map((e) => e.msg)).toEqual([3, 5])
    expect(refusal.line).toContain("80 ms after the 404 that ended INVITE CSeq 1 (msg 0")
    expect(refusal.line).toContain(
      `300 ms after INVITE CSeq 2 (msg 3, From tag '${CALLER_TAG}'), which drew no final`
    )
  })

  it("is refused on a called vantage too: one leg scripts one dialog, either direction", () => {
    const refusal = reoffer(sutRedirectRetryFlows())
    expect(refusal.evidence).toEqual([
      expect.objectContaining({ leg: 1, hop: 0, msg: 3, callId: CALLEE_CALL_ID })
    ])
    expect(refusal.line).toContain("10 ms after the 302 that ended INVITE CSeq 1 (msg 0")
  })

  it("is refused from the same host on another port (RFC 3261 §18.1.1)", () => {
    const [host] = SOCKETS.caller.split(":")
    expect(
      reoffers(proxyRetriedCopyFlows({ sentBy: `${host}:5070`, branch: "z9hG4bK-new-own" }))
    ).toHaveLength(1)
  })

  it("is refused where the bottom Vias name no branch that could prove a copy", () => {
    const branchless = (m: Flows.Msg): Flows.Msg => ({
      ...m,
      via: m.via?.map((v, i, all) => (i === all.length - 1 ? { ...v, branch: null } : v))
    }) as Flows.Msg
    const flows = withMsg(withMsg(proxyRetriedCopyFlows(), { leg: 0, msg: 0 }, branchless), { leg: 0, msg: 3 }, branchless)
    expect(reoffers(flows)).toHaveLength(1)
  })

  it("is refused where the second copy carries no Via that could prove it the same request", () => {
    expect(reoffers(withMsg(proxyRetriedCopyFlows(), { leg: 0, msg: 3 }, withoutVia))).toHaveLength(1)
    expect(reoffers(withMsg(proxyRetriedCopyFlows(), { leg: 0, msg: 0 }, withoutVia))).toHaveLength(1)
  })
})

describe("the final a finding names", () => {
  it("is the one whose branch answers the opener, not an earlier opener's on the same CSeq", () => {
    expect(reoffer(sameCseqOffersFlows()).line).toContain(
      "80 ms after the 404 that ended INVITE CSeq 1 (msg 2"
    )
  })

  it("is each opener's first final, never one an earlier opener already took", () => {
    expect(reoffer(stripVias(repeatedNotFoundFlows())).line).toContain(
      "80 ms after the 404 that ended INVITE CSeq 1 (msg 3"
    )
  })

  it("is read by CSeq and From tag where the vantage carried no Via", () => {
    const refusal = reoffer(stripVias(twoTagOffersFlows()))
    expect(refusal.evidence?.map((e) => e.msg)).toEqual([2, 5, 8])
    expect(refusal.line).toContain("100 ms after INVITE CSeq 1 (msg 0, From tag 'a'), which drew no final")
    expect(refusal.line).toContain("80 ms after the 404 that ended INVITE CSeq 1 (msg 2, From tag 'b')")
    expect(refusal.line).toContain("80 ms after the 404 that ended INVITE CSeq 2 (msg 5, From tag 'a')")
  })
})

describe("a second INVITE that opens no second dialog of the vantage's originator", () => {
  it("is not refused where it is the caller's one request forwarded again, its bottom Via kept", () => {
    expect(reoffers(proxyRetriedCopyFlows())).toEqual([])
  })

  it("is not refused where another element originated it", () => {
    expect(
      reoffers(proxyRetriedCopyFlows({ sentBy: "10.0.0.8:5060", branch: "z9hG4bK-elsewhere" }))
    ).toEqual([])
  })

  it("is not refused where it carries a To tag: an in-dialog request, never an opener", () => {
    expect(reoffers(taggedInviteAfterFinalFlows())).toEqual([])
  })

  it("is not refused where the capture marks it a retransmission", () => {
    const flows = withMsg(reofferAfterCancelFlows(), { leg: 0, msg: 7 }, (m) => ({ ...m, retx: true }))
    expect(reoffers(flows)).toEqual([])
  })

  it("is not refused where it arrives at another SUT socket, which the cut makes a call of its own", () => {
    // Each ingress is its own vantage, so neither case holds both INVITEs.
    const decided = decide(twoIngressOneLegFlows())
    expect(decided.outcomes).toHaveLength(2)
    expect(decided.refused.filter((r) => r.reason === REOFFERED)).toEqual([])
    expect(reoffers(secondIngressTailFlows())).toEqual([])
  })
})
