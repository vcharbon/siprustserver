/**
 * The capture-edge rule, one termination shape at a time: what counts as a
 * leg terminated inside the capture, and what the file cut through.
 *
 * Every fixture puts the capture's end on a CLOCK leg of its own — an OPTIONS
 * ping answered at `endMs` — so the distance between a call's last message and
 * the end of the capture is the test's to set, and the call under test is never
 * the one deciding where the file ends.
 *
 * Every test asks the rule itself, with the vantages a cut would hand it: the
 * rule judges the legs those vantages script, and reads which of them the
 * system under test dialled.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import {
  CAPTURE_EDGE_RULE,
  captureEndUs,
  GIVE_UP_SILENCE_US,
  RETRANSMISSION_TAIL_US,
  TRUNCATED_AT_EDGE
} from "../src/capture-edge.js"
import { CAPTURE_RULES } from "../src/capture-rules.js"
import { CASE_RULES } from "../src/case-rules.js"
import type { CutVantage } from "../src/cut.js"
import { SutSet } from "../src/sut.js"
import {
  BASE_CALL_ID,
  completenessFlows,
  DERIVED_CALL_ID,
  dialogMsgs,
  doc,
  FAR,
  leg,
  msg,
  SUT_EGRESS,
  SUT_INGRESS,
  SUT_IP,
  UPSTREAM,
  type DialogEnd
} from "./cutfixtures.js"

const TAIL_MS = RETRANSMISSION_TAIL_US / 1000
const SILENCE_MS = GIVE_UP_SILENCE_US / 1000

const CAPTURE = "synthetic.pcap.gz"
const sut = new SutSet([SUT_IP])

const CLOCK_CALL_ID = "clock-0001@198.51.100.4"
/** A third Call-ID of the family, one derivation further down. */
const TWICE_DERIVED_CALL_ID = `1-${DERIVED_CALL_ID}`

/** The leg that ends the capture: an OPTIONS the SUT answered at `endMs`. */
const clockLeg = (endMs: number): Flows.Leg =>
  leg(CLOCK_CALL_ID, [{ a: UPSTREAM, b: SUT_INGRESS }], [
    msg({ callId: CLOCK_CALL_ID, seq: 1, method: "OPTIONS", src: UPSTREAM, dst: SUT_INGRESS, hop: 0, ts_ms: endMs - 5 }),
    msg({ callId: CLOCK_CALL_ID, seq: 1, method: "OPTIONS", status: 200, src: SUT_INGRESS, dst: UPSTREAM, hop: 0, ts_ms: endMs })
  ])

/** The caller's dialog into the SUT, opened at 0 and ending as `end` says. */
const callerMsgs = (end: DialogEnd, at_ms = 0): ReadonlyArray<Flows.Msg> =>
  dialogMsgs({ callId: BASE_CALL_ID, hop: 0, from: UPSTREAM, to: SUT_INGRESS, at_ms, toTag: "callee", end })

const callerLeg = (msgs: ReadonlyArray<Flows.Msg>): Flows.Leg =>
  leg(BASE_CALL_ID, [{ a: UPSTREAM, b: SUT_INGRESS }], msgs)

/** The SUT's dialled leg out to the far end, on the derived Call-ID. */
const dialledLeg = (msgs: ReadonlyArray<Flows.Msg>): Flows.Leg =>
  leg(DERIVED_CALL_ID, [{ a: SUT_EGRESS, b: FAR }], msgs)

const dialledMsgs = (end: DialogEnd): ReadonlyArray<Flows.Msg> =>
  dialogMsgs({ callId: DERIVED_CALL_ID, hop: 0, from: SUT_EGRESS, to: FAR, at_ms: 10, toTag: "far", end })

/** One family — the caller leg alone — beside the clock leg, in one document. */
const family = (msgs: ReadonlyArray<Flows.Msg>, endMs: number): Flows.FlowsDoc =>
  doc([callerLeg(msgs), clockLeg(endMs)], [{ legs: [0] }, { legs: [1] }])

/** The caller into the SUT and the SUT's derived b-leg out, as one family. */
const twoLegs = (caller: DialogEnd, derived: DialogEnd, endMs: number): Flows.FlowsDoc =>
  doc([callerLeg(callerMsgs(caller)), dialledLeg(dialledMsgs(derived)), clockLeg(endMs)], [{ legs: [0, 1] }, { legs: [2] }])

