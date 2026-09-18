/**
 * Flow synthesis: every captured message becomes a step, an automatic is marked
 * rather than dropped, a retransmission collapses onto the step it repeats, and
 * `in_dialog` / `confirms_dialog` are stamped totally.
 */
import { describe, expect, it } from "vitest"
import { synthesize, type HeaderClassifier } from "../src/flowsteps.js"
import { build } from "../src/topology.js"
import type { CallIdDerivation } from "../src/derivation.js"
import type { Flows } from "@sip/contracts"
import type { Vantage } from "../src/selection.js"
import {
  CALLEE_CALL_ID,
  CALLER_CALL_ID,
  delayedOfferFlows,
  derivesOnePrefix,
  doc,
  leg,
  oneHop,
  plan,
  reAckedFinalFlows,
  reInviteFlows,
  reInviteOverUnackedFinalFlows,
  request,
  response,
  SOCKETS,
  sutSet,
  twoForksAnsweredFlows,
  withoutRepeatOf
} from "./fixtures.js"

const flowOf = (
  flows: Flows.FlowsDoc,
  vantages: ReadonlyArray<Vantage>,
  derives: CallIdDerivation = derivesOnePrefix,
  headerClass?: HeaderClassifier
) => synthesize(flows, build(flows, vantages, sutSet(), plan(), derives), plan(), headerClass)

const CALLER_ONLY: ReadonlyArray<Vantage> = [{ leg: 0, hop: 0 }]

describe("every captured message is a step", () => {
  it("keeps one step per non-repeated message, in capture order", () => {
    const flows = reInviteFlows()
    const flow = flowOf(flows, CALLER_ONLY)
    expect(flow.steps).toHaveLength(flows.legs[0]!.msgs.length)
    expect(flow.steps.map((s) => s.id)).toEqual(flow.steps.map((_, i) => `s${i + 1}`))
    expect(flow.sources.map((s) => s.msgIdx)).toEqual([0, 1, 2, 3, 4, 5, 6, 7, 8, 9])
  })

  it("marks a stack-owned automatic instead of dropping it", () => {
    const flow = flowOf(reInviteFlows(), CALLER_ONLY)
    // The 100 Trying and both ACKs are the stack's, and they are still steps.
    const autos = flow.steps.filter((s) => s.auto === true)
    expect(autos.map((s) => s.id)).toEqual(["s2", "s5", "s8"])
    // An automatic expect is only ever RECORDED: its content is the stack's
    // composition, not something this document may hold a lane to.
    expect(flow.steps[1]!.check).toBe("record")
  })

  // Issue 76: `auto` marks who COMPOSES the message, never what the document
  // may hold. The closed field list left an ACK's frozen headers and the
  // delayed offer's answer with no home at all.
  it("stores a transaction-derived step's content like any other step's", () => {
    const flow = flowOf(delayedOfferFlows(), CALLER_ONLY)
    const [confirming, answering, refused] = flow.steps.filter(
      (s) => (s.msg.method ?? "").toUpperCase() === "ACK"
    )
    expect(confirming?.auto).toBe(true)
    expect(confirming?.msg.headers).toEqual([{ name: "P-Charging-Vector", value: "icid-value=abc" }])
    // The captured CSeq stays: it labels the transaction that obliged the ACK.
    expect(confirming?.msg.cseq).toBe(1)
    // The delayed offer's ANSWER rides the ACK to the 2xx (RFC 3261 §13.2.1).
    expect(answering?.msg.body).toMatchObject({ ref: expect.stringContaining(".sdp") })
    expect(flow.resources.map((r) => r.relPath)).toContain(
      (answering!.msg.body as { ref: string }).ref
    )
    // The ACK to the 488 belongs to the INVITE transaction (§17.1.1.3) and
    // reaches no TU that could read a body, so nothing stores one.
    expect(refused?.msg.body).toBeUndefined()
    expect(flow.flags.map((f) => f.kind)).toContain("automatic-body-dropped")
  })

  it("keeps a step's observed coordinate pointing back at the message", () => {
    const flow = flowOf(reInviteFlows(), CALLER_ONLY)
    expect(flow.steps[0]!.observed).toEqual({ leg: 0, msg: 0, at_us: 0 })
    expect(flow.steps[3]!.observed).toEqual({ leg: 0, msg: 3, at_us: 1_000_000 })
  })
})

