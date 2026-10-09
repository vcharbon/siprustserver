/**
 * The captured violations of a case: a census hit charging a scripted party is
 * stated on the step that carries its anchor message, and nothing else is.
 */
import type { Census } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { capturedWith } from "../src/captured.js"
import { caseCallIds, correlate } from "../src/cut.js"
import { synthesize } from "../src/flowsteps.js"
import { build } from "../src/topology.js"
import {
  CALLEE_CALL_ID,
  CALLER_CALL_ID,
  cancelRaceFlows,
  derivesOnePrefix,
  doc,
  leg,
  plan,
  request,
  response,
  SOCKETS,
  sutSet
} from "./fixtures.js"

const CAPTURE = "race.pcap"

/** `no-200-after-cancel` on the callee leg's 200 to INVITE (msg 5), charged to `emitter`. */
const answeredAfterCancel = (emitter: string, leg = 1, msg = 5): Census.CensusHit => ({
  document: "race.flows.json",
  capture: CAPTURE,
  rule: "no-200-after-cancel",
  group: 0,
  leg,
  call_id: leg === 1 ? CALLEE_CALL_ID : CALLER_CALL_ID,
  emitter,
  emitter_role: "peer",
  taker: SOCKETS.sut,
  cseq: 1,
  relayed: false,
  cancel_msg: 3,
  cancel_hop: 0,
  cancel_ts_us: 1_020_000,
  response_msg: msg,
  response_hop: 0,
  response_ts_us: 1_060_000,
  status: 200,
  gap_us: 40_000
})

const captured = (
  hits: ReadonlyArray<Census.CensusHit>,
  covered?: ReadonlySet<string>,
  flows = cancelRaceFlows(),
  vantages = [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }]
) => {
  const sut = sutSet()
  const layout = build(flows, vantages, sut, plan(), derivesOnePrefix)
  const flow = synthesize(flows, layout, plan())
  const stated = capturedWith(hits, covered)({
    layout,
    capture: CAPTURE,
    callIds: caseCallIds(flows, sut, [0, 1], correlate(flows, derivesOnePrefix)),
    flows,
    sources: flow.sources,
    steps: flow.steps
  })
  return { out: stated.violations, flags: stated.flags, flow, layout }
}