const req = (method: string, seq: number, ms: number, tag?: string): Flows.Msg =>
  msg({ callId: BASE_CALL_ID, seq, method, src: UPSTREAM, dst: SUT_INGRESS, hop: 0, ts_ms: ms, toTag: tag })
const res = (status: number, method: string, seq: number, ms: number, tag = "callee"): Flows.Msg =>
  msg({ callId: BASE_CALL_ID, seq, method, status, src: SUT_INGRESS, dst: UPSTREAM, hop: 0, ts_ms: ms, toTag: tag })

/** The caller's vantage alone: the family of {@link family}. */
const CALLER: ReadonlyArray<CutVantage> = [{ leg: 0, hop: 0 }]
/** The caller's vantage and the SUT's dialled one: the family of {@link twoLegs}. */
const BOTH: ReadonlyArray<CutVantage> = [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }]

/**
 * The case the rule is asked about: `vantages` script it, and `cutLegs` is
 * every leg the cut selected for it, scripted or not.
 */
const caseInput = (
  flows: Flows.FlowsDoc,
  vantages: ReadonlyArray<CutVantage>,
  cutLegs: ReadonlyArray<number> = [...new Set(vantages.map((v) => v.leg))]
) => ({
  flows,
  capture: CAPTURE,
  sut,
  legs: cutLegs,
  callIds: cutLegs.map((l) => flows.legs[l]!.call_id),
  caseId: "case1",
  spec: { id: "case1", uac: vantages[0]!, uas: vantages.slice(1), defects: [], cutLegs }
})

const edge = (flows: Flows.FlowsDoc, vantages: ReadonlyArray<CutVantage> = CALLER, cutLegs?: ReadonlyArray<number>) =>
  CAPTURE_EDGE_RULE.refuses(caseInput(flows, vantages, cutLegs))

const details = (
  flows: Flows.FlowsDoc,
  vantages: ReadonlyArray<CutVantage> = CALLER,
  cutLegs?: ReadonlyArray<number>
): ReadonlyArray<string> => edge(flows, vantages, cutLegs)?.evidence?.map((e) => e.detail) ?? []

describe("the capture edge: a leg terminated inside the capture", () => {
  it("a BYE that drew its final, whatever the distance to the end", () => {
    // The end of the capture is the 200 to the BYE itself, as a per-call export ends.
    const msgs = callerMsgs("torn-down")
    expect(details(doc([callerLeg(msgs)], [{ legs: [0] }]))).toEqual([])
    expect(details(family(msgs, 5_005))).toEqual([])
    expect(details(family(msgs, 5_006))).toEqual([])
  })

  it("a non-2xx final with its ACK", () => {
    expect(details(family(callerMsgs("rejected"), 305))).toEqual([])
  })

  it("a CANCEL, its 487 and the ACK", () => {
    expect(details(family(callerMsgs("cancelled"), 415))).toEqual([])
  })

  it("a retransmitted 2xx after the BYE re-opens nothing", () => {
    expect(details(family(callerMsgs("retransmitted-after-bye"), 5_100))).toEqual([])
  })

  it("an INVITE nothing answered, once the give-up silence has passed", () => {
    // The 100 at 5 ms is the leg's last message.
    expect(details(family(callerMsgs("unanswered"), 5 + SILENCE_MS))).toEqual([])
    expect(details(family(callerMsgs("silent"), SILENCE_MS))).toEqual([])
  })

  it("a CANCEL nothing finalled, once the give-up silence has passed", () => {
    const msgs = [req("INVITE", 1, 0), res(180, "INVITE", 1, 200), req("CANCEL", 1, 400), res(200, "CANCEL", 1, 405)]
    expect(details(family(msgs, 405 + SILENCE_MS))).toEqual([])
  })

  it("a BYE nothing answered, once the retransmission tail has passed", () => {
    const msgs = [...callerMsgs("left-open"), req("BYE", 2, 5_000, "callee")]
    expect(details(family(msgs, 5_000 + TAIL_MS))).toEqual([])
  })

  it("a BYE nothing answered and never retransmitted, however close to the end", () => {
    // A per-call export that ends on the BYE it relayed: the BYE is the capture's last datagram.
    const msgs = [...callerMsgs("left-open"), req("BYE", 2, 5_000, "callee")]
    expect(details(family(msgs, 5_000 + TAIL_MS - 1))).toEqual([])
    expect(details(doc([callerLeg(msgs)], [{ legs: [0] }]))).toEqual([])
  })

  it("a BYE glare: each side's BYE crossed the other's, neither answered nor retransmitted", () => {
    const callee = (ms: number): Flows.Msg =>
      msg({ callId: BASE_CALL_ID, seq: 7, method: "BYE", src: SUT_INGRESS, dst: UPSTREAM, hop: 0, ts_ms: ms, toTag: "callee" })
    const msgs = [...callerMsgs("left-open"), req("BYE", 2, 5_000, "callee"), callee(5_001)]
    expect(details(family(msgs, 5_010))).toEqual([])
  })

  it("a non-2xx final nobody ACKed, whatever the distance to the end", () => {
    const msgs = [req("INVITE", 1, 0), res(486, "INVITE", 1, 300)]
    expect(details(family(msgs, 300 + SILENCE_MS - 1))).toEqual([])
    expect(details(family(msgs, 300 + SILENCE_MS))).toEqual([])
    expect(details(doc([callerLeg(msgs)], [{ legs: [0] }]))).toEqual([])
  })

  it("a non-INVITE leg whose last request drew its final", () => {
    const options = [req("OPTIONS", 1, 0), res(200, "OPTIONS", 1, 5)]
    expect(details(family(options, 5))).toEqual([])
  })
})

