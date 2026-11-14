/**
 * The cut joins legs on a `header-pair` rule beside the Call-ID derivation: a
 * leg whose initial INVITE names another leg's Call-ID in a header of its own
 * is one call with it, where no derivation relates the two Call-IDs.
 */
import type { Flows, Rules } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { correlate, cutCalls, MISSING_UPSTREAM_LEG } from "../src/cut.js"
import { neverDerives } from "../src/derivation.js"
import { nameByCallIds, sutSet } from "./fixtures.js"
import { calleeLeg, callerLeg, LINK_HEADER, LINK_RULE, linkedDoc } from "./linked-legs.js"

const CALLER = "caller-1@192.0.2.9"
const CALLEE = "callee-1@192.0.2.1"
const OTHER_CALLEE = "callee-2@192.0.2.1"

/** The cut of `flows` at the fixture SUT, its legs correlated by `rules` alone: each call's legs and each refusal's reason. */
const cutOf = (flows: Flows.FlowsDoc, rules: ReadonlyArray<Rules.HeaderPairRule>) => {
  const cut = cutCalls(flows, sutSet(), "linked.pcap", correlate(flows, neverDerives, rules), nameByCallIds)
  return {
    calls: cut.calls.map((call) => ({ uac: call.uac.leg, uas: call.uas.map((v) => v.leg), callIds: call.callIds })),
    refused: cut.refused.map((r) => `${r.reason} ${(r.callIds ?? []).join(",")}`)
  }
}

describe("a header-pair rule in the cut", () => {
  const linked = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, CALLER)])

  it("joins the leg whose link header names another leg's Call-ID to that leg, as one call", () => {
    const correlation = correlate(linked, neverDerives, [LINK_RULE])
    expect(correlation.families).toEqual([[0, 1]])
    expect([correlation.basesOf(0), correlation.basesOf(1)], "the callee leg was set up off the caller's").toEqual([[], [0]])
    expect(cutOf(linked, [LINK_RULE])).toEqual({ calls: [{ uac: 0, uas: [1], callIds: [CALLER, CALLEE] }], refused: [] })
  })

  it("keeps today's joins where no rule is bound: the callee leg misses its upstream leg", () => {
    expect(correlate(linked, neverDerives).families).toEqual([[0], [1]])
    expect(cutOf(linked, [])).toEqual({
      calls: [{ uac: 0, uas: [], callIds: [CALLER] }],
      refused: [`${MISSING_UPSTREAM_LEG} ${CALLEE}`]
    })
  })

  it("joins nothing on a key no captured leg's Call-ID carries: the callee leg still misses its upstream leg", () => {
    const stray = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, "uncaptured-0@192.0.2.9")])
    expect(correlate(stray, neverDerives, [LINK_RULE]).families).toEqual([[0], [1]])
    expect(cutOf(stray, [LINK_RULE])).toEqual({
      calls: [{ uac: 0, uas: [], callIds: [CALLER] }],
      refused: [`${MISSING_UPSTREAM_LEG} ${CALLEE}`]
    })
  })

  it("joins every leg carrying one key: two legs naming one caller are one call with it", () => {
    const hunt = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, CALLER), calleeLeg(OTHER_CALLEE, 2_000, CALLER)])
    const correlation = correlate(hunt, neverDerives, [LINK_RULE])
    expect(correlation.families).toEqual([[0, 1, 2]])
    expect([1, 2].map((leg) => correlation.basesOf(leg))).toEqual([[0], [0]])
    expect(cutOf(hunt, [LINK_RULE]).calls).toEqual([{ uac: 0, uas: [1, 2], callIds: [CALLER, CALLEE, OTHER_CALLEE] }])
  })

  it("refuses a document that does not project a header the rule reads, naming the re-emission", () => {
    const unprojected = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, CALLER)], ["Call-ID"])
    expect(() => correlate(unprojected, neverDerives, [LINK_RULE])).toThrow(`--emit-headers Call-ID,${LINK_HEADER}`)
  })
})
