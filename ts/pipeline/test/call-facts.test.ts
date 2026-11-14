/**
 * Call facts, capture side: each fact stated on a scenario in RFC terms. The
 * scenarios are written with the scenario builder, so every fact is read off
 * datagrams and never off a leg's summary.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import * as CallFacts from "../src/call-facts.js"
import { multipart, scenario, SDP, type Scenario, type ScenarioLeg } from "../src/scenario.js"
import { type ActorObs, build } from "../src/topology.js"
import { plan } from "./fixtures.js"

const PARTY = "198.51.100.10:5060"
const CALLEE = "203.0.113.20:5060"

const facts = (s: Scenario, leg: ScenarioLeg, flows = s.flows()): CallFacts.LegFacts =>
  CallFacts.atVantage(flows, s.sut, leg.vantage())

const seenAt = (fact: CallFacts.Fact<CallFacts.Entry>): number | undefined => CallFacts.valueOf(fact)?.at_us

/** A leg the party opens, rings, has answered and closes itself. */
const answeredCall = (s: Scenario): ScenarioLeg =>
  s
    .leg({ callId: "answered@example.com", party: PARTY })
    .invite({ at: 0, by: "party", body: SDP })
    .response(100, { at: 5 })
    .response(180, { at: 20 })
    .response(200, { at: 900, body: SDP })
    .ack({ at: 910, by: "party" })
    .bye({ at: 5000, by: "party" })
    .response(200, { at: 5010 })

describe("an answered leg", () => {
  it("shows its INVITE, its first 180, its 2xx and the BYE that closed it, each with its time and sender", () => {
    const s = scenario()
    const leg = facts(s, answeredCall(s))

    expect(CallFacts.valueOf(leg.invite)).toMatchObject({ at_us: 0, bySystem: false, method: "INVITE", dialog: "opening" })
    expect(CallFacts.valueOf(leg.ringing)).toMatchObject({ at_us: 20_000, status: 180, bySystem: true })
    expect(CallFacts.valueOf(leg.final)).toMatchObject({ at_us: 900_000, status: 200 })
    expect(seenAt(leg.answer)).toBe(900_000)
    expect(CallFacts.valueOf(leg.ending)).toMatchObject({ by: "BYE", entry: { at_us: 5_000_000, bySystem: false } })
    expect(leg.crossedCancel).toBeUndefined()
  })

  it("points every entry at the message it was read from", () => {
    const s = scenario()
    const call = answeredCall(s)
    const flows = s.flows()
    const leg = facts(s, call, flows)

    const bye = CallFacts.valueOf(leg.ending)!.entry
    expect(CallFacts.source(leg.timeline, bye)).toBe(flows.legs[0]!.msgs[bye.msg])
    expect(CallFacts.source(leg.timeline, bye).summary).toMatchObject({ kind: "request", method: "BYE" })
    expect(leg.timeline.entries.map((e) => e.msg)).toEqual([0, 1, 2, 3, 4, 5, 6])
  })

  it("is closed by the system where the system sent the BYE", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "released@example.com", party: PARTY })
      .invite({ at: 0, by: "party" })
      .response(200, { at: 40 })
      .ack({ at: 50, by: "party" })
      .bye({ at: 700, by: "system" })
      .response(200, { at: 710 })

    expect(CallFacts.valueOf(facts(s, leg).ending)).toMatchObject({ by: "BYE", entry: { bySystem: true, at_us: 700_000 } })
  })

  it("is closed by the BYE captured ahead of a re-INVITE's late 2xx", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "glare@example.com", party: PARTY })
      .invite({ at: 0, by: "party" })
      .response(200, { at: 40 })
      .ack({ at: 50, by: "party" })
      .invite({ at: 600, by: "party" })
      .bye({ at: 610, by: "system" })
      .response(200, { at: 620, to: "INVITE" })
      .response(200, { at: 630, to: "BYE" })

    const read = facts(s, leg)
    expect(seenAt(read.answer)).toBe(40_000)
    expect(CallFacts.valueOf(read.ending)).toMatchObject({ by: "BYE", entry: { at_us: 610_000 } })
  })
})