describe("retransmissions", () => {
  it("collapses a ladder onto the step it repeats, as a count", () => {
    const flows = reAckedFinalFlows()
    const flow = flowOf(flows, CALLER_ONLY)
    // 11 captured messages, four of them repeats: 7 steps.
    expect(flow.steps).toHaveLength(7)
    const ok = flow.steps.find((s) => s.msg.status === 200 && s.msg["cseq-method"] === "INVITE")
    expect(ok?.retransmits).toBe(2)
  })

  it("keeps the ladder's own gaps beside the count, one per rung (§6.9)", () => {
    const flow = flowOf(reAckedFinalFlows(), CALLER_ONLY)
    const ok = flow.steps.find((s) => s.msg.status === 200 && s.msg["cseq-method"] === "INVITE")
    // The capture put its copies at 1 000 / 1 500 / 2 500 ms. T1-doubling would
    // say 500 / 1 000, and the second rung would land a second early.
    expect(ok?.retransmits).toBe(2)
    expect(ok?.retransmit_intervals_ms).toEqual([500, 1_000])
  })

  it("measures each gap from the emission before it, not from the ladder's head", () => {
    const flow = flowOf(reAckedFinalFlows(), CALLER_ONLY)
    const ack = flow.steps.find((s) => (s.msg.method ?? "").toUpperCase() === "ACK")
    // 2 600 / 2 601 / 2 602 ms — 1 ms apart each, not 1 ms and 2 ms.
    expect(ack?.retransmit_intervals_ms).toEqual([1, 1])
  })

  it("states no gaps on a step with no ladder", () => {
    const flow = flowOf(reAckedFinalFlows(), CALLER_ONLY)
    const ringing = flow.steps.find((s) => s.msg.status === 180)
    expect(ringing?.retransmit_intervals_ms).toBeUndefined()
  })

  it("collapses a fresh-branch re-ACK too, because repeat_of says so", () => {
    const flow = flowOf(reAckedFinalFlows(), CALLER_ONLY)
    const acks = flow.steps.filter((s) => (s.msg.method ?? "").toUpperCase() === "ACK")
    expect(acks).toHaveLength(1)
    expect(acks[0]!.retransmits).toBe(2)
  })

  it("falls back to retx alone on a producer that states no repeat_of, and SAYS so", () => {
    const flow = flowOf(withoutRepeatOf(reAckedFinalFlows()), CALLER_ONLY)
    expect(flow.flags.map((f) => f.kind)).toContain("retransmit-collapse-legacy")
    // The re-ACKs carry a fresh branch, so `retx` never saw them: they stay
    // separate steps, which is exactly what the flag warns about.
    const acks = flow.steps.filter((s) => (s.msg.method ?? "").toUpperCase() === "ACK")
    expect(acks).toHaveLength(3)
  })
})

describe("a repeat nothing paces stays its own step (§6.9)", () => {
  const RELIABLE = ["Require: 100rel", "RSeq: 1900416859"]

  it("keeps the repeated unreliable 180 as a step on each leg, with its own coordinate", () => {
    const flow = flowOf(withRepeated180(relayedB2bFlows()), BOTH_VANTAGES)
    const ringing = flow.steps.filter((s) => s.msg.status === 180)
    expect(ringing.map((s) => `${s.leg}${s.op}`)).toEqual(["Bsend", "Aexpect", "Bsend", "Aexpect"])
    expect(ringing.map((s) => s.observed?.msg)).toEqual([2, 2, 3, 3])
    expect(ringing.every((s) => s.retransmits === undefined)).toBe(true)
  })

  it("keeps the measured interval the count would have destroyed", () => {
    const flow = flowOf(withRepeated180(relayedB2bFlows()), BOTH_VANTAGES)
    const [first, relay] = flow.steps.filter((s) => s.msg.status === 180 && s.observed?.msg === 3)
    // The callee's re-emission dwells the half second the capture measured,
    // anchored on the 180 it repeats; the SUT's relay of it rides that step.
    const origin = flow.steps.find((s) => s.leg === "B" && s.msg.status === 180)!
    expect(first!.delay).toMatchObject({ ms: 500, from: `step:${origin.id}` })
    expect(relay!.delay).toMatchObject({ ms: 0, from: `step:${first!.id}` })
  })

  it("says what it did not collapse", () => {
    const flow = flowOf(withRepeated180(relayedB2bFlows()), BOTH_VANTAGES)
    const flag = flow.flags.find((f) => f.kind === "unreliable-provisional-repeat-expanded")!
    expect(flag.detail).toContain("B leg1/msg3")
    expect(flag.detail).toContain("A leg0/msg3")
  })

  it("collapses a RELIABLE provisional's repeat, which RFC 3262 §3 paces", () => {
    const flow = flowOf(withRepeated180(relayedB2bFlows(RELIABLE)), BOTH_VANTAGES)
    const ringing = flow.steps.filter((s) => s.msg.status === 180)
    expect(ringing).toHaveLength(2)
    expect(ringing.every((s) => s.retransmits === 1)).toBe(true)
    expect(flow.flags.map((f) => f.kind)).not.toContain("unreliable-provisional-repeat-expanded")
  })
})

/**
 * A relayed B2B call at BOTH vantages: the b-leg INVITE and BYE follow their
 * a-leg counterparts within the relay window — content-propagated AND
 * SUT-minted — while the 200 to the caller's BYE precedes the callee's and has
 * no captured origin at all.
 */
