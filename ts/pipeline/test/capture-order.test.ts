/**
 * The capture-order rule, one causal inversion at a time: a leg holding a
 * message captured before a message it can only follow is refused under its
 * own token, and a leg that only LOST a message is left to the rules that
 * name the loss.
 *
 * Every fixture is the caller's dialog into the system under test, ordered as
 * a conformant exchange would be, with the one inversion the test names. The
 * rule is asked with the vantages a cut would hand it.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { CASE_RULES, type CaseRule, type RefusalInput } from "../src/case-rules.js"
import { decideCases } from "../src/caseset.js"
import type { CaseSpec } from "../src/case-spec.js"
import { TRUNCATED_AT_EDGE } from "../src/capture-edge.js"
import type { CutVantage } from "../src/cut.js"
import { policyWith } from "../src/policy.js"
import {
  CALLEE_CALL_ID,
  CALLER_CALL_ID,
  derivesOnePrefix,
  doc,
  leg,
  oneHop,
  plan,
  request,
  response,
  SOCKETS,
  sutSet
} from "./fixtures.js"

const MISORDERED = "source-capture-misordered"
const CAPTURE = "synthetic.pcap"
const { caller: CALLER, sut: SUT, callee: CALLEE } = SOCKETS
const TAG = "sut-tag"

/** The rule under test, found on the pipeline's own case-tier roster by its token. */
const rule = (): CaseRule | undefined => CASE_RULES.find((r) => r.id === MISORDERED)

/** The caller's request into the system under test. */
const req = (method: string, seq: number, ms: number, more: Partial<Parameters<typeof request>[0]> = {}) =>
  request({ callId: CALLER_CALL_ID, seq, method, src: CALLER, dst: SUT, ts_ms: ms, ...more })

/** The system under test's response to the caller. */
const res = (status: number, method: string, seq: number, ms: number, more: Partial<Parameters<typeof response>[0]> = {}) =>
  response({ callId: CALLER_CALL_ID, seq, status, reason: "Testing", cseqMethod: method, src: SUT, dst: CALLER, ts_ms: ms, ...more })

/** A request the system under test sends back up the caller's dialog: its own tag is the From. */
const sutReq = (method: string, seq: number, ms: number) =>
  request({
    callId: CALLER_CALL_ID,
    seq,
    method,
    src: SUT,
    dst: CALLER,
    ts_ms: ms,
    fromTag: TAG,
    toTag: `from-${CALLER_CALL_ID}`
  })

/** The answered call's head: INVITE, 100, 180, 200, ACK. */
const answered = (): ReadonlyArray<Flows.Msg> => [
  req("INVITE", 1, 0),
  res(100, "INVITE", 1, 5),
  res(180, "INVITE", 1, 200, { toTag: TAG }),
  res(200, "INVITE", 1, 1_000, { toTag: TAG }),
  req("ACK", 1, 1_005, { toTag: TAG })
]

/** The caller's BYE and its 200, in order, at `ms`. */
const teardown = (seq: number, ms: number): ReadonlyArray<Flows.Msg> => [
  req("BYE", seq, ms, { toTag: TAG }),
  res(200, "BYE", seq, ms + 5, { toTag: TAG })
]

const callerOnly = (msgs: ReadonlyArray<Flows.Msg>): Flows.FlowsDoc =>
  doc([leg(CALLER_CALL_ID, oneHop(CALLER, SUT), msgs)], [{ legs: [0] }])

const CALLER_VANTAGE: ReadonlyArray<CutVantage> = [{ leg: 0, hop: 0 }]

const caseInput = (
  flows: Flows.FlowsDoc,
  vantages: ReadonlyArray<CutVantage> = CALLER_VANTAGE,
  cutLegs: ReadonlyArray<number> = [...new Set(vantages.map((v) => v.leg))]
): RefusalInput => ({
  flows,
  capture: CAPTURE,
  sut: sutSet(),
  legs: cutLegs,
  callIds: cutLegs.map((l) => flows.legs[l]!.call_id),
  caseId: "case1",
  spec: { id: "case1", uac: vantages[0]!, uas: vantages.slice(1), defects: [], cutLegs }
})

/** What the rule charges, one clause per inversion; empty where it does not fire. */
const details = (flows: Flows.FlowsDoc, vantages?: ReadonlyArray<CutVantage>, cutLegs?: ReadonlyArray<number>) => {
  const found = rule()
  expect(found, `no case-tier rule mints '${MISORDERED}'`).toBeDefined()
  return found!.refuses(caseInput(flows, vantages, cutLegs))?.evidence?.map((e) => e.detail) ?? []
}