describe("a leg with several opening transactions", () => {
  it("takes the retry's final as the outcome of a challenged INVITE (RFC 3261 §22.2)", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "challenged@example.com", party: CALLEE })
      .invite({ at: 0, by: "system" })
      .response(407, { at: 10 })
      .ack({ at: 15, by: "system" })
      .invite({ at: 30, by: "system", fresh: true, headers: ["Proxy-Authorization: Digest username=\"u\""] })
      .response(180, { at: 60 })
      .response(200, { at: 400 })
      .ack({ at: 410, by: "system" })
    const read = facts(s, leg)

    expect(read.openings.map((t) => [t.seq, t.final?.status])).toEqual([[1, 407], [2, 200]])
    expect(seenAt(read.invite)).toBe(0)
    expect(read.outcome?.request?.at_us).toBe(30_000)
    expect(CallFacts.valueOf(read.final)?.status).toBe(200)
    expect(seenAt(read.answer)).toBe(400_000)
    expect(seenAt(read.ringing)).toBe(60_000)
  })

  it("takes the last attempt's failure where the retry failed too", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "retried@example.com", party: CALLEE })
      .invite({ at: 0, by: "system" })
      .response(503, { at: 10 })
      .ack({ at: 15, by: "system" })
      .invite({ at: 500, by: "system", fresh: true })
      .response(486, { at: 520 })
      .ack({ at: 525, by: "system" })
    const read = facts(s, leg)

    expect(CallFacts.valueOf(read.final)).toMatchObject({ status: 486, at_us: 520_000 })
    expect(read.answer.state).toBe("absent")
    expect(read.ending.state).toBe("absent")
  })
})

describe("a re-INVITE", () => {
  it("opens no leg and its failure final is not the leg's outcome", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "renegotiated@example.com", party: PARTY })
      .invite({ at: 0, by: "party" })
      .response(200, { at: 40 })
      .ack({ at: 50, by: "party" })
      .invite({ at: 600, by: "party" })
      .response(488, { at: 620 })
      .ack({ at: 630, by: "party" })
      .bye({ at: 900, by: "party" })
      .response(200, { at: 910 })
    const read = facts(s, leg)

    expect(read.openings).toHaveLength(1)
    expect(CallFacts.valueOf(read.final)?.status).toBe(200)
    expect(read.accepted.map((a) => a.request.method)).toEqual(["BYE"])
  })

  it("sent the other way under the opening INVITE's CSeq number answers its own transaction (RFC 3261 §12.2.1.1)", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "same-cseq@example.com", party: PARTY })
      .invite({ at: 0, by: "party", seq: 1 })
      .response(200, { at: 40 })
      .ack({ at: 50, by: "party" })
      .invite({ at: 600, by: "system", seq: 1 })
      .response(491, { at: 620 })
      .ack({ at: 630, by: "system" })
    const read = facts(s, leg)

    expect(CallFacts.valueOf(read.final)).toMatchObject({ status: 200, at_us: 40_000 })
    expect(read.timeline.transactions.map((t) => [t.method, t.seq, t.bySystem, t.opening, t.final?.status])).toEqual([
      ["INVITE", 1, false, true, 200],
      ["INVITE", 1, true, false, 491]
    ])
  })

  it("accepted is listed among the accepted in-dialog requests", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "held@example.com", party: PARTY })
      .invite({ at: 0, by: "party" })
      .response(200, { at: 40 })
      .ack({ at: 50, by: "party" })
      .invite({ at: 600, by: "system", body: SDP })
      .response(200, { at: 620, body: SDP })
      .ack({ at: 630, by: "system" })
    const read = facts(s, leg)

    expect(read.accepted).toHaveLength(1)
    expect(read.accepted[0]).toMatchObject({
      request: { method: "INVITE", bySystem: true, at_us: 600_000, dialog: "confirmed" },
      response: { status: 200, at_us: 620_000 }
    })
    expect(CallFacts.mediaTypes(read.timeline, read.accepted[0]!.request)).toEqual(["application/sdp"])
  })
})

describe("a cancelled leg", () => {
  const dialled = (s: Scenario): ScenarioLeg =>
    s
      .leg({ callId: "dialled@example.com", party: CALLEE })
      .invite({ at: 0, by: "system" })
      .response(180, { at: 30 })

  it("ended on the system's CANCEL where the 487 it asks for followed (RFC 3261 §9.1)", () => {
    const s = scenario()
    const leg = dialled(s)
      .cancel({ at: 20_000, by: "system" })
      .response(200, { at: 20_010 })
      .response(487, { at: 20_020, to: "INVITE" })
      .ack({ at: 20_030, by: "system" })
    const read = facts(s, leg)

    expect(CallFacts.valueOf(read.ending)).toMatchObject({ by: "CANCEL", entry: { bySystem: true, at_us: 20_000_000 } })
    expect(CallFacts.valueOf(read.final)?.status).toBe(487)
    expect(read.answer.state).toBe("absent")
    expect(read.crossedCancel).toBeUndefined()
  })

  it("whose 2xx crossed the CANCEL is answered, and ends on the BYE that follows", () => {
    const s = scenario()
    const leg = dialled(s)
      .cancel({ at: 20_000, by: "system" })
      .response(200, { at: 20_005, to: "INVITE" })
      .response(200, { at: 20_010, to: "CANCEL" })
      .ack({ at: 20_020, by: "system" })
      .bye({ at: 20_030, by: "system" })
      .response(200, { at: 20_040 })
    const read = facts(s, leg)

    expect(seenAt(read.answer)).toBe(20_005_000)
    expect(read.crossedCancel?.at_us).toBe(20_000_000)
    expect(CallFacts.valueOf(read.ending)).toMatchObject({ by: "BYE", entry: { bySystem: true, at_us: 20_030_000 } })
  })

  it("ended on the party's CANCEL where the party opened it", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "abandoned@example.com", party: PARTY })
      .invite({ at: 0, by: "party" })
      .response(180, { at: 30 })
      .cancel({ at: 4000, by: "party" })
      .response(200, { at: 4005 })
      .response(487, { at: 4010, to: "INVITE" })
      .ack({ at: 4020, by: "party" })

    expect(CallFacts.valueOf(facts(s, leg).ending)).toMatchObject({ by: "CANCEL", entry: { bySystem: false } })
  })

  it("ended on a BYE of its early dialog where the opener sent one (RFC 3261 §15.1)", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "early-bye@example.com", party: PARTY })
      .invite({ at: 0, by: "party" })
      .response(183, { at: 30 })
      .bye({ at: 4000, by: "party" })
      .response(200, { at: 4005 })
      .response(487, { at: 4010, to: "INVITE" })
      .ack({ at: 4020, by: "party" })

    expect(CallFacts.valueOf(facts(s, leg).ending)).toMatchObject({ by: "BYE", entry: { bySystem: false, dialog: "early" } })
  })
})

