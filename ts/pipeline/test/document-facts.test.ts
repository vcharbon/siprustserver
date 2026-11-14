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
import { watchedDocument } from "./counted-work.js"
import { derivesOnePrefix, plan } from "./fixtures.js"

const CALLER = "198.51.100.10:5060"
const CALLEE = "203.0.113.20:5060"
const CALLER_ID = "call@example.com"

interface Cut {
  readonly s: Scenario
  readonly caller: ScenarioLeg
  readonly dialled: ReadonlyArray<ScenarioLeg>
}

const callerLeg = (s: Scenario): ScenarioLeg =>
  s.leg({ callId: CALLER_ID, party: CALLER, from: "sip:+15556000001@example.com", to: "sip:+15556000004@example.com" })

const dialledLeg = (s: Scenario, n = 1): ScenarioLeg =>
  s.leg({
    callId: `${n}-${CALLER_ID}`,
    party: n === 1 ? CALLEE : `203.0.113.${20 + n}:5060`,
    from: "sip:+15556000001@example.com",
    to: "sip:+15556000004@example.com"
  })

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

  it("reads the document's flow once, whatever is asked of it after", () => {
    const cut = answered()
    const { flows, pivot } = assembled(cut)
    const watched = watchedDocument(pivot)

    const facts = DocumentFacts.ofDocument(watched.pivot)
    const call = facts.callOf(pivot.calls[0]!)!
    for (const leg of facts.legs.values()) DocumentFacts.release(call, leg, 500_000)
    DocumentFacts.parity(flows, cut.s.sut, [cut.caller.vantage()], watched.pivot)
    DocumentFacts.ofDocument(watched.pivot)

    expect(watched.flowReads()).toBe(1)
  })

  it("shares the reading of its steps, taken before the document existed", () => {
    const { pivot } = assembled(answered())
    const steps = [...pivot.flow] as ReadonlyArray<Flow.Step>

    const read = DocumentFacts.ofSteps(steps)
    const facts = DocumentFacts.ofDocument({ ...pivot, flow: steps })

    expect(DocumentFacts.ofSteps(steps)).toBe(read)
    expect(facts.entries).toBe(read.entries)
    expect(facts.legs).toBe(read.legs)
  })

  it("states each final an ACK discharges, a 2xx's and a failure final's alike (RFC 3261 §13.2.2.4, §17.1.1.3)", () => {
    const answeredFacts = assembled(answered()).facts
    const cancelledFacts = assembled(cancelled()).facts
    const finals = (facts: DocumentFacts.StepFacts) =>
      [...facts.legs.values()].map((leg) => [CallFacts.valueOf(leg.final)?.status, facts.acked(CallFacts.valueOf(leg.final)!.step.id)])

    expect(finals(answeredFacts)).toEqual([[200, true], [200, true]])
    expect(finals(cancelledFacts)).toEqual([[487, true], [487, true]])
  })

  it("states no ACK for a final its party never acknowledged, nor for a provisional", () => {
    const s = scenario()
    const caller = callerLeg(s).invite({ at: 0, by: "party", body: SDP }).response(180, { at: 5 }).response(404, { at: 20 })
    const { facts } = assembled({ s, caller, dialled: [] })
    const leg = documentLegOf(facts, caller)

    expect(facts.acked(CallFacts.valueOf(leg.final)!.step.id)).toBe(false)
    expect(facts.acked(CallFacts.valueOf(leg.ringing)!.step.id)).toBe(false)
  })

  it("states that the caller left its leg by its BYE, and that no party left the leg the system hung up", () => {
    const cut = answered()
    const { facts } = assembled(cut)
    const caller = documentLegOf(facts, cut.caller)

    expect(CallFacts.valueOf(caller.left)).toEqual({ by: "BYE", entry: caller.byes[0] })
    expect(documentLegOf(facts, cut.dialled[0]!).left.state).toBe("absent")
  })

  it("names the capture leg each leg was cut from, its BYEs and the INVITE that decides it", () => {
    const cut = answered()
    const { facts } = assembled(cut)
    const caller = documentLegOf(facts, cut.caller)
    const dialled = documentLegOf(facts, cut.dialled[0]!)

    expect([caller.observedLeg, dialled.observedLeg]).toEqual([cut.caller.index, cut.dialled[0]!.index])
    expect(caller.byes.map((b) => [b.method, b.status, b.bySystem])).toEqual([["BYE", undefined, false]])
    expect(dialled.byes.map((b) => [b.method, b.status, b.bySystem])).toEqual([["BYE", undefined, true]])
    expect(CallFacts.valueOf(dialled.outcome)).toBe(CallFacts.valueOf(dialled.invite))
    expect([caller.cancel.state, dialled.cancel.state]).toEqual(["absent", "absent"])
  })

  it("states the caller answered, with no failure final, and its BYE as its give-up", () => {
    const cut = answered()
    const { facts } = assembled(cut)
    const call = facts.calls[0]!
    const caller = documentLegOf(facts, cut.caller)

    expect(CallFacts.valueOf(call.answered)).toBe(CallFacts.valueOf(caller.answer))
    expect(call.failed.state).toBe("absent")
    expect(CallFacts.valueOf(call.gaveUp)).toEqual({ by: "BYE", entry: caller.byes[0] })
    expect(facts.callOf(call.call)).toBe(call)
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

  it("is each leg's CANCEL, the caller's 487 as its failure final and its CANCEL as its give-up", () => {
    const cut = cancelled()
    const { facts } = assembled(cut)
    const call = facts.calls[0]!
    const caller = documentLegOf(facts, cut.caller)
    const dialled = documentLegOf(facts, cut.dialled[0]!)

    expect([CallFacts.valueOf(caller.cancel)?.bySystem, CallFacts.valueOf(dialled.cancel)?.bySystem]).toEqual([false, true])
    expect(call.answered.state).toBe("absent")
    expect(statusOf(call.failed)).toBe(487)
    expect(CallFacts.valueOf(call.gaveUp)).toEqual({ by: "CANCEL", entry: CallFacts.valueOf(caller.cancel) })
  })

  /** A caller challenged with a 407 sends its INVITE again (RFC 3261 §22.2): the second INVITE decides the leg. */
  it("takes a leg's outcome INVITE as the one sent again after a challenge, and its first as the one that opens it", () => {
    const s = scenario()
    const caller = callerLeg(s)
      .invite({ at: 0, by: "party", body: SDP })
      .response(407, { at: 5 })
      .ack({ at: 8, by: "party" })
      .invite({ at: 50, by: "party", fresh: true, body: SDP })
      .response(404, { at: 60 })
      .ack({ at: 65, by: "party" })
    const { facts } = assembled({ s, caller, dialled: [] })
    const leg = documentLegOf(facts, caller)
    const invites = leg.entries.filter((e) => e.method === "INVITE" && e.status === undefined)

    expect(invites.length).toBe(2)
    expect([CallFacts.valueOf(leg.invite), CallFacts.valueOf(leg.outcome)]).toEqual(invites)
    expect(statusOf(facts.calls[0]!.failed)).toBe(404)
  })
})