describe("the capture edge: a leg the file cut through", () => {
  it("a dialog answered and never closed, however long the silence", () => {
    const found = details(family(callerMsgs("left-open"), 905 + 10 * SILENCE_MS))
    expect(found).toHaveLength(1)
    expect(found[0]).toMatch(/took the 200 \(msg 1\) answering its INVITE \(msg 0\) and no BYE closed the dialog: dialog-left-open/)
  })

  it("a CANCEL the 2xx beat, with no BYE after it", () => {
    expect(details(family(callerMsgs("cancelled-but-answered"), 415 + SILENCE_MS))[0]).toMatch(/dialog-left-open/)
  })

  it("a second attempt answered and left up after the first was torn down", () => {
    expect(details(family(callerMsgs("re-answered-after-teardown"), 6_905 + SILENCE_MS))[0]).toMatch(
      /took the 200 \(msg 6\) answering its INVITE \(msg 5\) and no BYE.*dialog-left-open/
    )
  })

  it("an INVITE nothing answered, inside the give-up silence", () => {
    const found = details(family(callerMsgs("unanswered"), 5 + SILENCE_MS - 1))
    expect(found).toHaveLength(1)
    expect(found[0]).toMatch(/its INVITE \(msg 0\) drew no final, and its last message \(msg 1\) is 31\.999 s before the capture ends: gave-up-at-edge/)
  })

  it("a CANCEL nothing finalled, inside the give-up silence", () => {
    const msgs = [req("INVITE", 1, 0), res(180, "INVITE", 1, 200), req("CANCEL", 1, 400), res(200, "CANCEL", 1, 405)]
    expect(details(family(msgs, 405 + SILENCE_MS - 1))[0]).toMatch(
      /its INVITE \(msg 0\) was cancelled \(msg 2\) and drew no final.*gave-up-at-edge/
    )
  })

  it("a BYE still being retried at the end of the capture", () => {
    // The retransmission at 8 s is the last message; the file ends 1 s after it.
    const msgs = [...callerMsgs("left-open"), req("BYE", 2, 5_000, "callee"), { ...req("BYE", 2, 8_000, "callee"), retx: true }]
    const found = details(family(msgs, 9_000))
    expect(found).toHaveLength(1)
    expect(found[0]).toMatch(/its BYE \(msg 3\) drew no final and was retransmitted.*1\.000 s before the capture ends: bye-unanswered-at-edge/)
    expect(details(family(msgs, 8_000 + TAIL_MS))).toEqual([])
  })

  it("a challenged BYE whose authenticated retry is still being retransmitted", () => {
    // The 407 finals the first BYE's transaction, not the retry's.
    const msgs = [
      ...callerMsgs("left-open"),
      req("BYE", 2, 5_000, "callee"),
      res(407, "BYE", 2, 5_005),
      req("BYE", 3, 5_010, "callee"),
      { ...req("BYE", 3, 5_510, "callee"), retx: true },
      { ...req("BYE", 3, 6_510, "callee"), retx: true }
    ]
    const found = details(family(msgs, 7_000))
    expect(found).toHaveLength(1)
    expect(found[0]).toMatch(/its BYE \(msg 5\) drew no final and was retransmitted.*bye-unanswered-at-edge/)
  })

  it("a fresh 2xx to the attempt, whatever non-2xx another branch answered after it", () => {
    // RFC 3261 §16.7: a 2xx forwarded on one branch establishes the dialog however the others end.
    const msgs = [req("INVITE", 1, 0), res(200, "INVITE", 1, 300, "a"), req("ACK", 1, 305, "a"), res(486, "INVITE", 1, 310, "b")]
    const found = details(family(msgs, 310 + 10 * SILENCE_MS))
    expect(found).toHaveLength(1)
    expect(found[0]).toMatch(/took the 200 \(msg 1\) answering its INVITE \(msg 0\) and no BYE closed the dialog: dialog-left-open/)
  })

  it("an INVITE retried after a non-2xx and never answered, inside the give-up silence", () => {
    const msgs = [req("INVITE", 1, 0), res(486, "INVITE", 1, 300), req("INVITE", 2, 400)]
    const found = details(family(msgs, 1_000))
    expect(found).toHaveLength(1)
    expect(found[0]).toMatch(/its INVITE \(msg 2\) drew no final.*gave-up-at-edge/)
  })

  it("a non-INVITE leg whose last request drew no final, inside the give-up silence", () => {
    const options = [req("OPTIONS", 1, 0)]
    expect(details(family(options, SILENCE_MS - 1))[0]).toMatch(/its OPTIONS \(msg 0\) drew no final.*gave-up-at-edge/)
  })

  it("a leg opening on a response: the request it answers precedes the capture", () => {
    // Every message after the lost INVITE: the 200, the ACK, the BYE and its 200.
    const msgs = [res(200, "INVITE", 1, 0), req("ACK", 1, 5, "callee"), req("BYE", 2, 5_000, "callee"), res(200, "BYE", 2, 5_005)]
    const found = details(family(msgs, 5_005 + SILENCE_MS))
    expect(found).toHaveLength(1)
    expect(found[0]).toMatch(/opens on the 200 to INVITE \(msg 0\): head-truncated, the request it answers precedes the capture/)
  })

  it("a leg opening on an in-dialog request: the dialog began before the capture", () => {
    const msgs = [req("BYE", 2, 0, "callee"), res(200, "BYE", 2, 5)]
    const found = details(family(msgs, 5 + SILENCE_MS))
    expect(found).toHaveLength(1)
    expect(found[0]).toMatch(/opens on an in-dialog BYE \(msg 0\): head-truncated, the dialog began before the capture/)
  })

  it("a leg cut at both edges carries both findings", () => {
    const msgs = [res(200, "INVITE", 1, 0), req("ACK", 1, 5, "callee")]
    expect(details(family(msgs, 10_000))).toEqual([
      expect.stringMatching(/head-truncated/),
      expect.stringMatching(/took the 200 \(msg 0\) answering the INVITE before the capture and no BYE.*dialog-left-open/)
    ])
  })
})