describe("a leg a failure final ended", () => {
  it("was not answered and shows no BYE or CANCEL", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "busy@example.com", party: CALLEE })
      .invite({ at: 0, by: "system" })
      .response(486, { at: 40 })
      .ack({ at: 50, by: "system" })
    const read = facts(s, leg)

    expect(CallFacts.valueOf(read.final)).toMatchObject({ status: 486, bySystem: false })
    expect(read.answer.state).toBe("absent")
    expect(read.ringing.state).toBe("absent")
    expect(read.ending.state).toBe("absent")
  })
})

describe("what the capture's edges leave out", () => {
  const unanswered = (s: Scenario): ScenarioLeg =>
    s.leg({ callId: "cut@example.com", party: PARTY }).invite({ at: 0, by: "party" }).response(100, { at: 5 })

  const laterTraffic = (s: Scenario, at: number): void => {
    s.leg({ callId: "other@example.com", party: CALLEE }).request("OPTIONS", { at, by: "party" }).response(200, { at: at + 1 })
  }

  it("is not captured where the capture stops inside the transaction's own timeout", () => {
    const s = scenario()
    const leg = unanswered(s)
    laterTraffic(s, 5_000)
    const read = facts(s, leg)

    expect([read.final.state, read.ringing.state, read.answer.state, read.ending.state]).toEqual([
      "not-captured",
      "not-captured",
      "not-captured",
      "not-captured"
    ])
  })

  it("is absent where the capture runs past the transaction's timeout in silence (RFC 3261 §17.1.1.2)", () => {
    const s = scenario()
    const leg = unanswered(s)
    laterTraffic(s, 40_000)
    const read = facts(s, leg)

    expect([read.final.state, read.ringing.state, read.answer.state, read.ending.state]).toEqual([
      "absent",
      "absent",
      "absent",
      "absent"
    ])
  })

  it("leaves the BYE of an answered leg not captured however long the silence (RFC 3261 §15)", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "open@example.com", party: PARTY })
      .invite({ at: 0, by: "party" })
      .response(200, { at: 40 })
      .ack({ at: 50, by: "party" })
    laterTraffic(s, 3_600_000)

    expect(facts(s, leg).ending.state).toBe("not-captured")
  })

  it("leaves the INVITE not captured where the position opens on what answers it", () => {
    const s = scenario()
    const leg = s.leg({ callId: "headless@example.com", party: PARTY })
    leg.invite({ at: 0, by: "party" }).response(200, { at: 40 }).bye({ at: 900, by: "party" }).response(200, { at: 910 })
    const flows = s.flows()
    const headless = { ...flows, legs: [{ ...flows.legs[0]!, msgs: flows.legs[0]!.msgs.slice(1) }] }
    const read = CallFacts.atLeg(headless, s.sut, leg.index)

    expect(read.invite.state).toBe("not-captured")
    expect(read.final.state).toBe("not-captured")
    expect(read.openings).toHaveLength(0)
  })

  it("reads a document loaded in part against the legs it holds", () => {
    const s = scenario()
    laterTraffic(s, 40_000)
    const leg = unanswered(s)
    const flows = s.flows()
    const loaded: Array<(typeof flows.legs)[number]> = []
    loaded[leg.index] = flows.legs[leg.index]!

    expect(CallFacts.atVantage(flows, s.sut, leg.vantage()).final.state).toBe("absent")
    expect(CallFacts.atVantage({ ...flows, legs: loaded }, s.sut, leg.vantage()).final.state).toBe("not-captured")
  })

  it("shows no INVITE on a leg that never opens a dialog", () => {
    const s = scenario()
    const leg = s.leg({ callId: "ping@example.com", party: PARTY }).request("OPTIONS", { at: 0, by: "party" }).response(200, { at: 5 })
    const read = CallFacts.atLeg(s.flows(), s.sut, leg.index)

    expect(read.invite.state).toBe("absent")
    expect(read.timeline.entries.map((e) => e.dialog)).toEqual(["outside", "outside"])
  })
})

