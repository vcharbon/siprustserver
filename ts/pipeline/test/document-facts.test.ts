/**
 * Call facts, document side: what a case document states of its legs and
 * calls. Every document here is assembled by the real `assemble` from a
 * scenario's capture, and each leg's document facts are held against the
 * capture's own on the subset the two sides agree on (ADR-0061).
 */
import { Flow } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { assemble } from "../src/assemble.js"
import * as CallFacts from "../src/call-facts.js"
import * as DocumentFacts from "../src/document-facts.js"
import { policyWith } from "../src/policy.js"
import { scenario, SDP, type Scenario, type ScenarioLeg } from "../src/scenario.js"
import { derivesOnePrefix, plan } from "./fixtures.js"

const CALLER = "198.51.100.10:5060"
const CALLEE = "203.0.113.20:5060"
const CALLER_ID = "call@example.com"
const DIALLED_ID = `1-${CALLER_ID}`

interface Cut {
  readonly s: Scenario
  readonly caller: ScenarioLeg
  readonly dialled: ReadonlyArray<ScenarioLeg>
}

const callerLeg = (s: Scenario): ScenarioLeg =>
  s.leg({ callId: CALLER_ID, party: CALLER, from: "sip:+15556000001@example.com", to: "sip:+15556000004@example.com" })

const dialledLeg = (s: Scenario): ScenarioLeg =>
  s.leg({ callId: DIALLED_ID, party: CALLEE, from: "sip:+15556000001@example.com", to: "sip:+15556000004@example.com" })

/** The document `assemble` builds of the case cut at the scenario's legs, with the capture it was built from. */
const assembled = (cut: Cut) => {
  const flows = cut.s.flows()
  const legs = [cut.caller, ...cut.dialled]
  const { pivot } = assemble({
    flows,
    capture: "scenario.pcap",
    spec: {
      id: "scenario-1",
      uac: cut.caller.vantage(),
      uas: cut.dialled.map((leg) => leg.vantage()),
      defects: [],
      cutLegs: legs.map((leg) => leg.index)
    },
    sut: cut.s.sut,
    plan: plan(),
    policy: policyWith({ derives: derivesOnePrefix })
  })
  return { flows, pivot, facts: DocumentFacts.ofDocument(pivot) }
}

/** The document leg cut from a scenario leg: the one whose steps were observed on it. */
const documentLegOf = (facts: DocumentFacts.DocumentFacts, leg: ScenarioLeg): DocumentFacts.DocumentLeg =>
  [...facts.legs.values()].find((held) => held.entries.some((e) => e.step.observed?.leg === leg.index))!

/** Each leg of the case as both sides state the agreed subset: `[document, capture]`. */
const bothSides = (cut: Cut): ReadonlyArray<readonly [DocumentFacts.Agreed, DocumentFacts.Agreed]> => {
  const { flows, facts } = assembled(cut)
  return [cut.caller, ...cut.dialled].map((leg) => [
    DocumentFacts.agreedOfDocument(documentLegOf(facts, leg)),
    DocumentFacts.agreedOfCapture(CallFacts.atVantage(flows, cut.s.sut, leg.vantage()))
  ])
}

/** A call the system relays to one callee, which rings and answers; the caller hangs up. */
const answered = (): Cut => {
  const s = scenario()
  const caller = callerLeg(s).invite({ at: 0, by: "party", body: SDP }).response(100, { at: 2 })
  const dialled = dialledLeg(s)
    .invite({ at: 10, by: "system", body: SDP })
    .response(180, { at: 200 })
    .response(200, { at: 900, body: SDP })
    .ack({ at: 920, by: "system" })
  caller.response(180, { at: 210 }).response(200, { at: 910, body: SDP }).ack({ at: 915, by: "party" })
  caller.bye({ at: 5_000, by: "party" }).response(200, { at: 5_005 })
  dialled.bye({ at: 5_010, by: "system" }).response(200, { at: 5_015 })
  return { s, caller, dialled: [dialled] }
}

/** A call the caller gives up while the callee rings (RFC 3261 §9.1). */
const cancelled = (): Cut => {
  const s = scenario()
  const caller = callerLeg(s).invite({ at: 0, by: "party", body: SDP }).response(100, { at: 2 })
  const dialled = dialledLeg(s).invite({ at: 10, by: "system", body: SDP }).response(180, { at: 200 })
  caller.response(180, { at: 210 }).cancel({ at: 3_000, by: "party" }).response(200, { at: 3_002 })
  dialled.cancel({ at: 3_010, by: "system" }).response(200, { at: 3_015 }).response(487, { at: 3_020, to: "INVITE" }).ack({ at: 3_025, by: "system" })
  caller.response(487, { at: 3_030, to: "INVITE" }).ack({ at: 3_035, by: "party" })
  return { s, caller, dialled: [dialled] }
}