describe("the capture edge: a leg the system under test dialled", () => {
  it("is exempt at its tail once every caller-side dialog was closed by a BYE", () => {
    expect(edge(twoLegs("torn-down", "left-open", 20_000), BOTH)).toBeUndefined()
    // Whatever the dialled leg holds: its teardown is the system under test's own at replay.
    expect(edge(twoLegs("torn-down", "unanswered", 5_100), BOTH)).toBeUndefined()
  })

  it("is judged where a caller-side dialog was left open", () => {
    const finding = edge(twoLegs("left-open", "left-open", 20_000), BOTH)
    expect(finding?.legs).toEqual([0, 1])
    expect(finding?.line).toBe(
      `${CAPTURE}: case 'case1' EXCLUDED ${TRUNCATED_AT_EDGE} — ` +
        `leg 0 on '${BASE_CALL_ID}' took the 200 (msg 1) answering its INVITE (msg 0) and no BYE closed the ` +
        `dialog: dialog-left-open; ` +
        `leg 1 on '${DERIVED_CALL_ID}' took the 200 (msg 1) answering its INVITE (msg 0) and no BYE closed the ` +
        `dialog: dialog-left-open`
    )
  })

  it("is judged where one caller-side dialog of several failed, so no BYE closed it", () => {
    const SECOND_CALLER_CALL_ID = "second-caller-0001@198.51.100.4"
    const flows = doc(
      [
        callerLeg(callerMsgs("torn-down")),
        leg(SECOND_CALLER_CALL_ID, [{ a: UPSTREAM, b: SUT_INGRESS }], dialogMsgs({
          callId: SECOND_CALLER_CALL_ID, hop: 0, from: UPSTREAM, to: SUT_INGRESS, at_ms: 5, toTag: "second", end: "rejected"
        })),
        dialledLeg(dialledMsgs("left-open")),
        clockLeg(20_000)
      ],
      [{ legs: [0, 1, 2] }, { legs: [3] }]
    )
    const finding = edge(flows, [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }, { leg: 2, hop: 0 }])
    expect(finding?.legs).toEqual([2])
    expect(finding?.evidence?.[0]?.detail).toMatch(/dialog-left-open/)
  })

  it("is judged where the caller-side dialog was cancelled and the dialled leg's CANCEL drew no final", () => {
    const dialled = [
      ...dialledMsgs("unanswered").slice(0, 1),
      msg({ callId: DERIVED_CALL_ID, seq: 1, method: "INVITE", status: 180, src: FAR, dst: SUT_EGRESS, hop: 0, ts_ms: 100, toTag: "far" }),
      msg({ callId: DERIVED_CALL_ID, seq: 1, method: "CANCEL", src: SUT_EGRESS, dst: FAR, hop: 0, ts_ms: 410 }),
      msg({ callId: DERIVED_CALL_ID, seq: 1, method: "CANCEL", status: 200, src: FAR, dst: SUT_EGRESS, hop: 0, ts_ms: 415 })
    ]
    const flows = doc([callerLeg(callerMsgs("cancelled")), dialledLeg(dialled), clockLeg(2_000)], [{ legs: [0, 1] }, { legs: [2] }])
    const finding = edge(flows, BOTH)
    expect(finding?.legs).toEqual([1])
    expect(finding?.evidence?.[0]?.detail).toMatch(/was cancelled \(msg 2\) and drew no final.*gave-up-at-edge/)
  })

  it("is judged where the SUT dialled it after the BYE that closed the caller-side dialog", () => {
    // The caller's BYE at 5 s and its 200; the SUT dials out at 6 s and the file ends at 6.1 s.
    const dialled = [msg({ callId: DERIVED_CALL_ID, seq: 1, method: "INVITE", src: SUT_EGRESS, dst: FAR, hop: 0, ts_ms: 6_000 })]
    const flows = doc([callerLeg(callerMsgs("torn-down")), dialledLeg(dialled), clockLeg(6_100)], [{ legs: [0, 1] }, { legs: [2] }])
    const finding = edge(flows, BOTH)
    expect(finding?.legs).toEqual([1])
    expect(finding?.evidence?.[0]?.detail).toMatch(/its INVITE \(msg 0\) drew no final.*gave-up-at-edge/)
  })

  it("is no exemption for a leg the case holds only dialled vantages of", () => {
    const finding = edge(twoLegs("torn-down", "left-open", 20_000), [{ leg: 1, hop: 0 }])
    expect(finding?.legs).toEqual([1])
  })

  it("is no exemption for a leg that is also a caller-side one: the SUT relayed it on one Call-ID", () => {
    // Hop 0 is the caller's dialog into the SUT, hop 1 the SUT's own out to the far end. The
    // caller's BYE closed hop 0 and is retried at the edge.
    const msgs = [
      req("INVITE", 1, 0),
      msg({ callId: BASE_CALL_ID, seq: 1, method: "INVITE", src: SUT_EGRESS, dst: FAR, hop: 1, ts_ms: 10 }),
      msg({ callId: BASE_CALL_ID, seq: 1, method: "INVITE", status: 200, src: FAR, dst: SUT_EGRESS, hop: 1, ts_ms: 800, toTag: "callee" }),
      res(200, "INVITE", 1, 900),
      req("ACK", 1, 905, "callee"),
      req("BYE", 2, 5_000, "callee"),
      { ...req("BYE", 2, 5_500, "callee"), retx: true }
    ]
    const relayed = leg(BASE_CALL_ID, [{ a: UPSTREAM, b: SUT_INGRESS }, { a: SUT_EGRESS, b: FAR }], msgs)
    const flows = doc([relayed, clockLeg(6_000)], [{ legs: [0] }, { legs: [1] }])
    const found = details(flows, [{ leg: 0, hop: 0 }, { leg: 0, hop: 1 }])
    expect(found).toHaveLength(1)
    expect(found[0]).toMatch(/its BYE \(msg 5\) drew no final and was retransmitted.*bye-unanswered-at-edge/)
  })

  it("is no exemption for a second caller-side dialog the capture left open", () => {
    const flows = completenessFlows({ caller: "torn-down", derived: "left-open", derivedArrives: true })
    expect(edge(flows, BOTH)?.legs).toEqual([1])
  })

  it("keeps its head: a dialled leg that opens mid-dialog is refused at the head", () => {
    const dialled = [
      msg({ callId: DERIVED_CALL_ID, seq: 2, method: "BYE", src: SUT_EGRESS, dst: FAR, hop: 0, ts_ms: 10, toTag: "far" }),
      msg({ callId: DERIVED_CALL_ID, seq: 2, method: "BYE", status: 200, src: FAR, dst: SUT_EGRESS, hop: 0, ts_ms: 15, toTag: "far" }),
      ...dialogMsgs({ callId: DERIVED_CALL_ID, hop: 0, from: SUT_EGRESS, to: FAR, at_ms: 20, toTag: "far-2", end: "left-open" })
    ]
    const flows = doc([callerLeg(callerMsgs("torn-down")), dialledLeg(dialled), clockLeg(20_000)], [{ legs: [0, 1] }, { legs: [2] }])
    expect(details(flows, BOTH)).toEqual([
      "opens on an in-dialog BYE (msg 0): head-truncated, the dialog began before the capture"
    ])
  })
})