describe("retransmissions", () => {
  it("fold into the message they repeat and make no entry of their own", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "retransmitted@example.com", party: CALLEE })
      .invite({ at: 0, by: "system" })
      .invite({ at: 500, by: "system", retx: true })
      .invite({ at: 1500, by: "system", retx: true })
      .response(486, { at: 1600 })
      .ack({ at: 1610, by: "system" })
    const read = facts(s, leg)

    expect(read.timeline.entries.map((e) => [e.method, e.status, e.retransmissions])).toEqual([
      ["INVITE", undefined, 2],
      ["INVITE", 486, 0],
      ["ACK", undefined, 0]
    ])
    expect(read.timeline.lastAt_us).toBe(1_610_000)
  })
})

/** Each matching is stated twice: on Via branches, and on the dialog where the Via carries none. */
describe.each([
  { matchedOn: "the Via branch", branches: true },
  { matchedOn: "the dialog", branches: false }
])("a transaction matched on $matchedOn (RFC 3261 §17.1.3, §17.2.3)", ({ branches }) => {
  it("is one per fork where the system numbers a request alike in two early dialogs (§12.2.1.1)", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "forked@example.com", party: CALLEE, branches })
      .invite({ at: 0, by: "system" })
      .response(183, { at: 20, tag: "fork-a" })
      .response(183, { at: 25, tag: "fork-b" })
      .request("INFO", { at: 40, by: "system", seq: 2, fork: "fork-a" })
      .response(200, { at: 45 })
      .request("INFO", { at: 50, by: "system", seq: 2, fork: "fork-b" })
      .response(200, { at: 55 })
      .response(200, { at: 900, to: "INVITE", tag: "fork-b" })
      .ack({ at: 910, by: "system" })
    const read = facts(s, leg)

    expect(read.accepted.map((a) => [a.request.method, a.request.at_us, a.response.at_us])).toEqual([
      ["INFO", 40_000, 45_000],
      ["INFO", 50_000, 55_000]
    ])
    expect(read.openings).toHaveLength(1)
    expect(read.outcome?.responses.map((r) => [r.status, CallFacts.source(read.timeline, r).summary.to.tag])).toEqual([
      [183, "fork-a"],
      [183, "fork-b"],
      [200, "fork-b"]
    ])
    expect(CallFacts.source(read.timeline, read.outcome!.ack!).summary.to.tag).toBe("fork-b")
  })

  it("holds the ACK of a failure final and the ACK of a 2xx with the INVITE each acknowledges (§17.1.1.3, §13.2.2.4)", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "acknowledged@example.com", party: CALLEE, branches })
      .invite({ at: 0, by: "system" })
      .response(503, { at: 100 })
      .ack({ at: 105, by: "system" })
      .invite({ at: 500, by: "system", fresh: true })
      .response(200, { at: 900 })
      .ack({ at: 910, by: "system" })
    const read = facts(s, leg)

    expect(read.timeline.transactions.map((t) => [t.method, t.seq, t.final?.status, t.ack?.at_us])).toEqual([
      ["INVITE", 1, 503, 105_000],
      ["INVITE", 2, 200, 910_000]
    ])
  })

  it("points the INVITE at the CANCEL that names it, a transaction of its own (§9.1)", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "cancelled@example.com", party: CALLEE, branches })
      .invite({ at: 0, by: "system" })
      .response(180, { at: 30 })
      .cancel({ at: 4_000, by: "system" })
      .response(200, { at: 4_005 })
      .response(487, { at: 4_020, to: "INVITE" })
      .ack({ at: 4_030, by: "system" })
    const read = facts(s, leg)

    expect(read.timeline.transactions.map((t) => [t.method, t.final?.status, t.cancel?.at_us])).toEqual([
      ["INVITE", 487, 4_000_000],
      ["CANCEL", 200, undefined]
    ])
  })

  it("of each early dialog keeps its own final where two dialogs of one leg number a request alike", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "retried@example.com", party: CALLEE, branches })
      .invite({ at: 0, by: "system" })
      .response(183, { at: 20 })
      .request("UPDATE", { at: 30, by: "party", seq: 1 })
      .response(488, { at: 35 })
      .response(503, { at: 100, to: "INVITE" })
      .ack({ at: 105, by: "system" })
      .invite({ at: 500, by: "system", fresh: true })
      .response(183, { at: 520 })
      .request("UPDATE", { at: 530, by: "party", seq: 1 })
      .response(200, { at: 535 })
    const read = facts(s, leg)
    const updates = read.timeline.entries.filter((e) => e.method === "UPDATE" && e.status === undefined)

    expect(updates.map((e) => read.timeline.transactions[e.transaction]!.final?.status)).toEqual([488, 200])
    expect(read.accepted.map((a) => [a.request.method, a.request.at_us])).toEqual([["UPDATE", 530_000]])
  })

  it("keeps the two directions of a hop between two ports of one address apart, read over the whole leg", () => {
    const s = scenario()
    const leg = s
      .leg({
        callId: "one-host@example.com",
        party: "203.0.113.20:5060",
        hops: [{ party: "203.0.113.20:5070", system: "203.0.113.20:5062" }],
        branches
      })
      .invite({ at: 0, by: "system" })
      .invite({ at: 2, by: "system", hop: 1, seq: 1, fresh: true })
      .response(200, { at: 30, hop: 1 })
      .response(200, { at: 32 })
      .ack({ at: 40, by: "system" })
      .ack({ at: 42, by: "system", hop: 1 })
      .invite({ at: 2_000, by: "party", hop: 1, seq: 1 })
      .response(488, { at: 2_010, hop: 1, to: "INVITE", by: "system" })
      .ack({ at: 2_015, by: "party", hop: 1 })
    const whole = CallFacts.atLeg(s.flows(), s.sut, leg.index)
    const atSecondHop = whole.timeline.transactions.filter((t) => t.request?.hop === 1)

    expect(atSecondHop.map((t) => [t.request?.at_us, t.opening, t.final?.status, t.ack?.at_us])).toEqual([
      [2_000, true, 200, 42_000],
      [2_000_000, false, 488, 2_015_000]
    ])
  })
})

