/**
 * The cut joins legs on a `header-pair` rule beside the Call-ID derivation: a
 * leg whose initial INVITE names another leg in a header of its own is one call
 * with it, where no derivation relates the two Call-IDs. Under a window, a leg
 * joins the nearest leg carrying its key within it, and none outside.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { correlate, cutCalls, MISSING_UPSTREAM_LEG } from "../src/cut.js"
import { neverDerives } from "../src/derivation.js"
import { type LegRules, NO_LEG_RULES, UnprojectedHeaders } from "../src/header-pair-join.js"
import { nameByCallIds, sutSet } from "./fixtures.js"
import {
  calleeLeg,
  callerLeg,
  dialledLeg,
  LINK_HEADER,
  LINK_RULE,
  linkedDoc,
  PARENT_REF_HEADER,
  REF_HEADER,
  REF_RULE,
  requiring,
  skipping
} from "./linked-legs.js"

const CALLER = "caller-1@192.0.2.9"
const CALLEE = "callee-1@192.0.2.1"
const OTHER_CALLER = "caller-2@192.0.2.9"
const OTHER_CALLEE = "callee-2@192.0.2.1"

/** A caller leg and the callee leg naming it, every rule header projected. */
const linked = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, CALLER)])

/** The cut of `flows` at the fixture SUT, its legs correlated by `legRules` alone: each call's legs and each refusal's reason. */
const cutOf = (flows: Flows.FlowsDoc, legRules: LegRules) => {
  const cut = cutCalls(flows, sutSet(), "linked.pcap", correlate(flows, neverDerives, legRules), nameByCallIds)
  return {
    calls: cut.calls.map((call) => ({ uac: call.uac.leg, uas: call.uas.map((v) => v.leg), callIds: call.callIds })),
    refused: cut.refused.map((r) => `${r.reason} ${(r.callIds ?? []).join(",")}`)
  }
}

describe("a header-pair rule in the cut", () => {
  it("joins the leg whose link header names another leg's Call-ID, never projected, to that leg, as one call", () => {
    const correlation = correlate(linked, neverDerives, requiring(LINK_RULE))
    expect(linked.emit_headers, "Call-ID is read off the leg").not.toContain("Call-ID")
    expect(correlation.families).toEqual([[0, 1]])
    expect([correlation.basesOf(0), correlation.basesOf(1)], "the callee leg was set up off the caller's").toEqual([[], [0]])
    expect(correlation.joinsOf(1)).toEqual([{ base: 0, by: "rule", rule: LINK_RULE.name }])
    expect(cutOf(linked, requiring(LINK_RULE))).toEqual({ calls: [{ uac: 0, uas: [1], callIds: [CALLER, CALLEE] }], refused: [] })
  })

  it("keeps today's joins where no rule is bound: the callee leg misses its upstream leg", () => {
    expect(correlate(linked, neverDerives).families).toEqual([[0], [1]])
    expect(cutOf(linked, NO_LEG_RULES)).toEqual({
      calls: [{ uac: 0, uas: [], callIds: [CALLER] }],
      refused: [`${MISSING_UPSTREAM_LEG} ${CALLEE}`]
    })
  })

  it("joins nothing on a key no captured leg's Call-ID carries: the callee leg still misses its upstream leg", () => {
    const stray = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, "uncaptured-0@192.0.2.9")])
    expect(correlate(stray, neverDerives, requiring(LINK_RULE)).families).toEqual([[0], [1]])
    expect(cutOf(stray, requiring(LINK_RULE))).toEqual({
      calls: [{ uac: 0, uas: [], callIds: [CALLER] }],
      refused: [`${MISSING_UPSTREAM_LEG} ${CALLEE}`]
    })
  })

  it("joins every leg carrying one key: two legs naming one caller are one call with it", () => {
    const hunt = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, CALLER), calleeLeg(OTHER_CALLEE, 2_000, CALLER)])
    const correlation = correlate(hunt, neverDerives, requiring(LINK_RULE))
    expect(correlation.families).toEqual([[0, 1, 2]])
    expect([1, 2].map((leg) => correlation.basesOf(leg))).toEqual([[0], [0]])
    expect(cutOf(hunt, requiring(LINK_RULE)).calls).toEqual([{ uac: 0, uas: [1, 2], callIds: [CALLER, CALLEE, OTHER_CALLEE] }])
  })

  it("never joins a leg to itself: a leg naming its own Call-ID is set up off no leg, with or without a window", () => {
    const own = linkedDoc([callerLeg(CALLER, 0, [[LINK_HEADER, CALLER]])])
    for (const rule of [LINK_RULE, { ...LINK_RULE, window_ms: 5_000 }]) {
      const correlation = correlate(own, neverDerives, requiring(rule))
      expect([correlation.families, correlation.joinsOf(0)], `window ${rule.window_ms}`).toEqual([[[0]], []])
    }
  })

  it("joins a leg carrying its key twice to its base once", () => {
    const twice = linkedDoc([callerLeg(CALLER, 0), dialledLeg(CALLEE, 10, [[LINK_HEADER, CALLER], [LINK_HEADER, CALLER]])])
    expect(correlate(twice, neverDerives, requiring(LINK_RULE)).joinsOf(1)).toEqual([{ base: 0, by: "rule", rule: LINK_RULE.name }])
  })
})