const relayedB2bFlows = (
  on180: ReadonlyArray<string> = [],
  onInvite: ReadonlyArray<string> = []
): Flows.FlowsDoc => {
  const { caller, callee, sut } = SOCKETS
  const pcv = "P-Charging-Vector: icid-value=abc123"
  const pai = "P-Asserted-Identity: <sip:+33600000004@10.0.0.1>"
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: "sut-tag", headers: [pcv, pai, ...on180] }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_005, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: 2_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 2_005, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(sut, callee), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10, headers: [pcv, ...onInvite] }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 190, toTag: "callee-tag", headers: [pcv, pai, ...on180] }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 990, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_010, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: 2_010, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 2_015, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

/**
 * The 180 repeated on BOTH legs: the callee re-emits its own, and the SUT
 * relays that. One event, two datagrams, half a second after the first pair.
 */
const withRepeated180 = (flows: Flows.FlowsDoc): Flows.FlowsDoc => ({
  ...flows,
  legs: flows.legs.map((l) => ({
    ...l,
    msgs: [
      ...l.msgs.slice(0, 3),
      { ...l.msgs[2]!, ts_us: l.msgs[2]!.ts_us + 500_000, retx: true, repeat_of: 2 },
      ...l.msgs.slice(3)
    ]
  }))
})

const BOTH_VANTAGES: ReadonlyArray<Vantage> = [
  { leg: 0, hop: 0 },
  { leg: 1, hop: 0 }
]

describe("assert vs record (§6.4)", () => {
  it("records every expect on the SUT-initiated leg, whatever the capture order", () => {
    const flow = flowOf(relayedB2bFlows(), BOTH_VANTAGES)
    // The b-leg INVITE follows the a-leg's by 10 ms: content-propagated, and
    // still the SUT's own message. Same for the relayed BYE.
    const bExpects = flow.steps.filter((s) => s.leg === "B" && s.op === "expect")
    expect(bExpects.length).toBeGreaterThanOrEqual(2)
    expect(bExpects.every((s) => s.check === "record")).toBe(true)
  })

  it("asserts relayed content on the caller's leg and records SUT-originated content", () => {
    const flow = flowOf(relayedB2bFlows(), BOTH_VANTAGES)
    const aExpect = (pick: (s: (typeof flow.steps)[number]) => boolean) =>
      flow.steps.find((s) => s.leg === "A" && s.op === "expect" && pick(s))!
    // The 180 and the 200 relay the callee's own answers.
    expect(aExpect((s) => s.msg.status === 180).check).toBe("assert")
    expect(aExpect((s) => s.msg.status === 200 && s.msg["cseq-method"] === "INVITE").check).toBe("assert")
    // The 200 to the caller's BYE precedes the callee's: no captured origin.
    expect(aExpect((s) => s.msg["cseq-method"] === "BYE").check).toBe("record")
  })
})

describe("stack-owned RSeq (§8)", () => {
  const reliable = ["Require: 100rel", "RSeq: 1900416859"]

  it("freezes the RSeq value on the scripted send, existence-only on the expect", () => {
    const flow = flowOf(relayedB2bFlows(reliable), BOTH_VANTAGES)
    const b180 = flow.steps.find((s) => s.leg === "B" && s.op === "send" && s.msg.status === 180)!
    expect(b180.msg.headers?.find((h) => h.name === "RSeq")?.value).toBe("1900416859")
    const a180 = flow.steps.find((s) => s.leg === "A" && s.op === "expect" && s.msg.status === 180)!
    expect(a180.msg.headers?.some((h) => h.name === "RSeq")).toBe(false)
    expect(a180.msg["headers-present"]).toContain("rseq")
    // Require relays verbatim and still gates — reliability detection keeps it.
    expect(a180.msg.headers?.find((h) => h.name === "Require")?.value).toBe("100rel")
  })

  it("states no existence check for a session timer the replaying stack may not run", () => {
    // Existence gates on every lane, so it states an OBLIGATION. RFC 4028 §3
    // negotiates the timer and obliges nobody to offer one: a stack that runs
    // none sends no Session-Expires, and that is a delta the confrontation
    // judges, never a match the datagram fails.
    const timer = ["Session-Expires: 1800;refresher=uac", "Min-SE: 90"]
    const flow = flowOf(relayedB2bFlows([], timer), BOTH_VANTAGES)
    const bInvite = flow.steps.find((s) => s.leg === "B" && s.op === "expect" && s.msg.method === "INVITE")!
    expect(bInvite.msg["headers-present"]).toBeUndefined()
  })
})