describe("a request that crosses the system twice on one Call-ID, read over its whole leg", () => {
  /** The party's INVITE at one of its addresses, then the same INVITE at its second, as a spiral brings it back. */
  const spiral = (s: Scenario, branches: boolean): ScenarioLeg =>
    s
      .leg({ callId: "spiral@example.com", party: PARTY, hops: [{ party: "198.51.100.11:5060", system: "192.0.2.10:5060" }], branches })
      .invite({ at: 0, by: "party" })
      .invite({ at: 4, by: "party", hop: 1, seq: 1, fresh: true })
      .response(486, { at: 30, hop: 1 })
      .response(486, { at: 32 })

  it("is one transaction per crossing, each with the final sent back on its branch", () => {
    const s = scenario()
    const leg = spiral(s, true)
    const read = CallFacts.atLeg(s.flows(), s.sut, leg.index)

    expect(read.openings.map((t) => [t.request?.hop, t.final?.hop, t.final?.at_us])).toEqual([
      [0, 0, 32_000],
      [1, 1, 30_000]
    ])
  })

  it("is one transaction where the Via carries no branch to tell the crossings apart", () => {
    const s = scenario()
    const leg = spiral(s, false)
    const read = CallFacts.atLeg(s.flows(), s.sut, leg.index)

    expect(read.openings.map((t) => [t.request?.hop, t.final?.at_us])).toEqual([[0, 30_000]])
  })
})

/** A response goes back to the address its request came from, from whatever port its sender picks. */
describe.each([
  { matchedOn: "the Via branch", branches: true },
  { matchedOn: "the dialog", branches: false }
])("a final sent from another port than its INVITE reached, matched on $matchedOn (RFC 3261 §18.2.2)", ({ branches }) => {
  const NEXT = "203.0.113.20:5060"
  const NEXT_FORWARDS_FROM = "203.0.113.20:5062"
  const FAR = "203.0.113.30:5060"
  const FAR_ANSWERS_FROM = "203.0.113.30:49152"

  it("answers the INVITE of a hop between two parties, read over the whole leg", () => {
    const s = scenario()
    const leg = s
      .leg({
        callId: "forwarded@example.com",
        party: NEXT,
        hops: [
          { party: FAR, system: NEXT_FORWARDS_FROM },
          { party: FAR_ANSWERS_FROM, system: NEXT_FORWARDS_FROM }
        ],
        branches
      })
      .invite({ at: 0, by: "system" })
      .invite({ at: 2, by: "system", hop: 1, seq: 1, fresh: true })
      .response(488, { at: 30, hop: 2 })
      .ack({ at: 31, by: "system", hop: 1 })
      .response(488, { at: 33 })
      .ack({ at: 34, by: "system" })
    const whole = CallFacts.atLeg(s.flows(), s.sut, leg.index)

    expect(whole.openings.map((t) => [t.request?.hop, t.final?.status, t.final?.hop, t.ack?.hop])).toEqual([
      [0, 488, 0, 0],
      [1, 488, 2, 1]
    ])
    expect(CallFacts.valueOf(whole.final)).toMatchObject({ status: 488, at_us: 30_000, hop: 2, bySystem: false })
    expect(whole.ending.state).toBe("absent")
  })

  it("answers the system's INVITE at the vantage of the hop it left on", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "refused@example.com", party: FAR, hops: [{ party: FAR_ANSWERS_FROM, system: "192.0.2.10:5060" }], branches })
      .invite({ at: 0, by: "system" })
      .response(486, { at: 30, hop: 1 })
      .ack({ at: 31, by: "system" })
    const read = facts(s, leg)

    expect(read.openings.map((t) => [t.request?.hop, t.final?.status, t.final?.hop, t.ack?.hop])).toEqual([[0, 486, 1, 0]])
    expect(CallFacts.valueOf(read.final)).toMatchObject({ status: 486, at_us: 30_000, bySystem: false })
  })
})