/** The release of each dialled leg, by kind, cause and the step that explains it. */
const releasesOf = (cut: Cut, skewUs = 500_000) => {
  const { facts } = assembled(cut)
  const call = facts.calls[0]!
  return cut.dialled.map((leg) => {
    const released = DocumentFacts.release(call, documentLegOf(facts, leg), skewUs)
    return released.kind === "none"
      ? { kind: released.kind }
      : {
          kind: released.kind,
          by: released.ending.by,
          ...(released.kind === "relayed" ? { cause: released.cause, because: released.because.method, status: released.because.status } : {})
        }
  })
}

/** A hunt: the system gives the first callee up after 12 s of ringing; the second answers and hangs up. */
const hunted = (): Cut => {
  const s = scenario()
  const caller = callerLeg(s).invite({ at: 0, by: "party", body: SDP }).response(100, { at: 2 })
  const first = dialledLeg(s, 1).invite({ at: 10, by: "system", body: SDP }).response(180, { at: 200 })
  caller.response(180, { at: 210 })
  first.cancel({ at: 12_200, by: "system" }).response(200, { at: 12_205 }).response(487, { at: 12_210, to: "INVITE" }).ack({ at: 12_215, by: "system" })
  const second = dialledLeg(s, 2).invite({ at: 12_300, by: "system", body: SDP }).response(200, { at: 13_000, body: SDP }).ack({ at: 13_020, by: "system" })
  caller.response(200, { at: 13_010, body: SDP, to: "INVITE" }).ack({ at: 13_015, by: "party" })
  second.bye({ at: 20_000, by: "party" }).response(200, { at: 20_005 })
  caller.bye({ at: 20_010, by: "system" }).response(200, { at: 20_015 })
  return { s, caller, dialled: [first, second] }
}

