/**
 * The capture-edge rule, one termination shape at a time: what counts as a
 * leg terminated inside the capture, and what the file cut through.
 *
 * Every fixture puts the capture's end on a CLOCK leg of its own — an OPTIONS
 * ping answered at `endMs` — so the distance between a call's last message and
 * the end of the capture is the test's to set, and the call under test is never
 * the one deciding where the file ends.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import {
  captureEndUs,
  edgeEvidence,
  edgeFinding,
  GIVE_UP_SILENCE_US,
  RETRANSMISSION_TAIL_US,
  TRUNCATED_AT_EDGE
} from "../src/capture-edge.js"
import { CAPTURE_RULES } from "../src/capture-rules.js"
import { SutSet } from "../src/sut.js"
import {
  BASE_CALL_ID,
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

const CLOCK_CALL_ID = "clock-0001@198.51.100.4"

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

/** One family — the caller leg alone — beside the clock leg, in one document. */
const family = (msgs: ReadonlyArray<Flows.Msg>, endMs: number): Flows.FlowsDoc =>
  doc([callerLeg(msgs), clockLeg(endMs)], [{ legs: [0] }, { legs: [1] }])

const req = (method: string, seq: number, ms: number, tag?: string): Flows.Msg =>
  msg({ callId: BASE_CALL_ID, seq, method, src: UPSTREAM, dst: SUT_INGRESS, hop: 0, ts_ms: ms, toTag: tag })
const res = (status: number, method: string, seq: number, ms: number, tag = "callee"): Flows.Msg =>
  msg({ callId: BASE_CALL_ID, seq, method, status, src: SUT_INGRESS, dst: UPSTREAM, hop: 0, ts_ms: ms, toTag: tag })

const details = (flows: Flows.FlowsDoc, legs: ReadonlyArray<number> = [0]): ReadonlyArray<string> =>
  edgeEvidence(flows, legs).map((e) => e.detail)

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

  it("a non-2xx final nobody ACKed, inside the give-up silence", () => {
    const msgs = [req("INVITE", 1, 0), res(486, "INVITE", 1, 300)]
    expect(details(family(msgs, 300 + SILENCE_MS - 1))[0]).toMatch(/its 486 to INVITE \(msg 1\) was never ACKed.*gave-up-at-edge/)
    expect(details(family(msgs, 300 + SILENCE_MS))).toEqual([])
  })

  it("a BYE nothing answered, inside the retransmission tail", () => {
    const msgs = [...callerMsgs("left-open"), req("BYE", 2, 5_000, "callee")]
    const found = details(family(msgs, 5_000 + TAIL_MS - 1))
    expect(found).toHaveLength(1)
    expect(found[0]).toMatch(/its BYE \(msg 3\) drew no final.*3\.999 s before the capture ends: bye-unanswered-at-edge/)
  })

  it("a BYE still being retried at the end of the capture", () => {
    // The retransmission at 8 s is the last message; the file ends 1 s after it.
    const msgs = [...callerMsgs("left-open"), req("BYE", 2, 5_000, "callee"), { ...req("BYE", 2, 8_000, "callee"), retx: true }]
    expect(details(family(msgs, 9_000))[0]).toMatch(/bye-unanswered-at-edge/)
    expect(details(family(msgs, 8_000 + TAIL_MS))).toEqual([])
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

describe("the capture edge: the family and the rule", () => {
  /** The caller into the SUT and the SUT's derived b-leg out, as one family. */
  const twoLegs = (caller: DialogEnd, derived: DialogEnd, endMs: number): Flows.FlowsDoc =>
    doc(
      [
        callerLeg(callerMsgs(caller)),
        leg(DERIVED_CALL_ID, [{ a: SUT_EGRESS, b: FAR }], dialogMsgs({
          callId: DERIVED_CALL_ID, hop: 0, from: SUT_EGRESS, to: FAR, at_ms: 10, toTag: "far", end: derived
        })),
        clockLeg(endMs)
      ],
      [{ legs: [0, 1] }, { legs: [2] }]
    )

  const input = (flows: Flows.FlowsDoc, legs: ReadonlyArray<number>) => ({
    flows,
    capture: "synthetic.pcap.gz",
    sut: new SutSet([SUT_IP]),
    legs,
    callIds: legs.map((l) => flows.legs[l]!.call_id),
    caseId: "case1"
  })

  it("every leg is judged: a b-leg left open refuses the family the a-leg finished", () => {
    const finding = edgeFinding(input(twoLegs("torn-down", "left-open", 20_000), [0, 1]))
    expect(finding?.legs).toEqual([1])
    expect(finding?.callIds).toEqual([DERIVED_CALL_ID])
    expect(finding?.line).toBe(
      `synthetic.pcap.gz: case 'case1' EXCLUDED ${TRUNCATED_AT_EDGE} — leg 1 on '${DERIVED_CALL_ID}' ` +
        `took the 200 (msg 1) answering its INVITE (msg 0) and no BYE closed the dialog: dialog-left-open`
    )
  })

  it("a family every leg of which is terminated is no finding", () => {
    expect(edgeFinding(input(twoLegs("torn-down", "torn-down", 20_000), [0, 1]))).toBeUndefined()
    expect(edgeFinding(input(twoLegs("rejected", "rejected", 20_000), [0, 1]))).toBeUndefined()
  })

  it("the end of the capture is the last SIP datagram of the whole document, whatever the family", () => {
    const flows = twoLegs("torn-down", "torn-down", 123_456)
    expect(captureEndUs(flows)).toBe(123_456_000)
    // Read off the legs the family does not hold: the clock leg is not in it.
    expect(edgeFinding(input(twoLegs("unanswered", "torn-down", 30_000), [0, 1]))?.legs).toEqual([0])
  })

  it("is on the capture-tier roster under its own token, as a source refusal", () => {
    const rule = CAPTURE_RULES.find((r) => r.id === TRUNCATED_AT_EDGE)
    expect(rule?.subject).toBe("source")
    expect(rule?.disposition).toBe("refuses")
    expect(rule?.refuses(input(twoLegs("torn-down", "left-open", 20_000), [0, 1]))?.legs).toEqual([1])
  })
})