describe("in-dialog requests", () => {
  it("are listed with their method, body and acceptance time where a 2xx accepted them", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "in-dialog@example.com", party: PARTY })
      .invite({ at: 0, by: "party" })
      .response(183, { at: 20 })
      .request("UPDATE", { at: 30, by: "party", body: SDP })
      .response(200, { at: 35, body: SDP })
      .response(200, { at: 40, to: "INVITE" })
      .ack({ at: 50, by: "party" })
      .request("INFO", { at: 300, by: "party", body: { type: "application/dtmf-relay", text: "Signal=5\r\n" } })
      .response(200, { at: 310 })
      .request("UPDATE", { at: 400, by: "system" })
      .response(488, { at: 410 })
      .request("REFER", { at: 500, by: "party", headers: ["Refer-To: <sip:15550123@example.com>"] })
      .response(202, { at: 520 })
    const read = facts(s, leg)

    expect(
      read.accepted.map((a) => [
        a.request.method,
        a.request.dialog,
        a.response.at_us,
        CallFacts.mediaTypes(read.timeline, a.request)
      ])
    ).toEqual([
      ["UPDATE", "early", 35_000, ["application/sdp"]],
      ["INFO", "confirmed", 310_000, ["application/dtmf-relay"]],
      ["REFER", "confirmed", 520_000, []]
    ])
  })
})

describe("what the opening INVITE and its provisionals carry", () => {
  it("reads the offer, the 100rel option, a reliable provisional and one with a session description", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "reliable@example.com", party: CALLEE })
      .invite({ at: 0, by: "system", body: SDP, headers: ["Supported: timer, 100rel"] })
      .response(180, { at: 20 })
      .response(183, { at: 30, body: SDP, headers: ["Require: 100rel", "RSeq: 1"] })
      .response(200, { at: 90 })
    const read = facts(s, leg)
    const invite = CallFacts.valueOf(read.invite)!

    expect(CallFacts.carriesSession(read.timeline, invite)).toBe(true)
    expect(CallFacts.asksReliable(read.timeline, invite)).toBe(true)
    expect(seenAt(CallFacts.reliableProvisional(read))).toBe(30_000)
    expect(seenAt(CallFacts.sessionProvisional(read))).toBe(30_000)
    expect(seenAt(read.ringing)).toBe(20_000)
  })

  it("reads a session description that is one part of a multipart body (RFC 2046 §5.1)", () => {
    const s = scenario()
    const mixed = multipart(SDP, { type: "application/isup;version=itu-t92+", text: "ISUP" })
    const leg = s
      .leg({ callId: "mixed@example.com", party: CALLEE })
      .invite({ at: 0, by: "system", body: mixed })
      .response(183, { at: 30, body: multipart({ type: "text/plain", text: "ringing" }) })
      .response(183, { at: 40, body: mixed })
    const read = facts(s, leg)
    const invite = CallFacts.valueOf(read.invite)!

    expect(CallFacts.mediaTypes(read.timeline, invite)).toEqual(["multipart/mixed", "application/sdp", "application/isup"])
    expect(CallFacts.carriesSession(read.timeline, invite)).toBe(true)
    expect(seenAt(CallFacts.sessionProvisional(read))).toBe(40_000)
    const body = CallFacts.source(read.timeline, invite).body!
    expect(Buffer.from(mixed.text).subarray(body.parts![0]!.offset, body.parts![0]!.offset + body.parts![0]!.len).toString()).toBe(SDP.text)
  })

  it("reads none of them on a bare INVITE answered by a plain 180", () => {
    const s = scenario()
    const leg = s.leg({ callId: "plain@example.com", party: CALLEE }).invite({ at: 0, by: "system" }).response(180, { at: 20 }).response(200, { at: 90 })
    const read = facts(s, leg)
    const invite = CallFacts.valueOf(read.invite)!

    expect(CallFacts.carriesSession(read.timeline, invite)).toBe(false)
    expect(CallFacts.asksReliable(read.timeline, invite)).toBe(false)
    expect(CallFacts.reliableProvisional(read).state).toBe("absent")
    expect(CallFacts.sessionProvisional(read).state).toBe("absent")
  })
})