describe("stack-derived RAck (§8, RFC 3262 §7.2)", () => {
  const { caller, callee, sut } = SOCKETS
  const RELIABLE = ["Require: 100rel", "RSeq: 13213449"]

  /** A PRACKed reliable 183 across a B2BUA, both vantages. */
  const prackedFlows = (): Flows.FlowsDoc =>
    doc(
      [
        leg(CALLER_CALL_ID, oneHop(caller, sut), [
          request({ callId: CALLER_CALL_ID, seq: 101, method: "INVITE", src: caller, dst: sut, ts_ms: 0, headers: ["Supported: 100rel"] }),
          response({ callId: CALLER_CALL_ID, seq: 101, status: 183, reason: "Session Progress", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: "sut-tag", headers: RELIABLE }),
          request({ callId: CALLER_CALL_ID, seq: 102, method: "PRACK", src: caller, dst: sut, ts_ms: 630, toTag: "sut-tag", headers: ["RAck: 13213449 101 INVITE"] }),
          response({ callId: CALLER_CALL_ID, seq: 102, status: 200, reason: "OK", cseqMethod: "PRACK", src: sut, dst: caller, ts_ms: 640, toTag: "sut-tag" }),
          response({ callId: CALLER_CALL_ID, seq: 101, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 2_200, toTag: "sut-tag" }),
          request({ callId: CALLER_CALL_ID, seq: 101, method: "ACK", src: caller, dst: sut, ts_ms: 2_205, toTag: "sut-tag" }),
          request({ callId: CALLER_CALL_ID, seq: 103, method: "BYE", src: caller, dst: sut, ts_ms: 6_000, toTag: "sut-tag" }),
          response({ callId: CALLER_CALL_ID, seq: 103, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 6_005, toTag: "sut-tag" })
        ]),
        leg(CALLEE_CALL_ID, oneHop(sut, callee), [
          request({ callId: CALLEE_CALL_ID, seq: 179726, method: "INVITE", src: sut, dst: callee, ts_ms: 10, headers: ["Supported: 100rel"] }),
          response({ callId: CALLEE_CALL_ID, seq: 179726, status: 183, reason: "Session Progress", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 190, toTag: "callee-tag", headers: RELIABLE }),
          request({ callId: CALLEE_CALL_ID, seq: 179727, method: "PRACK", src: sut, dst: callee, ts_ms: 635, toTag: "callee-tag", headers: ["RAck: 13213449 179726 INVITE"] }),
          response({ callId: CALLEE_CALL_ID, seq: 179727, status: 200, reason: "OK", cseqMethod: "PRACK", src: callee, dst: sut, ts_ms: 645, toTag: "callee-tag" }),
          response({ callId: CALLEE_CALL_ID, seq: 179726, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 2_190, toTag: "callee-tag" }),
          request({ callId: CALLEE_CALL_ID, seq: 179726, method: "ACK", src: sut, dst: callee, ts_ms: 2_195, toTag: "callee-tag" }),
          request({ callId: CALLEE_CALL_ID, seq: 179728, method: "BYE", src: sut, dst: callee, ts_ms: 6_002, toTag: "callee-tag" }),
          response({ callId: CALLEE_CALL_ID, seq: 179728, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 6_007, toTag: "callee-tag" })
        ])
      ],
      [{ legs: [0, 1] }]
    )

  const prackOn = (flow: ReturnType<typeof flowOf>, leg: string, op: "send" | "expect") =>
    flow.steps.find((s) => s.leg === leg && s.op === op && (s.msg.method ?? "") === "PRACK")!

  it("states no RAck on the scripted send: the stack composes it from what it was shown", () => {
    // A frozen RAck WINS over the composed value (`stack.rs::prack`), so
    // storing the captured one puts the captured platform's RSeq — and its
    // INVITE CSeq — on a wire where neither number exists.
    const send = prackOn(flowOf(prackedFlows(), BOTH_VANTAGES), "A", "send")
    expect(send.msg.headers?.some((h) => h.name === "RAck")).toBeFalsy()
  })

  it("states no RAck on the expect either, and checks existence instead", () => {
    // The SUT composes its own from its own b-leg bookkeeping, so the captured
    // value compares nothing — but RFC 3262 §7.2 makes the header mandatory, so
    // a PRACK without one is malformed however the peer numbers.
    const expectStep = prackOn(flowOf(prackedFlows(), BOTH_VANTAGES), "B", "expect")
    expect(expectStep.msg.headers?.some((h) => h.name === "RAck")).toBeFalsy()
    expect(expectStep.msg["headers-present"]).toContain("rack")
  })

  it("leaves the identifying CSeq on the automatic step untouched", () => {
    const send = prackOn(flowOf(prackedFlows(), BOTH_VANTAGES), "A", "send")
    expect(send.auto).toBe(true)
    expect(send.msg.cseq).toBe(102)
  })
})