describe("a header-pair rule's window", () => {
  const HOUR_MS = 3_600_000
  /** Two unrelated calls an hour apart whose short references are equal. */
  const twoCalls = linkedDoc([
    callerLeg(CALLER, 0, [[REF_HEADER, "7"]]),
    dialledLeg(CALLEE, 10, [[PARENT_REF_HEADER, "7"]]),
    callerLeg(OTHER_CALLER, HOUR_MS, [[REF_HEADER, "7"]]),
    dialledLeg(OTHER_CALLEE, HOUR_MS + 10, [[PARENT_REF_HEADER, "7"]])
  ])

  it("without a window, joins every leg carrying the key: the two calls are one family", () => {
    const correlation = correlate(twoCalls, neverDerives, requiring(REF_RULE))
    expect(correlation.families).toEqual([[0, 1, 2, 3]])
    expect([1, 3].map((leg) => correlation.basesOf(leg))).toEqual([[0, 2], [0, 2]])
  })

  it("with one, joins each leg to the nearest leg carrying its key within it: two calls, each of its own legs", () => {
    const windowed = requiring({ ...REF_RULE, window_ms: 5_000 })
    const correlation = correlate(twoCalls, neverDerives, windowed)
    expect(correlation.families).toEqual([[0, 1], [2, 3]])
    expect([1, 3].map((leg) => correlation.basesOf(leg))).toEqual([[0], [2]])
    expect(cutOf(twoCalls, windowed)).toEqual({
      calls: [
        { uac: 0, uas: [1], callIds: [CALLER, CALLEE] },
        { uac: 2, uas: [3], callIds: [OTHER_CALLER, OTHER_CALLEE] }
      ],
      refused: []
    })
  })

  it("joins the nearer of two legs within it, and none outside it", () => {
    const near = linkedDoc([
      callerLeg(CALLER, 0, [[REF_HEADER, "7"]]),
      callerLeg(OTHER_CALLER, 3_000, [[REF_HEADER, "7"]]),
      dialledLeg(CALLEE, 3_010, [[PARENT_REF_HEADER, "7"]]),
      dialledLeg(OTHER_CALLEE, 20_000, [[PARENT_REF_HEADER, "7"]])
    ])
    const correlation = correlate(near, neverDerives, requiring({ ...REF_RULE, window_ms: 5_000 }))
    expect([2, 3].map((leg) => correlation.basesOf(leg))).toEqual([[1], []])
    expect(correlation.families).toEqual([[0], [1, 2], [3]])
  })
})

describe("a document not projecting a header the rule reads", () => {
  const unprojected = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, CALLER)], [])

  it("is refused under a required rule, naming the rule and the header", () => {
    expect(() => correlate(unprojected, neverDerives, requiring(LINK_RULE))).toThrow(UnprojectedHeaders)
    expect(() => correlate(unprojected, neverDerives, requiring(LINK_RULE))).toThrow(`the leg rule "linked-call" reads ${LINK_HEADER}`)
  })

  it("is correlated without a skipped rule, the correlation naming it and the header", () => {
    const correlation = correlate(unprojected, neverDerives, skipping(LINK_RULE))
    expect(correlation.families).toEqual([[0], [1]])
    expect(correlation.skipped).toEqual([{ rule: LINK_RULE.name, missing: [LINK_HEADER] }])
    expect(correlate(linked, neverDerives, skipping(LINK_RULE)).skipped, "projected: nothing skipped").toEqual([])
  })
})