describe("a position", () => {
  /**
   * A leg of three hops: the party's own, a second address the party sends
   * its BYE from, and a hop the system forwards the call on, which the first
   * vantage does not own. The INVITE is retransmitted once.
   */
  const threeHops = (s: Scenario): ScenarioLeg =>
    s
      .leg({
        callId: "three-hops@example.com",
        party: PARTY,
        hops: [
          { party: "198.51.100.11:5060", system: "192.0.2.10:5060" },
          { party: CALLEE, system: "192.0.2.10:5062" }
        ]
      })
      .invite({ at: 0, by: "party" })
      .invite({ at: 500, by: "party", retx: true })
      .invite({ at: 510, by: "system", hop: 2, seq: 1, fresh: true })
      .response(200, { at: 600, hop: 2 })
      .response(200, { at: 610, by: "system" })
      .ack({ at: 620, by: "party" })
      .ack({ at: 625, by: "system", hop: 2 })
      .bye({ at: 900, by: "party", hop: 1 })
      .response(200, { at: 910, hop: 1 })

  const actorOf = (s: Scenario, flows: Flows.FlowsDoc, leg: ScenarioLeg): ActorObs =>
    build(flows, [leg.vantage()], s.sut, plan(), () => false).actorsObs[0]!

  it("of a layout's actor is its cut vantage's: the same messages, retransmissions and second address included", () => {
    const s = scenario()
    const leg = threeHops(s)
    const flows = s.flows()
    const actor = actorOf(s, flows, leg)

    expect(CallFacts.ofActor(flows, actor).msgIdxs).toEqual([0, 1, 4, 5, 7, 8])
    expect(CallFacts.ofActor(flows, actor).msgIdxs).toEqual(CallFacts.ofVantage(flows, s.sut, leg.vantage()).msgIdxs)
    expect(CallFacts.atVantage(flows, s.sut, leg.vantage())).toBe(CallFacts.atActor(flows, actor))
    expect(CallFacts.atActor(flows, actor).timeline.entries.map((e) => [e.msg, e.retransmissions])).toEqual([
      [0, 1],
      [4, 0],
      [5, 0],
      [7, 0],
      [8, 0]
    ])
  })

  it("of an actor that brings other messages than the facts kept for its hop were read off is refused", () => {
    const s = scenario()
    const leg = threeHops(s)
    const flows = s.flows()
    const actor = actorOf(s, flows, leg)
    CallFacts.atVantage(flows, s.sut, leg.vantage())
    const withoutRetransmissions = { ...actor, msgIdxs: actor.msgIdxs.filter((i) => !flows.legs[leg.index]!.msgs[i]!.retx) }

    expect(() => CallFacts.atActor(flows, withoutRetransmissions)).toThrow(
      "position leg 0 hop 0 brings 5 messages where its kept facts were read off 6"
    )
  })

  it("reads a party that speaks from a second address as the same party", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "two-addresses@example.com", party: PARTY, hops: [{ party: "198.51.100.11:5060", system: "192.0.2.10:5060" }] })
      .invite({ at: 0, by: "party" })
      .response(200, { at: 40 })
      .ack({ at: 50, by: "party" })
      .bye({ at: 900, by: "party", hop: 1 })
      .response(200, { at: 910, hop: 1 })
    const read = facts(s, leg)

    expect(CallFacts.valueOf(read.ending)).toMatchObject({ by: "BYE", entry: { hop: 1, bySystem: false } })
  })

  it("of a whole leg reads every hop the leg has", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "relayed@example.com", party: PARTY, hops: [{ party: CALLEE, system: "192.0.2.10:5062" }] })
      .invite({ at: 0, by: "party" })
      .invite({ at: 2, by: "system", hop: 1, seq: 1, fresh: true })
      .response(486, { at: 30, hop: 1 })
      .response(486, { at: 32, by: "system" })
    const flows = s.flows()
    const whole = CallFacts.atLeg(flows, s.sut, leg.index)

    expect(whole.timeline.entries).toHaveLength(4)
    expect(whole.openings.map((t) => t.bySystem)).toEqual([false, true])
    expect(CallFacts.atVantage(flows, s.sut, leg.vantage(0)).timeline.entries.length).toBeLessThan(4)
  })

  it("of a whole leg matches a transaction at a hop between two parties by the address its request came from", () => {
    const s = scenario()
    const next = "203.0.113.20:5060"
    const leg = s
      .leg({ callId: "forwarded@example.com", party: next, hops: [{ party: "203.0.113.30:5060", system: "203.0.113.20:5062" }] })
      .invite({ at: 0, by: "system" })
      .invite({ at: 2, by: "system", hop: 1, seq: 1, fresh: true })
      .response(180, { at: 30, hop: 1 })
      .response(180, { at: 32 })
      .cancel({ at: 4000, by: "system" })
      .response(200, { at: 4001 })
      .cancel({ at: 4002, by: "system", hop: 1 })
      .response(487, { at: 4040, hop: 1, to: "INVITE" })
      .ack({ at: 4041, by: "system", hop: 1 })
      .response(487, { at: 4042, to: "INVITE" })
      .ack({ at: 4043, by: "system" })
    const whole = CallFacts.atLeg(s.flows(), s.sut, leg.index)

    expect(whole.openings.map((t) => [t.request?.hop, t.bySystem, t.final?.status, t.final?.hop, t.cancel?.hop])).toEqual([
      [0, true, 487, 0, 0],
      [1, false, 487, 1, 1]
    ])
    expect(CallFacts.valueOf(whole.ending)).toMatchObject({ by: "CANCEL", entry: { hop: 1, at_us: 4_002_000 } })
  })
})