describe("the capture edge: the legs the case scripts", () => {
  /**
   * The caller, the SUT's dialled leg, and a third leg of the same Call-ID
   * family the cut selected but no vantage scripts: it opens mid-dialog, long
   * before the call under test.
   */
  const withUnscripted = (caller: ReadonlyArray<Flows.Msg>): Flows.FlowsDoc =>
    doc(
      [
        callerLeg(caller),
        dialledLeg(dialledMsgs("torn-down")),
        leg(TWICE_DERIVED_CALL_ID, [{ a: SUT_EGRESS, b: FAR }], [
          msg({ callId: TWICE_DERIVED_CALL_ID, seq: 7, method: "INVITE", src: SUT_EGRESS, dst: FAR, hop: 0, ts_ms: 1, toTag: "old" }),
          msg({ callId: TWICE_DERIVED_CALL_ID, seq: 7, method: "INVITE", status: 200, src: FAR, dst: SUT_EGRESS, hop: 0, ts_ms: 2, toTag: "old" })
        ]),
        clockLeg(20_000)
      ],
      [{ legs: [0, 1, 2] }, { legs: [3] }]
    )

  it("leaves a family leg no vantage scripts unjudged", () => {
    expect(edge(withUnscripted(callerMsgs("torn-down")), BOTH, [0, 1, 2])).toBeUndefined()
  })

  it("judges a scripted leg whatever the unscripted legs beside it", () => {
    const opensOnAnswer = [res(200, "INVITE", 1, 0), req("ACK", 1, 5, "callee"), req("BYE", 2, 5_000, "callee"), res(200, "BYE", 2, 5_005)]
    const finding = edge(withUnscripted(opensOnAnswer), BOTH, [0, 1, 2])
    expect(finding?.legs).toEqual([0])
    expect(finding?.evidence?.map((e) => e.detail)).toEqual([
      "opens on the 200 to INVITE (msg 0): head-truncated, the request it answers precedes the capture"
    ])
  })
})