describe("header classes and identity composition (§9.1, §8.1)", () => {
  const pcvOnly: HeaderClassifier = (name) =>
    name.trim().toLowerCase() === "p-charging-vector" ? "origin-platform-header" : undefined

  it("stamps the deployment's class on ASSERTED frozen headers only", () => {
    const flow = flowOf(relayedB2bFlows(), BOTH_VANTAGES, derivesOnePrefix, pcvOnly)
    const a180 = flow.steps.find((s) => s.leg === "A" && s.msg.status === 180)!
    expect(a180.check).toBe("assert")
    expect(a180.msg.headers?.find((h) => h.name === "P-Charging-Vector")?.class).toBe(
      "origin-platform-header"
    )
    // The same header on the recorded b-leg INVITE is matched against nothing,
    // so it carries no scoping.
    const bInvite = flow.steps.find((s) => s.leg === "B" && s.op === "expect" && s.msg.method === "INVITE")!
    expect(bInvite.check).toBe("record")
    expect(bInvite.msg.headers?.find((h) => h.name === "P-Charging-Vector")?.class).toBeUndefined()
    // A header the deployment does not own, relayed byte-for-byte from the
    // callee's 180, stays unclassified and gates.
    expect(a180.msg.headers?.find((h) => h.name === "P-Asserted-Identity")?.class).toBeUndefined()
  })

  // §6.4 at header granularity: a value the capture never shows reaching the
  // SUT is the origin platform's own emission, whatever its name.
  it("stamps origin-platform-header on an asserted value no capture-side send carries", () => {
    const minted = "User-to-User: 56a5;encoding=hex;purpose=isdn-uui;content=isdn-uui"
    const base = relayedB2bFlows()
    const { caller, sut } = SOCKETS
    // The SUT's 180 to the caller carries the UUI; the callee's 180 (the
    // capture-side send) does not, and neither does the caller's INVITE.
    const flows: Flows.FlowsDoc = {
      ...base,
      legs: base.legs.map((l, i) =>
        i === 0
          ? {
            ...l,
            msgs: l.msgs.map((m) =>
              m.summary.kind === "response" && m.summary.status === 180
                ? response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: "sut-tag", headers: ["P-Charging-Vector: icid-value=abc123", "P-Asserted-Identity: <sip:+33600000004@10.0.0.1>", minted] })
                : m
            )
          }
          : l
      )
    }
    const flow = flowOf(flows, BOTH_VANTAGES)
    const a180 = flow.steps.find((s) => s.leg === "A" && s.msg.status === 180)!
    expect(a180.check).toBe("assert")
    expect(a180.msg.headers?.find((h) => h.name === "User-to-User")?.class).toBe("origin-platform-header")
    // The relayed value the callee's 180 carries is protocol and still gates.
    expect(a180.msg.headers?.find((h) => h.name === "P-Asserted-Identity")?.class).toBeUndefined()
  })

  it("leaves a relayed value unclassified when the same header reaches the SUT with it", () => {
    const relayed = "User-to-User: 00;encoding=hex;purpose=isdn-uui;content=isdn-uui"
    const flow = flowOf(relayedB2bFlows([relayed]), BOTH_VANTAGES)
    const a180 = flow.steps.find((s) => s.leg === "A" && s.msg.status === 180)!
    expect(a180.msg.headers?.find((h) => h.name === "User-to-User")?.class).toBeUndefined()
  })

  it("composes a plan-recognized number in a number-bearing header as a ${num:…} accessor", () => {
    const flow = flowOf(relayedB2bFlows(), BOTH_VANTAGES)
    const a180 = flow.steps.find((s) => s.leg === "A" && s.msg.status === 180)!
    expect(a180.msg.headers?.find((h) => h.name === "P-Asserted-Identity")?.value).toBe(
      "<sip:${num:called-0-0:e164}@10.0.0.1>"
    )
    // A number-free header value is untouched.
    expect(a180.msg.headers?.find((h) => h.name === "P-Charging-Vector")?.value).toBe(
      "icid-value=abc123"
    )
  })
})