describe("a position that owns every message of its leg", () => {
  it("and the whole-leg position are one reading, whichever is asked first", () => {
    const s = scenario()
    const leg = answeredCall(s)
    const other = s.leg({ callId: "second@example.com", party: CALLEE }).invite({ at: 0, by: "system" }).response(486, { at: 9 })
    const flows = s.flows()
    const atHop = CallFacts.atVantage(flows, s.sut, leg.vantage())
    const paid = CallFacts.cost(flows)

    expect(CallFacts.atLeg(flows, s.sut, leg.index)).toBe(atHop)
    expect(CallFacts.cost(flows)).toEqual(paid)

    const whole = CallFacts.atLeg(flows, s.sut, other.index)
    const paidTwice = CallFacts.cost(flows)
    expect(CallFacts.atVantage(flows, s.sut, other.vantage())).toBe(whole)
    expect(CallFacts.cost(flows)).toEqual(paidTwice)
  })

  it("is not the whole-leg position where the leg has a hop it does not own", () => {
    const s = scenario()
    const leg = s
      .leg({ callId: "relayed@example.com", party: PARTY, hops: [{ party: CALLEE, system: "192.0.2.10:5062" }] })
      .invite({ at: 0, by: "party" })
      .invite({ at: 2, by: "system", hop: 1, seq: 1, fresh: true })
      .response(486, { at: 30, hop: 1 })
      .response(486, { at: 32, by: "system" })
    const flows = s.flows()

    expect(CallFacts.atVantage(flows, s.sut, leg.vantage(0))).not.toBe(CallFacts.atLeg(flows, s.sut, leg.index))
    expect(CallFacts.atLeg(flows, s.sut, leg.index).timeline.entries).toHaveLength(4)
  })
})

describe("the tags the parties minted (RFC 3261 §8.2.6.2)", () => {
  /**
   * The system dials a far end that rejects with its own To-tag, and answers
   * its caller with a To-tag of its own, or with the far end's where `relays`.
   */
  const rejected = (relays: boolean): { readonly s: Scenario; readonly caller: ScenarioLeg; readonly dialled: ScenarioLeg } => {
    const s = scenario()
    const caller = s.leg({ callId: "in@example.com", party: PARTY }).invite({ at: 0, by: "party" })
    const dialled = s
      .leg({ callId: "out@example.com", party: CALLEE })
      .invite({ at: 5, by: "system" })
      .response(486, { at: 40, tag: "far-end" })
    caller.response(486, { at: 45, ...(relays ? { tag: "far-end" } : {}) })
    return { s, caller, dialled }
  }
  const tagOf = (read: CallFacts.LegFacts): string =>
    CallFacts.source(read.timeline, CallFacts.valueOf(read.final)!).summary.to.tag ?? ""

  it("name the tag another leg's party put on its response", () => {
    const { s, caller } = rejected(true)
    const flows = s.flows()
    const tag = tagOf(CallFacts.atVantage(flows, s.sut, caller.vantage()))

    expect(tag).toBe("far-end")
    expect(CallFacts.partyTags(flows, s.sut).onAnotherLeg(tag, caller.index)).toBe(true)
  })

  it("do not hold a tag the system minted itself", () => {
    const { s, caller } = rejected(false)
    const flows = s.flows()
    const tag = tagOf(CallFacts.atVantage(flows, s.sut, caller.vantage()))

    expect(tag).not.toBe("")
    expect(CallFacts.partyTags(flows, s.sut).onAnotherLeg(tag, caller.index)).toBe(false)
  })

  it("do not name a leg's own party against that leg", () => {
    const { s, dialled } = rejected(true)
    const flows = s.flows()

    expect(CallFacts.partyTags(flows, s.sut).onAnotherLeg("far-end", dialled.index)).toBe(false)
  })

  it("name a tag two legs' parties show against either leg", () => {
    const s = scenario()
    const one = s.leg({ callId: "one@example.com", party: CALLEE }).invite({ at: 0, by: "system" }).response(486, { at: 9, tag: "shared" })
    const two = s.leg({ callId: "two@example.com", party: PARTY }).invite({ at: 1, by: "system" }).response(486, { at: 8, tag: "shared" })
    const tags = CallFacts.partyTags(s.flows(), s.sut)

    expect([tags.onAnotherLeg("shared", one.index), tags.onAnotherLeg("shared", two.index)]).toEqual([true, true])
  })
})