/** A call the system refuses on its own, dialling nobody. */
const refused = (): Cut => {
  const s = scenario()
  const caller = callerLeg(s).invite({ at: 0, by: "party", body: SDP }).response(404, { at: 20 }).ack({ at: 25, by: "party" })
  return { s, caller, dialled: [] }
}

/** A call the caller gives up before the system dials anybody. */
const abandoned = (): Cut => {
  const s = scenario()
  const caller = callerLeg(s)
    .invite({ at: 0, by: "party", body: SDP })
    .response(100, { at: 2 })
    .cancel({ at: 400, by: "party" })
    .response(200, { at: 402 })
    .response(487, { at: 405, to: "INVITE" })
    .ack({ at: 410, by: "party" })
  return { s, caller, dialled: [] }
}

const statusOf = (fact: CallFacts.Fact<DocumentFacts.StepEntry>): number | undefined => CallFacts.valueOf(fact)?.status

describe("what a document states of an answered call", () => {
  it("is each leg's INVITE and who sends it, its first 180, its 2xx, and the BYE that ends it", () => {
    const cut = answered()
    const { facts } = assembled(cut)
    const caller = documentLegOf(facts, cut.caller)
    const dialled = documentLegOf(facts, cut.dialled[0]!)

    expect(DocumentFacts.agreedOfDocument(caller)).toEqual({
      invite: "party",
      ringing: 180,
      final: 200,
      answered: true,
      ending: { by: "BYE", bySystem: false }
    })
    expect(DocumentFacts.agreedOfDocument(dialled)).toEqual({
      invite: "system",
      ringing: 180,
      final: 200,
      answered: true,
      ending: { by: "BYE", bySystem: true }
    })
  })

  it("points every entry at its step, in document order", () => {
    const { pivot, facts } = assembled(answered())

    expect(facts.entries.map((e) => e.step.id)).toEqual(pivot.flow.map((node) => node.id))
    expect(facts.entries.map((e) => e.index)).toEqual(facts.entries.map((_, i) => i))
    expect([...facts.legs.values()].flatMap((leg) => leg.entries).length).toBe(facts.entries.length)
  })

  it("names the call's caller leg and each attempt's leg with what the calls block states of it", () => {
    const cut = answered()
    const { pivot, facts } = assembled(cut)
    const call = facts.calls[0]!

    expect(call.caller).toBe(documentLegOf(facts, cut.caller))
    expect(call.attempts.map((a) => [a.leg, a.attempt.final?.status])).toEqual([[documentLegOf(facts, cut.dialled[0]!), 200]])
    expect(call.attempts[0]!.attempt).toBe(pivot.calls[0]!.attempts[0])
    expect([call.refused, call.abandoned]).toEqual([undefined, undefined])
  })

  it("is computed once per document", () => {
    const { pivot, facts } = assembled(answered())

    expect(DocumentFacts.ofDocument(pivot)).toBe(facts)
  })
})

describe("what a document states of a call that was not answered", () => {
  it("is the CANCEL that ends each leg, by the party on the caller's and by the system on the one it dialled", () => {
    const cut = cancelled()
    const { facts } = assembled(cut)

    expect(DocumentFacts.agreedOfDocument(documentLegOf(facts, cut.caller))).toEqual({
      invite: "party",
      ringing: 180,
      final: 487,
      answered: false,
      ending: { by: "CANCEL", bySystem: false }
    })
    expect(DocumentFacts.agreedOfDocument(documentLegOf(facts, cut.dialled[0]!)).ending).toEqual({ by: "CANCEL", bySystem: true })
  })

  it("is the failure final a call refused before any dial names", () => {
    const { facts } = assembled(refused())
    const call = facts.calls[0]!

    expect([call.refused?.status, call.refused?.bySystem, call.abandoned]).toEqual([404, true, undefined])
    expect(statusOf(call.caller!.final)).toBe(404)
    expect(call.caller!.ending.state).toBe("absent")
  })

  it("is the caller's CANCEL a call abandoned before any dial names", () => {
    const { facts } = assembled(abandoned())
    const call = facts.calls[0]!

    expect([call.abandoned?.method, call.abandoned?.bySystem, call.refused]).toEqual(["CANCEL", false, undefined])
    expect(CallFacts.valueOf(call.caller!.ending)?.entry).toBe(call.abandoned)
  })
})