describe("body descriptors (RFC 3261 §20.11–§20.13, §20.24)", () => {
  const SDP = "v=0\r\no=- 1 1 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n"
  const descriptors = [
    "P-Charging-Vector: icid-value=abc123",
    "Content-Disposition: session; handling=required",
    "Content-Encoding: identity",
    "Content-Language: en",
    "MIME-Version: 1.0"
  ]
  /** The callee rings with an SDP and its descriptors; the SUT's 180 to the caller carries `callerSdp`. */
  const strippedFlows = (callerSdp: string | undefined): Flows.FlowsDoc => {
    const base = relayedB2bFlows()
    const { caller, callee, sut } = SOCKETS
    return {
      ...base,
      legs: base.legs.map((l, i) => ({
        ...l,
        msgs: l.msgs.map((m) =>
          m.summary.kind === "response" && m.summary.status === 180
            ? i === 0
              ? response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: "sut-tag", headers: descriptors, sdp: callerSdp })
              : response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 190, toTag: "callee-tag", headers: descriptors, sdp: SDP })
            : m
        )
      }))
    }
  }
  const names = (s: { msg: { headers?: ReadonlyArray<{ name: string }> } }) => (s.msg.headers ?? []).map((h) => h.name)

  it("freezes no descriptor on an expect whose captured message carries no body", () => {
    const flow = flowOf(strippedFlows(undefined), BOTH_VANTAGES)
    const a180 = flow.steps.find((s) => s.leg === "A" && s.msg.status === 180)!
    expect(a180.msg.body).toEqual({ mode: "absent" })
    expect(names(a180)).not.toContain("Content-Disposition")
    expect(names(a180)).not.toContain("Content-Encoding")
    expect(names(a180)).not.toContain("Content-Language")
    expect(names(a180)).not.toContain("MIME-Version")
    expect(names(a180)).toContain("P-Charging-Vector")
    // The send keeps the captured bytes, descriptors included.
    const b180 = flow.steps.find((s) => s.leg === "B" && s.msg.status === 180)!
    expect(b180.op).toBe("send")
    expect(names(b180)).toContain("Content-Disposition")
  })

  it("freezes the descriptors on an expect whose captured message carries the body they describe", () => {
    const flow = flowOf(strippedFlows(SDP), BOTH_VANTAGES)
    const a180 = flow.steps.find((s) => s.leg === "A" && s.msg.status === 180)!
    expect(a180.msg.body).toMatchObject({ compare: "sdp" })
    expect(names(a180)).toEqual(expect.arrayContaining(["Content-Disposition", "Content-Encoding", "Content-Language", "MIME-Version"]))
  })
})