describe("captured violations (§11.1)", () => {
  it("states a scripted party's hit on the step that sends its anchor", () => {
    const { out, flow, layout } = captured([answeredAfterCancel(SOCKETS.callee)])
    expect(out).toHaveLength(1)
    const step = flow.steps.find((s) => s.id === out[0]!.step)!
    expect(step.op).toBe("send")
    expect(step.msg.status).toBe(200)
    expect(out[0]!.rule).toBe("no-200-after-cancel")
    expect(out[0]!.emitter).toBe(layout.legs.find((l) => l.id === step.leg)!.actor)
  })

  it("states nothing for a hit charging the system under test's side", () => {
    expect(captured([answeredAfterCancel(SOCKETS.sut)]).out).toEqual([])
  })

  it("states nothing for another capture's hit, nor one whose anchor no step carries", () => {
    expect(captured([{ ...answeredAfterCancel(SOCKETS.callee), capture: "other.pcap" }]).out).toEqual([])
    expect(captured([answeredAfterCancel(SOCKETS.callee, 1, 99)]).out).toEqual([])
  })

  it("states a hit charging the receiver on the step that takes its anchor", () => {
    // The caller took the platform's 487 (leg 0, msg 5) and, by the census,
    // never ACKed it: the withheld ACK is charged to the expect's actor.
    const hit: Census.CensusHit = {
      document: "race.flows.json",
      capture: CAPTURE,
      rule: "unacked-invite-non-2xx-final",
      group: 0,
      leg: 0,
      call_id: CALLER_CALL_ID,
      emitter: SOCKETS.caller,
      emitter_role: "peer",
      taker: SOCKETS.sut,
      cseq: 1,
      relayed: false,
      reject_msg: 5,
      reject_hop: 0,
      reject_ts_us: 1_100_000,
      status: 487,
      invite_msg: 0,
      branch: "z9hG4bK-x",
      window_us: 40_000_000
    }
    const { out, flow, layout } = captured([hit])
    expect(out).toHaveLength(1)
    const step = flow.steps.find((s) => s.id === out[0]!.step)!
    expect(step.op).toBe("expect")
    expect(step.msg.status).toBe(487)
    expect(out[0]!.emitter).toBe(layout.legs.find((l) => l.id === step.leg)!.actor)
    // Charged to the 487's SENDER instead, the expect's actor is not the party.
    expect(captured([{ ...hit, emitter: SOCKETS.sut }]).out).toEqual([])
  })

  it("flags a capture the census does not cover, instead of an empty reading", () => {
    const uncovered = captured([], new Set(["another.pcap"]))
    expect(uncovered.out).toEqual([])
    expect(uncovered.flags.map((f) => f.kind)).toEqual(["census-uncovered"])
    expect(captured([], new Set([CAPTURE])).flags).toEqual([])
    expect(captured([]).flags).toEqual([])
  })

  it("states one entry per rule and step, however many hits name it", () => {
    const hit = answeredAfterCancel(SOCKETS.callee)
    expect(captured([hit, hit]).out).toHaveLength(1)
  })

  describe("relayed onward", () => {
    // The callee's 180 (leg 1, msg 2) carries the violation; the platform's
    // 180 to the caller (leg 0, msg 2) carries it on.
    const RULE = "payload-type-mapping-stable" as const
    const origin: Census.CensusHit = {
      document: "race.flows.json",
      capture: CAPTURE,
      rule: RULE,
      group: 0,
      leg: 1,
      call_id: CALLEE_CALL_ID,
      emitter: SOCKETS.callee,
      emitter_role: "peer",
      taker: SOCKETS.sut,
      cseq: 1,
      relayed: false,
      anchor_msg: 2
    }
    const copy: Census.CensusHit = {
      ...origin,
      leg: 0,
      call_id: CALLER_CALL_ID,
      emitter: SOCKETS.sut,
      emitter_role: "platform",
      taker: SOCKETS.caller,
      relayed: true,
      relays: { leg: 1, anchor_msg: 2, emitter: SOCKETS.callee }
    }

    it("states the platform's copy as the SUT's, carrying on the party's entry", () => {
      const { out, flow, layout } = captured([origin, copy])
      expect(out).toHaveLength(2)
      const party = out.find((v) => v.emitter !== "sut")!
      const partyStep = flow.steps.find((s) => s.id === party.step)!
      expect(partyStep.op).toBe("send")
      expect(party.emitter).toBe(layout.legs.find((l) => l.id === partyStep.leg)!.actor)
      const relayed = out.find((v) => v.emitter === "sut")!
      expect(relayed.rule).toBe(RULE)
      expect(relayed.relays).toBe(party.step)
      const copyStep = flow.steps.find((s) => s.id === relayed.step)!
      expect(copyStep.op).toBe("expect")
      expect(copyStep.msg.status).toBe(180)
      expect(copyStep.leg).not.toBe(partyStep.leg)
    })

    it("states no copy whose origin the case does not state", () => {
      expect(captured([copy]).out).toEqual([])
      const unanchored = { ...origin, anchor_msg: 99 }
      expect(captured([unanchored, { ...copy, relays: { ...copy.relays!, anchor_msg: 99 } }]).out).toEqual([])
    })
  })

  describe("a hit on a hop the case does not carry", () => {
    // The caller leg crosses an edge element: hop 0 is the far caller ↔ the
    // edge's outer side, hop 1 the edge's inner side ↔ the platform. The case
    // is cut at hop 1, so its scripted caller plays the edge.
    const FAR = "10.0.0.30:5060"
    const OUTER = "10.0.0.9:5070"
    const INNER = SOCKETS.caller
    const SUT = SOCKETS.sut
    const CALLEE = SOCKETS.callee
    const a = CALLER_CALL_ID
    const b = CALLEE_CALL_ID
    const edgeFlows = () =>
      doc(
        [
          leg(a, [{ a: FAR, b: OUTER }, { a: INNER, b: SUT }], [
            request({ callId: a, seq: 1, method: "INVITE", src: FAR, dst: OUTER, ts_ms: 0, branch: "z9hG4bK-far" }),
            request({ callId: a, seq: 1, method: "INVITE", src: INNER, dst: SUT, ts_ms: 2, hop: 1, branch: "z9hG4bK-edge", below: [{ sentBy: FAR, branch: "z9hG4bK-far" }] }),
            response({ callId: a, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: INNER, ts_ms: 100, hop: 1, toTag: "sut-tag", branch: "z9hG4bK-edge", below: [{ sentBy: FAR, branch: "z9hG4bK-far" }] }),
            response({ callId: a, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: OUTER, dst: FAR, ts_ms: 102, toTag: "sut-tag", branch: "z9hG4bK-far" }),
            request({ callId: a, seq: 2, method: "BYE", src: SUT, dst: INNER, ts_ms: 40_000, hop: 1, toTag: "sut-tag" }),
            response({ callId: a, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: INNER, dst: SUT, ts_ms: 40_005, hop: 1, toTag: "sut-tag" })
          ]),
          leg(b, [{ a: SUT, b: CALLEE }], [
            request({ callId: b, seq: 1, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 10 }),
            response({ callId: b, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 90, toTag: "callee-tag" }),
            request({ callId: b, seq: 1, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 95, toTag: "callee-tag" }),
            request({ callId: b, seq: 2, method: "BYE", src: SUT, dst: CALLEE, ts_ms: 40_001, toTag: "callee-tag" }),
            response({ callId: b, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: CALLEE, dst: SUT, ts_ms: 40_006, toTag: "callee-tag" })
          ])
        ],
        [{ legs: [0, 1] }]
      )
    // The census charges the far caller, who opened the INVITE, with the 2xx
    // it took on hop 0 and never ACKed (leg 0, msg 3).
    const unacked: Census.CensusHit = {
      document: "edge.flows.json",
      capture: CAPTURE,
      rule: "no-ack-to-dialog-creating-2xx",
      group: 0,
      leg: 0,
      call_id: a,
      emitter: FAR,
      emitter_role: "peer",
      taker: OUTER,
      cseq: 1,
      relayed: false,
      anchor_msg: 3,
      final_msg: 3,
      final_hop: 0,
      final_ts_us: 102_000,
      to_tag: "sut-tag",
      status: 200,
      retransmits: 0,
      window_us: 39_900_000,
      emitter_window_us: 39_900_000
    }
    const cut = (hits: ReadonlyArray<Census.CensusHit>) =>
      captured(hits, undefined, edgeFlows(), [{ leg: 0, hop: 1 }, { leg: 1, hop: 0 }])

    it("is stated on the carried hop's copy of its anchor, charging the party on the same side", () => {
      const { out, flow, layout } = cut([unacked])
      expect(out).toHaveLength(1)
      const step = flow.steps.find((s) => s.id === out[0]!.step)!
      expect(step.op).toBe("expect")
      expect(step.msg.status).toBe(200)
      expect(step.observed).toMatchObject({ leg: 0, msg: 2 })
      expect(out[0]!.rule).toBe("no-ack-to-dialog-creating-2xx")
      expect(out[0]!.emitter).toBe(layout.legs.find((l) => l.id === step.leg)!.actor)
    })

    it("is not stated where no carried hop holds a copy of its anchor", () => {
      const other = { ...unacked, anchor_msg: 0, final_msg: 0 }
      // The far caller's INVITE has a carried copy, but the far caller is its
      // sender and the step's party there is the edge: charged on the same
      // side, a hit naming the INVITE's TAKER finds the platform, not a party.
      expect(cut([{ ...other, emitter: OUTER }]).out).toEqual([])
    })
  })
})