/** Two callees dialled at once: the first answers and the system cancels the second (RFC 3261 §16.7 step 10). */
const forked = (): Cut => {
  const s = scenario()
  const caller = callerLeg(s).invite({ at: 0, by: "party", body: SDP }).response(100, { at: 2 })
  const first = dialledLeg(s, 1).invite({ at: 10, by: "system", body: SDP }).response(180, { at: 200 })
  const second = dialledLeg(s, 2).invite({ at: 12, by: "system", body: SDP }).response(180, { at: 220 })
  caller.response(180, { at: 210 })
  first.response(200, { at: 900, body: SDP, to: "INVITE" }).ack({ at: 920, by: "system" })
  caller.response(200, { at: 910, body: SDP, to: "INVITE" }).ack({ at: 915, by: "party" })
  second.cancel({ at: 930, by: "system" }).response(200, { at: 935 }).response(487, { at: 940, to: "INVITE" }).ack({ at: 945, by: "system" })
  caller.bye({ at: 5_000, by: "party" }).response(200, { at: 5_005 })
  first.bye({ at: 5_010, by: "system" }).response(200, { at: 5_015 })
  return { s, caller, dialled: [first, second] }
}

/**
 * The caller gives up while the callee rings, and the capture stamps the
 * CANCEL the system relays `earlyMs` before the caller's own.
 */
const relayedEarly = (earlyMs: number): Cut => {
  const s = scenario()
  const caller = callerLeg(s).invite({ at: 0, by: "party", body: SDP }).response(100, { at: 2 })
  const dialled = dialledLeg(s).invite({ at: 10, by: "system", body: SDP }).response(180, { at: 200 })
  caller.response(180, { at: 210 })
  dialled.cancel({ at: 3_000 - earlyMs, by: "system" }).response(200, { at: 3_005 - earlyMs })
  caller.cancel({ at: 3_000, by: "party" }).response(200, { at: 3_002 })
  dialled.response(487, { at: 3_020, to: "INVITE" }).ack({ at: 3_025, by: "system" })
  caller.response(487, { at: 3_030, to: "INVITE" }).ack({ at: 3_035, by: "party" })
  return { s, caller, dialled: [dialled] }
}