describe("the capture edge: the rule", () => {
  it("a family every leg of which is terminated is no finding", () => {
    expect(edge(twoLegs("torn-down", "torn-down", 20_000), BOTH)).toBeUndefined()
    expect(edge(twoLegs("rejected", "rejected", 20_000), BOTH)).toBeUndefined()
  })

  it("the end of the capture is the last SIP datagram of the whole document, whatever the family", () => {
    const flows = twoLegs("torn-down", "torn-down", 123_456)
    expect(captureEndUs(flows)).toBe(123_456_000)
    // Read off the legs the case does not script: the clock leg is not in it.
    expect(edge(twoLegs("unanswered", "torn-down", 30_000), BOTH)?.legs).toEqual([0])
  })

  it("is on the pipeline's case-tier roster under its own token, as a source refusal", () => {
    expect(CAPTURE_RULES.map((r) => r.id)).not.toContain(TRUNCATED_AT_EDGE)
    const rule = CASE_RULES.find((r) => r.id === TRUNCATED_AT_EDGE)
    expect(rule?.subject).toBe("source")
    expect(rule?.disposition).toBe("refuses")
    expect(rule?.refuses(caseInput(twoLegs("left-open", "left-open", 20_000), BOTH))?.legs).toEqual([0, 1])
  })
})