describe("background claims (§5.1)", () => {
  /** The B2B flow plus the SUT's own in-dialog OPTIONS audit on each leg —
   * locally answered, one caller-side retransmission, relayed nowhere. */
  const auditedFlows = (): Flows.FlowsDoc => {
    const base = relayedB2bFlows()
    const { caller, callee, sut } = SOCKETS
    const legs = base.legs.map((l, i) => {
      if (i === 0) {
        const audit = request({ callId: CALLER_CALL_ID, seq: 3, method: "OPTIONS", src: sut, dst: caller, ts_ms: 1_500, toTag: "sut-tag" })
        return {
          ...l,
          msgs: [
            ...l.msgs.slice(0, 5),
            audit,
            { ...audit, ts_us: 1_600_000, retx: true, repeat_of: 5 },
            response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "OPTIONS", src: caller, dst: sut, ts_ms: 1_610, toTag: "sut-tag" }),
            ...l.msgs.slice(5)
          ]
        }
      }
      return {
        ...l,
        msgs: [
          ...l.msgs.slice(0, 5),
          request({ callId: CALLEE_CALL_ID, seq: 2, method: "OPTIONS", src: sut, dst: callee, ts_ms: 1_700, toTag: "callee-tag" }),
          response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "OPTIONS", src: callee, dst: sut, ts_ms: 1_705, toTag: "callee-tag" }),
          ...l.msgs.slice(5).map((m) => (m.summary.kind === "request" && m.summary.method === "BYE" ? { ...m, summary: { ...m.summary, cseq: { ...m.summary.cseq, seq: 3 } } } : m.summary.kind === "response" && m.summary.cseq.method === "BYE" ? { ...m, summary: { ...m.summary, cseq: { ...m.summary.cseq, seq: 3 } } } : m))
        ]
      }
    })
    return { ...base, legs }
  }

  const withPolicies = (flows: Flows.FlowsDoc) => {
    const layout = build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix)
    const policy = [{ match: { method: "OPTIONS" }, respond: { status: 200 } }]
    const background = new Map(layout.legs.map((l) => [l.actor, policy]))
    return synthesize(flows, layout, plan(), undefined, new Map(), background)
  }

  it("collapses the SUT's locally-minted audit exchanges into the policy, retransmissions included", () => {
    const flows = auditedFlows()
    const flow = withPolicies(flows)
    expect(flow.steps.some((s) => (s.msg.method ?? "").toUpperCase() === "OPTIONS" || s.msg["cseq-method"] === "OPTIONS")).toBe(false)
    expect(flow.flags.map((f) => f.kind)).toContain("background-claimed")
    // Ids stay dense over the surviving steps.
    expect(flow.steps.map((s) => s.id)).toEqual(flow.steps.map((_, i) => `s${i + 1}`))
  })

  it("keeps every step when no actor states a policy", () => {
    const flows = auditedFlows()
    const layout = build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix)
    const flow = synthesize(flows, layout, plan())
    const options = flow.steps.filter((s) => (s.msg.method ?? "").toUpperCase() === "OPTIONS")
    expect(options.length).toBeGreaterThan(0)
    expect(flow.flags.map((f) => f.kind)).not.toContain("background-claimed")
  })

  it("keeps a relayed end-to-end OPTIONS: the far-leg copy has a captured origin", () => {
    const { caller, callee, sut } = SOCKETS
    const base = relayedB2bFlows()
    const legs = base.legs.map((l, i) =>
      i === 0
        ? { ...l, msgs: [...l.msgs.slice(0, 5), request({ callId: CALLER_CALL_ID, seq: 3, method: "OPTIONS", src: caller, dst: sut, ts_ms: 1_500, toTag: "sut-tag" }), response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "OPTIONS", src: sut, dst: caller, ts_ms: 1_530, toTag: "sut-tag" }), ...l.msgs.slice(5)] }
        : { ...l, msgs: [...l.msgs.slice(0, 5), request({ callId: CALLEE_CALL_ID, seq: 2, method: "OPTIONS", src: sut, dst: callee, ts_ms: 1_510, toTag: "callee-tag" }), response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "OPTIONS", src: callee, dst: sut, ts_ms: 1_520, toTag: "callee-tag" }), ...l.msgs.slice(5).map((m) => (m.summary.cseq.method === "BYE" ? { ...m, summary: { ...m.summary, cseq: { ...m.summary.cseq, seq: 3 } } } : m))] }
    )
    const flow = withPolicies({ ...base, legs })
    // The callee-leg copy rides the caller's send within the relay window: both
    // exchanges stay scripted.
    const options = flow.steps.filter((s) => (s.msg.method ?? "").toUpperCase() === "OPTIONS")
    expect(options).toHaveLength(2)
    expect(flow.flags.map((f) => f.kind)).not.toContain("background-claimed")
  })

  it("collapses an answer whose request the vantage lost", () => {
    const { callee, sut } = SOCKETS
    const base = relayedB2bFlows()
    // The callee leg keeps a 200 to OPTIONS and holds no OPTIONS of that CSeq:
    // the probe dropped the request datagram out of the platform's own cadence.
    const legs = base.legs.map((l, i) =>
      i === 0
        ? l
        : { ...l, msgs: [...l.msgs.slice(0, 5), response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "OPTIONS", src: callee, dst: sut, ts_ms: 1_700, toTag: "callee-tag" }), ...l.msgs.slice(5).map((m) => (m.summary.cseq.method === "BYE" ? { ...m, summary: { ...m.summary, cseq: { ...m.summary.cseq, seq: 3 } } } : m))] }
    )
    const flow = withPolicies({ ...base, legs })
    expect(flow.steps.some((s) => s.msg["cseq-method"] === "OPTIONS")).toBe(false)
    expect(flow.flags.map((f) => f.kind)).toContain("background-claimed")
  })

  it("keeps an answer of a method no actor states a policy for", () => {
    const { callee, sut } = SOCKETS
    const base = relayedB2bFlows()
    const legs = base.legs.map((l, i) =>
      i === 0
        ? l
        : { ...l, msgs: [...l.msgs.slice(0, 5), response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "INFO", src: callee, dst: sut, ts_ms: 1_700, toTag: "callee-tag" }), ...l.msgs.slice(5).map((m) => (m.summary.cseq.method === "BYE" ? { ...m, summary: { ...m.summary, cseq: { ...m.summary.cseq, seq: 3 } } } : m))] }
    )
    const flow = withPolicies({ ...base, legs })
    expect(flow.steps.some((s) => s.msg["cseq-method"] === "INFO")).toBe(true)
  })

  it("collapses an audit whose only cross-leg origin already relayed to an earlier arrival", () => {
    const { caller, callee, sut } = SOCKETS
    const base = relayedB2bFlows()
    const legs = base.legs.map((l, i) =>
      i === 0
        ? { ...l, msgs: [...l.msgs.slice(0, 5), request({ callId: CALLER_CALL_ID, seq: 3, method: "OPTIONS", src: caller, dst: sut, ts_ms: 1_500, toTag: "sut-tag" }), response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "OPTIONS", src: sut, dst: caller, ts_ms: 1_530, toTag: "sut-tag" }), ...l.msgs.slice(5)] }
        : { ...l, msgs: [...l.msgs.slice(0, 5), request({ callId: CALLEE_CALL_ID, seq: 2, method: "OPTIONS", src: sut, dst: callee, ts_ms: 1_510, toTag: "callee-tag" }), response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "OPTIONS", src: callee, dst: sut, ts_ms: 1_520, toTag: "callee-tag" }), request({ callId: CALLEE_CALL_ID, seq: 3, method: "OPTIONS", src: sut, dst: callee, ts_ms: 1_900, toTag: "callee-tag" }), response({ callId: CALLEE_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "OPTIONS", src: callee, dst: sut, ts_ms: 1_910, toTag: "callee-tag" }), ...l.msgs.slice(5).map((m) => (m.summary.cseq.method === "BYE" ? { ...m, summary: { ...m.summary, cseq: { ...m.summary.cseq, seq: 4 } } } : m))] }
    )
    const flow = withPolicies({ ...base, legs })
    // The caller's 1500 ms send origins the 1510 ms relay and nothing else: the
    // 1900 ms audit is the platform's own and belongs to the policy.
    const options = flow.steps.filter((s) => (s.msg.method ?? "").toUpperCase() === "OPTIONS")
    expect(options).toHaveLength(2)
    expect(flow.flags.map((f) => f.kind)).toContain("background-claimed")
  })
})