describe("a leg captured out of causal order is refused as misordered", () => {
  it("a response captured before the request it answers", () => {
    const msgs = [...answered(), res(200, "BYE", 2, 9_000, { toTag: TAG }), req("BYE", 2, 9_005, { toTag: TAG })]
    expect(details(callerOnly(msgs))).toEqual([
      "the 200 to BYE (msg 5) is captured before the BYE it answers (msg 6)"
    ])
  })

  it("a 100 Trying captured before the INVITE it answers, at the head of the leg", () => {
    const msgs = [res(100, "INVITE", 1, 0), req("INVITE", 1, 3), ...answered().slice(2), ...teardown(2, 9_000)]
    expect(details(callerOnly(msgs))).toEqual([
      "the 100 to INVITE (msg 0) is captured before the INVITE it answers (msg 1)"
    ])
  })

  it("an ACK captured before the 2xx it acknowledges", () => {
    const msgs = [
      ...answered(),
      req("INVITE", 2, 5_000, { toTag: TAG }),
      res(100, "INVITE", 2, 5_001, { toTag: TAG }),
      req("ACK", 2, 5_060, { toTag: TAG }),
      res(200, "INVITE", 2, 5_130, { toTag: TAG }),
      ...teardown(3, 9_000)
    ]
    expect(details(callerOnly(msgs))).toEqual([
      "the ACK (msg 7) is captured before the final to the INVITE it acknowledges (msg 8)"
    ])
  })

  it("an ACK captured before the non-2xx final it acknowledges", () => {
    const msgs = [
      req("INVITE", 1, 0),
      res(100, "INVITE", 1, 5),
      req("CANCEL", 1, 200),
      res(200, "CANCEL", 1, 201, { toTag: TAG }),
      req("ACK", 1, 400, { toTag: TAG, branch: `z9hG4bK-${CALLER_CALL_ID}-1-INVITE` }),
      res(487, "INVITE", 1, 460, { toTag: TAG })
    ]
    expect(details(callerOnly(msgs))).toEqual([
      "the ACK (msg 4) is captured before the final to the INVITE it acknowledges (msg 5)"
    ])
  })

  it("a PRACK captured before the reliable provisional it acknowledges", () => {
    const msgs = [
      req("INVITE", 1, 0, { headers: ["Supported: 100rel"] }),
      res(100, "INVITE", 1, 5),
      req("PRACK", 2, 300, { toTag: TAG, headers: ["RAck: 77 1 INVITE"] }),
      res(183, "INVITE", 1, 360, { toTag: TAG, headers: ["Require: 100rel", "RSeq: 77"] }),
      res(200, "PRACK", 2, 365, { toTag: TAG }),
      res(200, "INVITE", 1, 1_000, { toTag: TAG }),
      req("ACK", 1, 1_005, { toTag: TAG }),
      ...teardown(3, 9_000)
    ]
    expect(details(callerOnly(msgs))).toEqual([
      "the PRACK (msg 2) is captured before the reliable provisional its RAck names (msg 3)"
    ])
  })

  it("an in-dialog request captured before the INVITE that opens its dialog", () => {
    const msgs = [sutReq("INFO", 7, 0), ...answered(), res(200, "INFO", 7, 1_010, { fromTag: TAG, toTag: `from-${CALLER_CALL_ID}` }), ...teardown(2, 9_000)]
    expect(details(callerOnly(msgs))).toEqual([
      "the in-dialog INFO (msg 0) is captured before the INVITE that opens its dialog (msg 1)"
    ])
  })

  it("is a case-tier source rule of its own token, decided ahead of the capture edge", () => {
    const at = CASE_RULES.findIndex((r) => r.id === MISORDERED)
    expect(at).toBeGreaterThanOrEqual(0)
    expect(at).toBeLessThan(CASE_RULES.findIndex((r) => r.id === TRUNCATED_AT_EDGE))
    expect(CASE_RULES[at]!.subject).toBe("source")
    expect(CASE_RULES[at]!.disposition).toBe("untrusted")
  })
})

