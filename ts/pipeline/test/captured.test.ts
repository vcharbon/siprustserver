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
  plan,
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

const captured = (hits: ReadonlyArray<Census.CensusHit>, covered?: ReadonlySet<string>) => {
  const flows = cancelRaceFlows()
  const sut = sutSet()
  const vantages = [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }]
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
})