describe("whether the system releases a leg on its own, as a document states it", () => {
  it("is its own where it cancels a ringing callee and the caller did nothing", () => {
    expect(releasesOf(hunted())).toEqual([{ kind: "own", by: "CANCEL" }, { kind: "none" }])
  })

  it("is the caller's CANCEL relayed where the caller gave up first", () => {
    expect(releasesOf(cancelled())).toEqual([{ kind: "relayed", by: "CANCEL", cause: "caller-gave-up", because: "CANCEL", status: undefined }])
  })

  it("is the caller's BYE relayed where the caller hangs up an answered call", () => {
    expect(releasesOf(answered())).toEqual([{ kind: "relayed", by: "BYE", cause: "caller-gave-up", because: "BYE", status: undefined }])
  })

  it("is a fork loser's CANCEL where the caller was answered while the leg rang", () => {
    expect(releasesOf(forked())).toEqual([
      { kind: "relayed", by: "BYE", cause: "caller-gave-up", because: "BYE", status: undefined },
      { kind: "relayed", by: "CANCEL", cause: "caller-answered", because: "INVITE", status: 200 }
    ])
  })

  it("is the caller's CANCEL relayed where the capture stamps the relay before it, within the skew", () => {
    expect(releasesOf(relayedEarly(10), 500_000)).toEqual([
      { kind: "relayed", by: "CANCEL", cause: "caller-gave-up", because: "CANCEL", status: undefined }
    ])
  })

  it("is its own where the caller's CANCEL follows it by more than the skew", () => {
    expect(releasesOf(relayedEarly(10), 5_000)).toEqual([{ kind: "own", by: "CANCEL" }])
  })

  /** The system CANCELs a ringing callee at 12.2 s, the callee's 2xx crosses it, and the system ACKs and BYEs it. */
  const crossedCancel = (): Cut => {
    const s = scenario()
    const caller = callerLeg(s).invite({ at: 0, by: "party", body: SDP }).response(100, { at: 2 })
    const dialled = dialledLeg(s).invite({ at: 10, by: "system", body: SDP }).response(180, { at: 200 })
    caller.response(180, { at: 210 })
    dialled
      .cancel({ at: 12_200, by: "system" })
      .response(200, { at: 12_202, body: SDP, to: "INVITE" })
      .response(200, { at: 12_204, to: "CANCEL" })
      .ack({ at: 12_206, by: "system" })
      .bye({ at: 12_210, by: "system" })
      .response(200, { at: 12_215 })
    caller.response(480, { at: 12_220, to: "INVITE" }).ack({ at: 12_225, by: "party" })
    return { s, caller, dialled: [dialled] }
  }

  /**
   * A CANCEL that crosses the callee's 2xx cancels nothing (RFC 3261 §9.1):
   * the INVITE was answered, and the system ends the dialog it established
   * with a BYE (§15). That BYE is how the leg ends, and the release.
   */
  it("is its own BYE where its CANCEL crossed the callee's 2xx", () => {
    const cut = crossedCancel()
    const { facts } = assembled(cut)

    expect(DocumentFacts.agreedOfDocument(documentLegOf(facts, cut.dialled[0]!)).ending).toEqual({ by: "BYE", bySystem: true })
    expect(releasesOf(cut)).toEqual([{ kind: "own", by: "BYE" }])
  })

  it("gave the INVITE up on its own by that CANCEL, whatever final crossed it", () => {
    const cut = crossedCancel()
    const { facts } = assembled(cut)
    const leg = documentLegOf(facts, cut.dialled[0]!)

    expect(DocumentFacts.cancelRelease(facts.calls[0]!, leg, 500_000)).toEqual({
      kind: "own",
      ending: { by: "CANCEL", entry: CallFacts.valueOf(leg.cancel) }
    })
  })

  it("gave nothing up by a CANCEL where the caller's CANCEL drove it, or where it sent none", () => {
    const relayed = assembled(cancelled())
    const hangUp = assembled(answered())
    const leg = (facts: DocumentFacts.DocumentFacts) => [...facts.legs.values()].find((l) => CallFacts.valueOf(l.invite)?.bySystem === true)!

    expect(DocumentFacts.cancelRelease(relayed.facts.calls[0]!, leg(relayed.facts), 500_000)).toMatchObject({ kind: "relayed", cause: "caller-gave-up" })
    expect(DocumentFacts.cancelRelease(hangUp.facts.calls[0]!, leg(hangUp.facts), 500_000)).toEqual({ kind: "none" })
  })

  /**
   * The system answers the caller at once and the caller's CANCEL crosses that
   * 200: it cancels nothing (RFC 3261 §9.1) and the caller stays. The system
   * dials a callee at 5 s; the caller hangs up at 8 s, and the system CANCELs
   * the callee it rings.
   */
  const crossedByCaller = (): Cut => {
    const s = scenario()
    const caller = callerLeg(s)
      .invite({ at: 0, by: "party", body: SDP })
      .response(100, { at: 2 })
      .response(200, { at: 100, body: SDP, to: "INVITE" })
      .cancel({ at: 102, by: "party" })
      .response(200, { at: 104, to: "CANCEL" })
      .ack({ at: 110, by: "party" })
    const dialled = dialledLeg(s).invite({ at: 5_000, by: "system", body: SDP }).response(180, { at: 5_030 })
    caller.bye({ at: 8_000, by: "party" }).response(200, { at: 8_005 })
    dialled.cancel({ at: 8_010, by: "system" }).response(200, { at: 8_015 }).response(487, { at: 8_020, to: "INVITE" }).ack({ at: 8_025, by: "system" })
    return { s, caller, dialled: [dialled] }
  }

  it("is the caller's BYE relayed where the caller's CANCEL crossed its 200 and the caller stayed", () => {
    const cut = crossedByCaller()
    const { facts } = assembled(cut)
    const caller = documentLegOf(facts, cut.caller)

    expect(CallFacts.valueOf(facts.calls[0]!.gaveUp)).toEqual({ by: "BYE", entry: caller.byes[0] })
    expect(releasesOf(cut)).toEqual([{ kind: "relayed", by: "CANCEL", cause: "caller-gave-up", because: "BYE", status: undefined }])
  })

  /**
   * A caller that left before the system dialled a leg did not drive that
   * leg's CANCEL: the system dialled it knowing the caller was gone, so
   * cancelling it is its own decision. The skew allows a relay stamped early,
   * never a give-up older than the leg itself.
   */
  it("is its own where the caller gave up longer than the skew before the leg was dialled", () => {
    const s = scenario()
    const caller = callerLeg(s).invite({ at: 0, by: "party", body: SDP }).response(100, { at: 2 })
    const first = dialledLeg(s, 1).invite({ at: 10, by: "system", body: SDP }).response(180, { at: 200 })
    caller.response(180, { at: 210 }).cancel({ at: 3_000, by: "party" }).response(200, { at: 3_002 })
    first.cancel({ at: 3_010, by: "system" }).response(200, { at: 3_015 }).response(487, { at: 3_020, to: "INVITE" }).ack({ at: 3_025, by: "system" })
    const second = dialledLeg(s, 2).invite({ at: 7_000, by: "system", body: SDP }).response(180, { at: 7_200 })
    second.cancel({ at: 9_000, by: "system" }).response(200, { at: 9_005 }).response(487, { at: 9_010, to: "INVITE" }).ack({ at: 9_015, by: "system" })
    caller.response(487, { at: 9_030, to: "INVITE" }).ack({ at: 9_035, by: "party" })

    expect(releasesOf({ s, caller, dialled: [first, second] }, 500_000)).toEqual([
      { kind: "relayed", by: "CANCEL", cause: "caller-gave-up", because: "CANCEL", status: undefined },
      { kind: "own", by: "CANCEL" }
    ])
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


describe("a document held against its capture", () => {
  const vantagesOf = (cut: Cut) => [cut.caller, ...cut.dialled].map((leg) => leg.vantage())

  it.each([
    ["an answered call", answered],
    ["a cancelled call", cancelled],
    ["a refused call", refused],
    ["an abandoned call", abandoned]
  ])("agrees with it on every leg of the document assembled from %s", (_, build) => {
    const cut = build()
    const { flows, pivot, facts } = assembled(cut)

    expect(DocumentFacts.parity(flows, cut.s.sut, vantagesOf(cut), pivot)).toEqual({
      compared: facts.legs.size,
      disagreements: []
    })
    expect(facts.legs.size).toBe(vantagesOf(cut).length)
  })

  it("compares no leg whose captured message none of the vantages owns", () => {
    const cut = answered()
    const { flows, pivot } = assembled(cut)

    expect(DocumentFacts.parity(flows, cut.s.sut, [cut.caller.vantage()], pivot)).toEqual({ compared: 1, disagreements: [] })
  })

  it("names the leg whose 180 the document does not state, with both readings", () => {
    const cut = answered()
    const { flows, pivot, facts } = assembled(cut)
    const caller = documentLegOf(facts, cut.caller).leg
    const silent = {
      ...pivot,
      flow: pivot.flow.filter((node) => !(Flow.isStepNode(node) && node.leg === caller && node.msg.status === 180))
    }

    expect(DocumentFacts.parity(flows, cut.s.sut, vantagesOf(cut), silent)).toMatchObject({
      compared: 2,
      disagreements: [{ leg: caller, vantage: cut.caller.vantage(), document: { ringing: undefined }, capture: { ringing: 180 } }]
    })
  })
})