describe("a leg in causal order, or one that only lost a message, is not misordered", () => {
  it("a whole call in order: reliable provisional, PRACK, re-INVITE, an in-dialog request each way", () => {
    const msgs = [
      req("INVITE", 1, 0, { headers: ["Supported: 100rel"] }),
      res(100, "INVITE", 1, 5),
      res(183, "INVITE", 1, 300, { toTag: TAG, headers: ["Require: 100rel", "RSeq: 77"] }),
      req("PRACK", 2, 310, { toTag: TAG, headers: ["RAck: 77 1 INVITE"] }),
      res(200, "PRACK", 2, 315, { toTag: TAG }),
      res(200, "INVITE", 1, 1_000, { toTag: TAG }),
      req("ACK", 1, 1_005, { toTag: TAG }),
      req("INVITE", 3, 5_000, { toTag: TAG }),
      res(200, "INVITE", 3, 5_050, { toTag: TAG }),
      req("ACK", 3, 5_055, { toTag: TAG }),
      sutReq("INFO", 7, 6_000),
      res(200, "INFO", 7, 6_005, { fromTag: TAG, toTag: `from-${CALLER_CALL_ID}` }),
      ...teardown(4, 9_000)
    ]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("a response whose request the capture never holds", () => {
    const msgs = [...answered(), res(200, "BYE", 2, 9_000, { toTag: TAG })]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("an ACK whose final the capture never holds", () => {
    const msgs = [req("INVITE", 1, 0), res(100, "INVITE", 1, 5), req("ACK", 1, 1_005, { toTag: TAG }), ...teardown(2, 9_000)]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("a PRACK naming a reliable provisional the capture never holds", () => {
    const msgs = [
      req("INVITE", 1, 0, { headers: ["Supported: 100rel"] }),
      res(100, "INVITE", 1, 5),
      req("PRACK", 2, 310, { toTag: TAG, headers: ["RAck: 77 1 INVITE"] }),
      res(200, "PRACK", 2, 315, { toTag: TAG }),
      res(200, "INVITE", 1, 1_000, { toTag: TAG }),
      req("ACK", 1, 1_005, { toTag: TAG }),
      ...teardown(3, 9_000)
    ]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("a 2xx retransmitted after its ACK crossed it", () => {
    const msgs = [...answered(), res(200, "INVITE", 1, 1_500, { toTag: TAG }), ...teardown(2, 9_000)]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("a request retransmitted after its response", () => {
    const msgs = [req("INVITE", 1, 0), res(100, "INVITE", 1, 5), req("INVITE", 1, 500), ...answered().slice(2), ...teardown(2, 9_000)]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("an in-dialog request after its INVITE whose tag-minting provisional the capture lost", () => {
    const msgs = [
      req("INVITE", 1, 0),
      res(100, "INVITE", 1, 5),
      req("UPDATE", 2, 300, { toTag: TAG }),
      res(200, "UPDATE", 2, 305, { toTag: TAG }),
      res(200, "INVITE", 1, 1_000, { toTag: TAG }),
      req("ACK", 1, 1_005, { toTag: TAG }),
      ...teardown(3, 9_000)
    ]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("a re-INVITE glare: both sides re-INVITE, each draws a 491 and its ACK", () => {
    const sutRes = (status: number, method: string, seq: number, ms: number) =>
      response({
        callId: CALLER_CALL_ID,
        seq,
        status,
        reason: "Testing",
        cseqMethod: method,
        src: CALLER,
        dst: SUT,
        ts_ms: ms,
        fromTag: TAG,
        toTag: `from-${CALLER_CALL_ID}`
      })
    const invite2 = `z9hG4bK-${CALLER_CALL_ID}-2-INVITE`
    const msgs = [
      ...answered(),
      req("INVITE", 2, 5_000, { toTag: TAG }),
      sutReq("INVITE", 2, 5_001),
      res(491, "INVITE", 2, 5_002, { toTag: TAG }),
      sutRes(491, "INVITE", 2, 5_003),
      req("ACK", 2, 5_004, { toTag: TAG, branch: invite2 }),
      request({
        callId: CALLER_CALL_ID,
        seq: 2,
        method: "ACK",
        src: SUT,
        dst: CALLER,
        ts_ms: 5_005,
        fromTag: TAG,
        toTag: `from-${CALLER_CALL_ID}`,
        branch: invite2
      }),
      ...teardown(3, 9_000)
    ]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("forked 183s sharing one RSeq, each PRACKed on its own fork", () => {
    const reliable = ["RSeq: 1", "Require: 100rel"]
    const msgs = [
      req("INVITE", 1, 0, { headers: ["Supported: 100rel"] }),
      res(183, "INVITE", 1, 100, { toTag: "fork-a", headers: reliable }),
      res(183, "INVITE", 1, 110, { toTag: "fork-b", headers: reliable }),
      req("PRACK", 2, 120, { toTag: "fork-b", headers: ["RAck: 1 1 INVITE"] }),
      req("PRACK", 3, 121, { toTag: "fork-a", headers: ["RAck: 1 1 INVITE"] }),
      res(200, "PRACK", 2, 125, { toTag: "fork-b" }),
      res(200, "PRACK", 3, 126, { toTag: "fork-a" }),
      res(200, "INVITE", 1, 1_000, { toTag: "fork-b" }),
      req("ACK", 1, 1_005, { toTag: "fork-b" }),
      req("BYE", 4, 9_000, { toTag: "fork-b" }),
      res(200, "BYE", 4, 9_005, { toTag: "fork-b" })
    ]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("forked 200s, each ACKed under its own To-tag, the second dialog closed at once", () => {
    const msgs = [
      req("INVITE", 1, 0),
      res(100, "INVITE", 1, 5),
      res(200, "INVITE", 1, 1_000, { toTag: "fork-b" }),
      res(200, "INVITE", 1, 1_001, { toTag: "fork-a" }),
      req("ACK", 1, 1_002, { toTag: "fork-b", branch: "z9hG4bK-ack-fork-b" }),
      req("ACK", 1, 1_003, { toTag: "fork-a", branch: "z9hG4bK-ack-fork-a" }),
      req("BYE", 2, 1_004, { toTag: "fork-a" }),
      res(200, "BYE", 2, 1_010, { toTag: "fork-a" }),
      req("BYE", 3, 9_000, { toTag: "fork-b" }),
      res(200, "BYE", 3, 9_005, { toTag: "fork-b" })
    ]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("a CANCEL crossing the 200: the 2xx wins, the CANCEL draws a 481", () => {
    const msgs = [
      req("INVITE", 1, 0),
      res(100, "INVITE", 1, 5),
      req("CANCEL", 1, 900),
      res(200, "INVITE", 1, 901, { toTag: TAG }),
      res(481, "CANCEL", 1, 902, { toTag: TAG }),
      req("ACK", 1, 905, { toTag: TAG }),
      ...teardown(2, 9_000)
    ]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("the 487 captured before the 200 to the CANCEL that asked for it", () => {
    const msgs = [
      req("INVITE", 1, 0),
      res(100, "INVITE", 1, 5),
      req("CANCEL", 1, 900),
      res(487, "INVITE", 1, 901, { toTag: TAG }),
      res(200, "CANCEL", 1, 902, { toTag: TAG }),
      req("ACK", 1, 905, { toTag: TAG, branch: `z9hG4bK-${CALLER_CALL_ID}-1-INVITE` })
    ]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("an in-dialog request of an earlier dialog, before a later INVITE of the same Call-ID and From-tag", () => {
    const msgs = [
      req("BYE", 2, 0, { toTag: "earlier-tag" }),
      res(200, "BYE", 2, 5, { toTag: "earlier-tag" }),
      req("INVITE", 3, 100),
      res(100, "INVITE", 3, 105),
      res(200, "INVITE", 3, 1_000, { toTag: TAG }),
      req("ACK", 3, 1_005, { toTag: TAG }),
      ...teardown(4, 9_000)
    ]
    expect(details(callerOnly(msgs))).toEqual([])
  })

  it("judges only the legs the case scripts", () => {
    const misordered = [
      response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 10 }),
      request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 12 })
    ]
    const flows = doc(
      [leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [...answered(), ...teardown(2, 9_000)]), leg(CALLEE_CALL_ID, oneHop(SUT, CALLEE), misordered)],
      [{ legs: [0, 1] }]
    )
    expect(details(flows, CALLER_VANTAGE, [0, 1])).toEqual([])
    expect(details(flows, [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }])).toEqual([
      "the 100 to INVITE (msg 0) is captured before the INVITE it answers (msg 1)"
    ])
  })
})

describe("a misordered case is charged to the misordering alone", () => {
  const SPEC: CaseSpec = { id: "case1", uac: { leg: 0, hop: 0 }, uas: [{ leg: 1, hop: 0 }], defects: [], cutLegs: [0, 1] }
  const CALLEE_TAG = "callee-tag"

  /** The system under test's request to the callee. */
  const out = (method: string, seq: number, ms: number, toTag?: string) =>
    request({ callId: CALLEE_CALL_ID, seq, method, src: SUT, dst: CALLEE, ts_ms: ms, ...(toTag === undefined ? {} : { toTag }) })
  /** The callee's response to the system under test. */
  const back = (status: number, method: string, seq: number, ms: number, toTag?: string) =>
    response({ callId: CALLEE_CALL_ID, seq, status, reason: "Testing", cseqMethod: method, src: CALLEE, dst: SUT, ts_ms: ms, ...(toTag === undefined ? {} : { toTag }) })

  /** The dialled leg, in order: answered, re-INVITEd once, torn down by the system under test. */
  const dialled = (): ReadonlyArray<Flows.Msg> => [
    out("INVITE", 1, 10),
    back(100, "INVITE", 1, 12),
    back(180, "INVITE", 1, 190, CALLEE_TAG),
    back(200, "INVITE", 1, 990, CALLEE_TAG),
    out("ACK", 1, 1_010, CALLEE_TAG),
    out("INVITE", 2, 5_002, CALLEE_TAG),
    back(200, "INVITE", 2, 5_050, CALLEE_TAG),
    out("ACK", 2, 5_052, CALLEE_TAG),
    out("BYE", 3, 9_010, CALLEE_TAG),
    back(200, "BYE", 3, 9_015, CALLEE_TAG)
  ]

  const family = (caller: ReadonlyArray<Flows.Msg>): Flows.FlowsDoc =>
    doc(
      [leg(CALLER_CALL_ID, oneHop(CALLER, SUT), caller), leg(CALLEE_CALL_ID, oneHop(SUT, CALLEE), dialled())],
      [{
        legs: [0, 1],
        evidence: [{
          kind: "derived_call_id",
          strategy: 1,
          legs: [0, 1],
          prefix: "1-",
          as_socket: SUT,
          peer_socket: CALLEE,
          shared_hop: false,
          dt_us: 10_000
        }]
      }]
    )

  const decide = (caller: ReadonlyArray<Flows.Msg>, quarantine = false) =>
    decideCases({
      flows: family(caller),
      capture: CAPTURE,
      specs: [SPEC],
      sut: sutSet(),
      plan: plan(),
      policy: policyWith({ derives: derivesOnePrefix }),
      quarantine
    })

  /** The caller's re-INVITE, its ACK stamped before the 200 it acknowledges. */
  const lateAnswer = (): ReadonlyArray<Flows.Msg> => [
    ...answered(),
    req("INVITE", 2, 5_000, { toTag: TAG }),
    res(100, "INVITE", 2, 5_001, { toTag: TAG }),
    req("ACK", 2, 5_060, { toTag: TAG }),
    res(200, "INVITE", 2, 5_130, { toTag: TAG }),
    ...teardown(3, 9_000)
  ]

  /** The caller's re-INVITE, in order. */
  const inOrder = (): ReadonlyArray<Flows.Msg> => [
    ...answered(),
    req("INVITE", 2, 5_000, { toTag: TAG }),
    res(100, "INVITE", 2, 5_001, { toTag: TAG }),
    res(200, "INVITE", 2, 5_055, { toTag: TAG }),
    req("ACK", 2, 5_060, { toTag: TAG }),
    ...teardown(3, 9_000)
  ]

  it("is refused under its own token and no not-captured token for the same inversion", () => {
    expect(decide(lateAnswer()).refused.map((r) => r.reason)).toEqual([MISORDERED])
  })

  it("is refused under its own token and not as truncated at the head", () => {
    const head = [res(100, "INVITE", 1, 0), req("INVITE", 1, 3), ...inOrder().slice(2)]
    expect(decide(head).refused.map((r) => r.reason)).toEqual([MISORDERED])
  })

  it("carries no quarantined document, and says why", () => {
    const decided = decide(lateAnswer(), true)
    expect(decided.outcomes[0]!.quarantined).toBeUndefined()
    expect(decided.outcomes[0]!.quarantineError).toBe(decided.refused[0]!.line)
  })

  it("a call in order is generated", () => {
    const decided = decide(inOrder())
    expect(decided.refused).toEqual([])
    expect(decided.outcomes[0]!.built).toBeDefined()
  })
})
