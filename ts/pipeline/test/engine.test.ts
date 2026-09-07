/**
 * The correlation engine: rules in, joins / chains / ambiguities out — and no
 * deployment knowledge anywhere in between.
 */
import type { Rules } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { callsOf } from "../src/call.js"
import { correlate } from "../src/engine.js"
import { cancelRaceFlows, rerouteFlows } from "./fixtures.js"

const CALL_ID_RULE: Rules.Rule = {
  kind: "call-id",
  name: "b2bua-one-prefix",
  left: "^(?<key>.+)$",
  right: "^1-(?<key>.+)$"
}

const RETRY_RULE: Rules.Rule = {
  kind: "retry",
  name: "busy-hunt",
  finals: ["486", "5xx"],
  window_ms: 5_000,
  match: ["from-user"]
}

describe("the engine's input unit", () => {
  it("is one call per group, carrying the extractor's own verdicts", () => {
    const calls = callsOf(rerouteFlows())
    expect(calls.map((c) => c.id)).toEqual(["g0", "g1"])
    expect(calls[0]!.final).toEqual({ status: 486, ts_ms: 400 })
    expect(calls[0]!.invite?.ruriUser).toBe("33600000004")
    expect(calls[1]!.invite?.ruriUser).toBe("33600000005")
  })

  it("keeps a group with no INVITE and no REFER out entirely", () => {
    const flows = rerouteFlows()
    const withEmpty = { ...flows, groups: [...flows.groups, { legs: [], evidence: [], t0_us: 0 }] }
    expect(callsOf(withEmpty).map((c) => c.id)).toEqual(["g0", "g1"])
  })
})

describe("call-id joins", () => {
  it("relates two calls whose Call-IDs match left regex against right", () => {
    const calls = callsOf(
      // One group per leg, so the joiner has two calls to relate rather than one.
      { ...cancelRaceFlows(), groups: [
        { legs: [0], evidence: [], t0_us: 0, initial_invite: { leg: 0, msg: 0 } },
        { legs: [1], evidence: [], t0_us: 10_000, initial_invite: { leg: 1, msg: 0 } }
      ] }
    )
    const out = correlate(calls, [CALL_ID_RULE])
    expect(out.joins).toHaveLength(1)
    expect(out.joins[0]!.rule).toBe("b2bua-one-prefix")
    expect(out.joins[0]!.left).toBe("g0")
    expect(out.joins[0]!.right).toBe("g1")
    expect(out.groups).toHaveLength(1)
    expect(out.groups[0]!.calls).toEqual(["g0", "g1"])
  })

  it("relates nothing when no rule fires, and every call is its own group", () => {
    const calls = callsOf(rerouteFlows())
    const out = correlate(calls, [CALL_ID_RULE])
    expect(out.joins).toEqual([])
    expect(out.groups.map((g) => g.calls)).toEqual([["g0"], ["g1"]])
  })
})

describe("retry chains", () => {
  it("pairs a qualifying final with the NEXT attempt and positions the ladder", () => {
    const calls = callsOf(rerouteFlows())
    const out = correlate(calls, [RETRY_RULE])
    expect(out.joins).toHaveLength(1)
    expect(out.joins[0]!.kind).toBe("retry")
    expect(out.chains).toHaveLength(1)
    expect(out.chains[0]!.calls).toEqual([
      { call: "g0", position: 0, final_status: 486 },
      { call: "g1", position: 1, final_status: 200 }
    ])
    expect(out.joins[0]!.evidence.from_position).toBe(0)
    expect(out.joins[0]!.evidence.to_position).toBe(1)
  })

  it("does not reach past its own window", () => {
    const calls = callsOf(rerouteFlows())
    const narrow: Rules.Rule = { ...RETRY_RULE, window_ms: 100 }
    expect(correlate(calls, [narrow]).joins).toEqual([])
  })

  it("does not fire on a final the rule does not name", () => {
    const calls = callsOf(rerouteFlows())
    const other: Rules.Rule = { ...RETRY_RULE, finals: ["408"] }
    expect(correlate(calls, [other]).joins).toEqual([])
  })

  it("needs the identity sets to intersect where the rule states a match", () => {
    const flows = rerouteFlows()
    // Give the second attempt a caller nothing shares, and the two calls are no
    // longer relatable: they are in different groups, so only `match` could join
    // them and it no longer does.
    const stranger = {
      ...flows,
      legs: [
        flows.legs[0]!,
        {
          ...flows.legs[1]!,
          msgs: flows.legs[1]!.msgs.map((m) => ({
            ...m,
            identities: { ...m.identities, from: { uri: "sip:stranger@x", user: "stranger", digits: null } }
          }))
        }
      ]
    }
    expect(correlate(callsOf(stranger), [RETRY_RULE]).joins).toEqual([])
  })
})

describe("ambiguity", () => {
  it("keeps every candidate and flags them all, because the engine never picks", () => {
    const flows = rerouteFlows()
    // A third attempt starting at the same instant as the second: two calls are
    // equally "next" after the 486.
    const third = {
      ...flows,
      legs: [...flows.legs, { ...flows.legs[1]!, call_id: "attempt-3-call-id" }],
      groups: [
        ...flows.groups,
        {
          legs: [2],
          evidence: [],
          t0_us: 1_300_000,
          initial_invite: { leg: 2, msg: 0 },
          final_status: 200,
          final_us: 2_000_000
        }
      ]
    }
    const out = correlate(callsOf(third), [RETRY_RULE])
    expect(out.joins).toHaveLength(2)
    expect(out.joins.every((j) => j.ambiguous)).toBe(true)
    expect(out.ambiguities).toHaveLength(1)
    expect(out.ambiguities[0]!.rule).toBe("busy-hunt")
    expect(out.ambiguities[0]!.call).toBe("g0")
    expect(out.ambiguities[0]!.role).toBe("left")
    expect(out.ambiguities[0]!.candidates.map((c) => c.other).sort()).toEqual(["g1", "g2"])
  })
})