describe("the subset a document and its capture agree on", () => {
  it.each([
    ["an answered call", answered],
    ["a cancelled call", cancelled],
    ["a refused call", refused],
    ["an abandoned call", abandoned]
  ])("holds on every leg of %s", (_, build) => {
    for (const [document, capture] of bothSides(build())) expect(document).toEqual(capture)
  })

  /**
   * A caller challenged with a 407 sends its INVITE again (RFC 3261 §22.2):
   * both sides take the leg's outcome from the second INVITE.
   */
  it("holds on a leg whose INVITE was sent again after a challenge", () => {
    const s = scenario()
    const caller = callerLeg(s)
      .invite({ at: 0, by: "party", body: SDP })
      .response(407, { at: 5 })
      .ack({ at: 8, by: "party" })
      .invite({ at: 50, by: "party", fresh: true, body: SDP })
      .response(100, { at: 52 })
    const dialled = dialledLeg(s)
      .invite({ at: 60, by: "system", body: SDP })
      .response(200, { at: 900, body: SDP })
      .ack({ at: 920, by: "system" })
    caller.response(200, { at: 910, body: SDP, to: "INVITE" }).ack({ at: 915, by: "party" })
    caller.bye({ at: 5_000, by: "party" }).response(200, { at: 5_005 })
    dialled.bye({ at: 5_010, by: "system" }).response(200, { at: 5_015 })
    const sides = bothSides({ s, caller, dialled: [dialled] })

    expect(sides[0]![0]).toMatchObject({ final: 200, answered: true, ringing: undefined })
    for (const [document, capture] of sides) expect(document).toEqual(capture)
  })

  /** A re-INVITE refused 488 inside the answered dialog is no outcome of the leg on either side. */
  it("holds on a leg whose re-INVITE was refused", () => {
    const s = scenario()
    const caller = callerLeg(s).invite({ at: 0, by: "party", body: SDP }).response(100, { at: 2 })
    const dialled = dialledLeg(s)
      .invite({ at: 10, by: "system", body: SDP })
      .response(200, { at: 900, body: SDP })
      .ack({ at: 920, by: "system" })
    caller.response(200, { at: 910, body: SDP }).ack({ at: 915, by: "party" })
    caller.invite({ at: 2_000, by: "party", body: SDP }).response(488, { at: 2_010 }).ack({ at: 2_015, by: "party" })
    caller.bye({ at: 5_000, by: "party" }).response(200, { at: 5_005 })
    dialled.bye({ at: 5_010, by: "system" }).response(200, { at: 5_015 })
    const sides = bothSides({ s, caller, dialled: [dialled] })

    expect(sides[0]![0]).toMatchObject({ final: 200, answered: true, ending: { by: "BYE", bySystem: false } })
    for (const [document, capture] of sides) expect(document).toEqual(capture)
  })
})


describe("the disagreements of a document with its capture", () => {
  const vantagesOf = (cut: Cut) => [cut.caller, ...cut.dialled].map((leg) => leg.vantage())

  it.each([
    ["an answered call", answered],
    ["a cancelled call", cancelled],
    ["a refused call", refused],
    ["an abandoned call", abandoned]
  ])("are none on the document assembled from %s", (_, build) => {
    const cut = build()
    const { flows, pivot } = assembled(cut)

    expect(DocumentFacts.disagreements(flows, cut.s.sut, vantagesOf(cut), pivot)).toEqual([])
  })

  it("name the leg whose 180 the document does not state, with both readings", () => {
    const cut = answered()
    const { flows, pivot, facts } = assembled(cut)
    const caller = documentLegOf(facts, cut.caller).leg
    const silent = {
      ...pivot,
      flow: pivot.flow.filter((node) => !(Flow.isStepNode(node) && node.leg === caller && node.msg.status === 180))
    }

    expect(DocumentFacts.disagreements(flows, cut.s.sut, vantagesOf(cut), silent)).toMatchObject([
      { leg: caller, vantage: cut.caller.vantage(), document: { ringing: undefined }, capture: { ringing: 180 } }
    ])
  })
})