describe("dialog markers", () => {
  it("stamps in_dialog on every step after the dialog-creating final", () => {
    const flow = flowOf(reInviteFlows(), CALLER_ONLY)
    const marked = flow.steps.filter((s) => s.in_dialog === true).map((s) => s.id)
    // s4 is the 200 that creates the dialog; everything after it is inside.
    expect(marked).toEqual(["s5", "s6", "s7", "s8", "s9", "s10"])
  })

  it("confirms the dialog on the FIRST ACK after that final, and no later one", () => {
    const flow = flowOf(reInviteFlows(), CALLER_ONLY)
    const confirming = flow.steps.filter((s) => s.confirms_dialog === true).map((s) => s.id)
    expect(confirming).toEqual(["s5"])
    // The re-INVITE's own ACK is in-dialog and confirms nothing.
    const reAck = flow.steps.find((s) => s.id === "s8")!
    expect(reAck.in_dialog).toBe(true)
    expect(reAck.confirms_dialog).toBeUndefined()
  })

  // The ACK that confirms the dialog is the one answering the dialog-creating
  // 2xx (RFC 3261 §13.2.2.4), not the first ACK the leg carries after it: a
  // re-INVITE sent over the un-ACKed 2xx is answered 491 (§14.1) and its ACK is
  // that transaction's own (§17.1.1.3). Pinned on the leg where the actor is
  // the UAC (A: send INVITE, expect finals, send ACKs) and on the one where it
  // is the UAS (B: expect INVITE, send finals, expect ACKs).
  it("confirms the dialog on the ACK to the 2xx, not on a 491 round's ACK sent before it", () => {
    const flow = flowOf(reInviteOverUnackedFinalFlows(), BOTH_VANTAGES)
    for (const legId of ["A", "B"]) {
      const onLeg = flow.steps.filter((s) => s.leg === legId)
      const acks = onLeg.filter((s) => (s.msg.method ?? "").toUpperCase() === "ACK")
      expect(acks.map((s) => s.msg.cseq)).toEqual([2, 1])
      const [to491, to200] = acks
      expect(to491!.confirms_dialog, `${legId}: the ACK to the 491`).toBeUndefined()
      expect(to200!.confirms_dialog, `${legId}: the ACK to the 200`).toBe(true)
      // Both ACKs run after the dialog-creating final, so both are in-dialog;
      // the 491 and its re-INVITE are too.
      const [invite, final] = onLeg
      expect(invite!.in_dialog).toBeUndefined()
      expect(final!.in_dialog).toBeUndefined()
      expect(onLeg.slice(2).every((s) => s.in_dialog === true)).toBe(true)
      expect(onLeg.filter((s) => s.confirms_dialog === true)).toHaveLength(1)
    }
  })

  // A 2xx under a second To-tag to the same INVITE is a second dialog, and the
  // UAC ACKs it too (RFC 3261 §13.2.2.4): the marker is stated once per
  // DIALOG, so a leg answered under two tags carries it twice — on the ACK to
  // each fork's 2xx — and not on the re-INVITE's ACK inside the second one.
  it("confirms each fork's dialog on the ACK to its own 2xx", () => {
    const flow = flowOf(twoForksAnsweredFlows(), BOTH_VANTAGES)
    for (const legId of ["A", "B"]) {
      const onLeg = flow.steps.filter((s) => s.leg === legId)
      const acks = onLeg.filter((s) => (s.msg.method ?? "").toUpperCase() === "ACK")
      expect(acks.map((s) => s.msg.cseq)).toEqual([1, 1, 3])
      const [toFirst, toSecond, toReInvite] = acks
      expect(toFirst!.confirms_dialog, `${legId}: the ACK to the first fork's 2xx`).toBe(true)
      expect(toSecond!.confirms_dialog, `${legId}: the ACK to the second fork's 2xx`).toBe(true)
      expect(toReInvite!.confirms_dialog, `${legId}: the re-INVITE's ACK`).toBeUndefined()
      // The second fork's 2xx runs after the leg's first dialog-creating
      // final, so it is in-dialog like everything behind it.
      const secondFinal = onLeg.filter((s) => s.msg.status === 200 && s.msg["cseq-method"] === "INVITE")[1]!
      expect(secondFinal.in_dialog).toBe(true)
    }
  })
})
