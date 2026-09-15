/**
 * The far side of an in-dialog INVITE exchange the capture holds on one leg
 * only: the vantage lost the callee leg past its 2xx, the caller goes on to
 * re-INVITE, and a platform that relays the re-INVITE puts it on a leg the
 * document would otherwise script nothing for (§6.9).
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { synthesize } from "../src/flowsteps.js"
import { build } from "../src/topology.js"
import type { Vantage } from "../src/selection.js"
import {
  ANSWER_SDP,
  CALLEE_CALL_ID,
  CALLER_CALL_ID,
  CALLEE_URI,
  CALLER_URI,
  derivesOnePrefix,
  doc,
  leg,
  MOVED_SDP,
  oneHop,
  plan,
  request,
  response,
  SOCKETS,
  sutSet
} from "./fixtures.js"

const BOTH_VANTAGES: ReadonlyArray<Vantage> = [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }]
const { caller, callee, sut } = SOCKETS

const OFFER = { contentType: "application/sdp", text: ANSWER_SDP }
const MOVED_OFFER = { contentType: "application/sdp", text: MOVED_SDP }

interface CallerShape {
  readonly answer?: { readonly status: number; readonly reason: string }
  /** The re-INVITE carries no offer: the 2xx offers and the ACK answers (RFC 3261 §13.2.1). */
  readonly delayedOffer?: boolean
  /** A second re-INVITE, CSeq 3, two seconds behind the first. */
  readonly twice?: boolean
  /** When the re-INVITE goes out, in ms. */
  readonly at?: number
}

/** The caller's leg: answered, ACKed, then a re-INVITE the platform answers with a 2xx of the callee's. */
const callerLeg = (shape: CallerShape = {}): Flows.Leg => {
  const answer = shape.answer ?? { status: 200, reason: "OK" }
  const at = shape.at ?? 5_000
  const reInvite = (seq: number, at: number): ReadonlyArray<Flows.Msg> => [
    request({ callId: CALLER_CALL_ID, seq, method: "INVITE", src: caller, dst: sut, ts_ms: at, toTag: "sut-tag", ...(shape.delayedOffer ? {} : { body: OFFER }) }),
    response({ callId: CALLER_CALL_ID, seq, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: at + 30 }),
    response({ callId: CALLER_CALL_ID, seq, ...answer, cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: at + 90, toTag: "sut-tag", headers: ["Server: far-party/1.0"], sdp: MOVED_SDP }),
    request({ callId: CALLER_CALL_ID, seq, method: "ACK", src: caller, dst: sut, ts_ms: at + 95, toTag: "sut-tag", ...(shape.delayedOffer ? { body: OFFER } : {}) })
  ]
  return leg(CALLER_CALL_ID, oneHop(caller, sut), [
    request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0, body: OFFER }),
    response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
    response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag", sdp: ANSWER_SDP }),
    request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_005, toTag: "sut-tag" }),
    ...reInvite(2, at),
    ...(shape.twice ? reInvite(3, at + 2_000) : []),
    request({ callId: CALLER_CALL_ID, seq: 4, method: "BYE", src: caller, dst: sut, ts_ms: 9_000, toTag: "sut-tag" }),
    response({ callId: CALLER_CALL_ID, seq: 4, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 9_005, toTag: "sut-tag" })
  ])
}

/** The callee's leg as the vantage kept it: up to the callee's 2xx and not one datagram more. */
const blindCalleeLeg = (tail: ReadonlyArray<Flows.Msg> = []): Flows.Leg =>
  leg(CALLEE_CALL_ID, oneHop(sut, callee), [
    request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10, body: OFFER }),
    response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
    response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 990, toTag: "callee-tag", sdp: ANSWER_SDP }),
    ...tail
  ])

const flowsOf = (callerSide: Flows.Leg, calleeSide: Flows.Leg): Flows.FlowsDoc =>
  doc([callerSide, calleeSide], [{ legs: [0, 1] }])

const flowOf = (flows: Flows.FlowsDoc, relaysReinvite: boolean) =>
  synthesize(
    flows,
    build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix),
    plan(),
    undefined,
    undefined,
    undefined,
    true,
    relaysReinvite
  )

const onLeg = (flow: ReturnType<typeof flowOf>, id: string) =>
  flow.steps.filter((s) => s.leg === id)

describe("the far side of a relayed re-INVITE (§6.9)", () => {
  const flows = flowsOf(callerLeg(), blindCalleeLeg())

  it("derives the callee's expect INVITE, send 2xx and expect ACK where the platform relays", () => {
    const flow = flowOf(flows, true)
    const b = onLeg(flow, "B")
    // The callee's captured record: INVITE, 100, 200 — then the three derived steps.
    expect(b.map((s) => `${s.op} ${s.msg.method ?? s.msg.status}`)).toEqual([
      "expect INVITE",
      "send 100",
      "send 200",
      "expect INVITE",
      "send 200",
      "expect ACK"
    ])
    const [, , answered, invite, answer, ack] = b
    expect(invite!.in_dialog).toBe(true)
    expect(invite!.check).toBe("record")
    expect(invite!.msg.body).toEqual({ ref: expect.stringMatching(/\.sdp$/), rewrite: ["c=addr", "m=port"], compare: "sdp" })
    // The 2xx is the one the caller received: headers and body as captured.
    expect(answer!.in_dialog).toBe(true)
    expect(answer!.msg.status).toBe(200)
    expect(answer!.msg["cseq-method"]).toBe("INVITE")
    expect(answer!.msg.headers).toEqual([{ name: "Server", value: "far-party/1.0" }])
    expect(answer!.msg.body).toEqual({ ref: expect.stringMatching(/\.sdp$/), rewrite: ["c=addr", "m=port"] })
    expect(answer!.confirms_dialog).toBeUndefined()
    expect(ack!.auto).toBe(true)
    expect(ack!.in_dialog).toBe(true)
    expect(ack!.confirms_dialog).toBeUndefined()
    expect(ack!.msg.cseq).toBe(2)
    expect(answered!.confirms_dialog).toBeUndefined()
  })

  it("orders the derived steps as the relay runs: INVITE and 2xx before the caller's 2xx, the ACK after the caller's", () => {
    const flow = flowOf(flows, true)
    const ids = flow.steps.map(
      (s) => `${s.leg}:${s.op}:${s.msg.method ?? `${s.msg.status}/${s.msg["cseq-method"]}`}`
    )
    const last = (needle: string) => ids.lastIndexOf(needle)
    expect(last("A:send:INVITE")).toBeLessThan(last("B:expect:INVITE"))
    expect(last("B:expect:INVITE")).toBeLessThan(last("B:send:200/INVITE"))
    expect(last("B:send:200/INVITE")).toBeLessThan(last("A:expect:200/INVITE"))
    expect(last("A:expect:200/INVITE")).toBeLessThan(last("A:send:ACK"))
    expect(last("A:send:ACK")).toBeLessThan(last("B:expect:ACK"))
    expect(flow.steps.map((s) => s.id)).toEqual(flow.steps.map((_, i) => `s${i + 1}`))
  })

  it("anchors each derived arrival on the emission it relays and the caller's 2xx on the derived answer", () => {
    const flow = flowOf(flows, true)
    const by = (leg: string, op: string, what: string | number) =>
      flow.steps.filter(
        (s) =>
          s.leg === leg &&
          s.op === op &&
          (s.msg.method ?? s.msg.status) === what &&
          (s.msg.status === undefined || s.msg["cseq-method"] === "INVITE")
      )
    const [reInvite] = by("A", "send", "INVITE").slice(-1)
    const [bInvite] = by("B", "expect", "INVITE").slice(-1)
    const [bAnswer] = by("B", "send", 200).slice(-1)
    const [aAnswer] = by("A", "expect", 200).slice(-1)
    const [aAck] = by("A", "send", "ACK").slice(-1)
    const [bAck] = by("B", "expect", "ACK")
    expect(bInvite!.delay).toEqual({ ms: 0, from: `step:${reInvite!.id}`, compressible: true, timer_linked: false })
    // The INVITE lands one measured hop (10 ms, the initial relay's) after the
    // caller sent it; the answer goes out at the instant the caller took it.
    expect(bAnswer!.delay).toEqual({ ms: 80, from: `step:${bInvite!.id}`, compressible: true, timer_linked: false })
    expect(aAnswer!.delay).toEqual({ ms: 0, from: `step:${bAnswer!.id}`, compressible: true, timer_linked: false })
    expect(aAnswer!.check).toBe("assert")
    expect(bAck!.delay).toEqual({ ms: 0, from: `step:${aAck!.id}`, compressible: true, timer_linked: false })
  })

  it("keeps the coordinate of the near-leg message each derived step copies, and marks its source", () => {
    const flow = flowOf(flows, true)
    const derived = flow.steps
      .map((s, i) => ({ s, src: flow.sources[i]! }))
      .filter(({ src }) => src.mirrored === true)
    expect(derived.map(({ s }) => `${s.leg}:${s.op}:${s.msg.method ?? s.msg.status}`)).toEqual([
      "B:expect:INVITE",
      "B:send:200",
      "B:expect:ACK"
    ])
    // Leg 0, messages 4, 6 and 7: the caller's re-INVITE, its 2xx and its ACK.
    expect(derived.map(({ src }) => [src.origLeg, src.msgIdx])).toEqual([[0, 4], [0, 6], [0, 7]])
    expect(derived.map(({ s }) => s.observed)).toEqual(
      derived.map(({ src }) => expect.objectContaining({ leg: src.origLeg, msg: src.msgIdx }))
    )
    expect(derived.map(({ src }) => src.emits)).toEqual([false, true, false])
    expect(derived.map(({ src }) => src.auto)).toEqual([false, false, true])
    expect(derived.every(({ s, src }) => s.id === src.id)).toBe(true)
  })

  it("names every derived step, its leg, and the leg whose record ended, in one flag", () => {
    const flow = flowOf(flows, true)
    const flag = flow.flags.find((f) => f.kind === "far-side-reinvite-derived")
    expect(flag).toBeDefined()
    const b = onLeg(flow, "B")
    for (const step of b.slice(3)) expect(flag!.detail).toContain(step.id)
    expect(flag!.detail).toContain("leg B")
    expect(flag!.detail).toContain(b[2]!.id)
  })

  it("derives nothing where the policy states no relay", () => {
    const flow = flowOf(flows, false)
    expect(onLeg(flow, "B")).toHaveLength(3)
    expect(flow.flags.some((f) => f.kind === "far-side-reinvite-derived")).toBe(false)
    const [aAnswer] = flow.steps.filter((s) => s.leg === "A" && s.op === "expect" && s.msg.status === 200).slice(-1)
    expect(aAnswer!.check).toBe("record")
  })

  it("derives nothing where the callee leg's record goes on past its 2xx", () => {
    // The vantage kept watching: the ACK came, and later the BYE — and no
    // INVITE. The platform did not relay it, and the document says so.
    const watched = blindCalleeLeg([
      request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_010, toTag: "callee-tag" }),
      request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: 9_002, toTag: "callee-tag" }),
      response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 9_007, toTag: "callee-tag" })
    ])
    const flow = flowOf(flowsOf(callerLeg(), watched), true)
    expect(onLeg(flow, "B").map((s) => `${s.op} ${s.msg.method ?? s.msg.status}`)).toEqual([
      "expect INVITE",
      "send 100",
      "send 200",
      "expect ACK",
      "expect BYE",
      "send 200"
    ])
    expect(flow.flags.some((f) => f.kind === "far-side-reinvite-derived")).toBe(false)
  })

  it("derives the second exchange on the same far leg, each ACK pairing with its own 2xx", () => {
    const flow = flowOf(flowsOf(callerLeg({ twice: true }), blindCalleeLeg()), true)
    const b = onLeg(flow, "B")
    expect(b.map((s) => `${s.op} ${s.msg.method ?? s.msg.status}`)).toEqual([
      "expect INVITE",
      "send 100",
      "send 200",
      "expect INVITE",
      "send 200",
      "expect ACK",
      "expect INVITE",
      "send 200",
      "expect ACK"
    ])
    const acks = b.filter((s) => s.msg.method === "ACK")
    expect(acks.map((s) => s.msg.cseq)).toEqual([2, 3])
    expect(b.some((s) => s.confirms_dialog === true)).toBe(false)
    // The first pair is closed before the second re-INVITE goes out.
    const ids = flow.steps.map((s) => `${s.leg}:${s.op}:${s.msg.method ?? s.msg.status}:${s.msg.cseq ?? ""}`)
    const firstAck = ids.indexOf("B:expect:ACK:2")
    const secondInvite = flow.steps.findIndex((s) => s.leg === "A" && s.msg.method === "INVITE" && s.observed?.msg === 8)
    expect(firstAck).toBeGreaterThan(0)
    expect(firstAck).toBeLessThan(secondInvite)
    const flag = flow.flags.find((f) => f.kind === "far-side-reinvite-derived")
    expect(flag!.detail).toMatch(/^2 in-dialog INVITE exchange/)
  })

  it("mirrors a delayed offer: the INVITE expects no body, the ACK is compared as the answer it carries", () => {
    const flow = flowOf(flowsOf(callerLeg({ delayedOffer: true }), blindCalleeLeg()), true)
    const b = onLeg(flow, "B")
    const [, , , invite, answer, ack] = b
    expect(invite!.msg.body).toEqual({ mode: "absent" })
    expect(answer!.msg.body).toEqual({ ref: expect.stringMatching(/\.sdp$/), rewrite: ["c=addr", "m=port"] })
    expect(ack!.msg.body).toEqual({ ref: expect.stringMatching(/\.sdp$/), rewrite: ["c=addr", "m=port"], compare: "sdp" })
  })

  it("derives nothing for an answer that already has a relay origin", () => {
    // The re-INVITE's 2xx lands inside the relay window of the far leg's own
    // 2xx, so the classifier reads it as that relay: an answer the far leg's
    // record already holds is a step, and nothing is transcribed for it.
    const flow = flowOf(flowsOf(callerLeg({ at: 1_500 }), blindCalleeLeg()), true)
    expect(onLeg(flow, "B")).toHaveLength(3)
    expect(flow.flags.some((f) => f.kind.startsWith("far-side-reinvite"))).toBe(false)
  })

  it("leaves a 491 alone and unflagged: glare the replaying platform answers itself (RFC 3261 §14.1)", () => {
    const flow = flowOf(
      flowsOf(callerLeg({ answer: { status: 491, reason: "Request Pending" } }), blindCalleeLeg()),
      true
    )
    expect(onLeg(flow, "B")).toHaveLength(3)
    expect(flow.flags.some((f) => f.kind.startsWith("far-side-reinvite"))).toBe(false)
  })

  it("derives nothing for a re-INVITE the platform refused, and says so", () => {
    const flow = flowOf(
      flowsOf(callerLeg({ answer: { status: 488, reason: "Not Acceptable Here" } }), blindCalleeLeg()),
      true
    )
    expect(onLeg(flow, "B")).toHaveLength(3)
    const flag = flow.flags.find((f) => f.kind === "far-side-reinvite-not-derived")
    expect(flag).toBeDefined()
    expect(flag!.detail).toContain("488")
  })
})

interface MirrorShape {
  readonly answer?: { readonly status: number; readonly reason: string }
  /** The far party's re-INVITE carries no offer: the caller's 2xx offers and the ACK answers. */
  readonly delayedOffer?: boolean
  /** When the platform's re-INVITE reaches the caller, in ms. */
  readonly at?: number
  /** The caller's own re-INVITE, CSeq 2, two seconds behind the platform's. */
  readonly callerToo?: boolean
}

/**
 * The caller's leg with the far party's re-INVITE on it: the platform sends an
 * in-dialog INVITE down the leg it did not open — numbered from its own CSeq
 * space (RFC 3261 §12.2), carrying the far party's offer — the caller answers
 * it, and the platform ACKs.
 */
const relayedReinviteCallerLeg = (shape: MirrorShape = {}): Flows.Leg => {
  const answer = shape.answer ?? { status: 200, reason: "OK" }
  const at = shape.at ?? 5_000
  const inDialog = { fromUri: CALLEE_URI, fromTag: "sut-tag", toTag: `from-${CALLER_CALL_ID}` } as const
  const callerReInvite = (seq: number, at: number): ReadonlyArray<Flows.Msg> => [
    request({ callId: CALLER_CALL_ID, seq, method: "INVITE", src: caller, dst: sut, ts_ms: at, toTag: "sut-tag", body: OFFER }),
    response({ callId: CALLER_CALL_ID, seq, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: at + 90, toTag: "sut-tag", headers: ["Server: far-party/1.0"], sdp: MOVED_SDP }),
    request({ callId: CALLER_CALL_ID, seq, method: "ACK", src: caller, dst: sut, ts_ms: at + 95, toTag: "sut-tag" })
  ]
  return leg(CALLER_CALL_ID, oneHop(caller, sut), [
    request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0, body: OFFER }),
    response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
    response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag", sdp: ANSWER_SDP }),
    request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_005, toTag: "sut-tag" }),
    request({ callId: CALLER_CALL_ID, seq: 41, method: "INVITE", src: sut, dst: caller, ts_ms: at, ruri: CALLER_URI, ...inDialog, headers: ["Supported: timer"], ...(shape.delayedOffer ? {} : { body: MOVED_OFFER }) }),
    response({ callId: CALLER_CALL_ID, seq: 41, ...answer, cseqMethod: "INVITE", src: caller, dst: sut, ts_ms: at + 30, toUri: CALLER_URI, ...inDialog, headers: ["Session-Expires: 1800;refresher=uas"], sdp: ANSWER_SDP }),
    request({ callId: CALLER_CALL_ID, seq: 41, method: "ACK", src: sut, dst: caller, ts_ms: at + 60, ruri: CALLER_URI, ...inDialog, ...(shape.delayedOffer ? { body: MOVED_OFFER } : {}) }),
    ...(shape.callerToo ? callerReInvite(2, at + 2_000) : []),
    request({ callId: CALLER_CALL_ID, seq: 3, method: "BYE", src: caller, dst: sut, ts_ms: 9_000, toTag: "sut-tag" }),
    response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 9_005, toTag: "sut-tag" })
  ])
}

describe("the far side of a relayed re-INVITE the far party sent (§6.9)", () => {
  const flows = flowsOf(relayedReinviteCallerLeg(), blindCalleeLeg())
  const shape = (s: { op: string; msg: { method?: string; status?: number } }) =>
    `${s.op} ${s.msg.method ?? s.msg.status}`

  it("derives the callee's send INVITE, expect 2xx and send ACK where the platform relays", () => {
    const flow = flowOf(flows, true)
    const b = onLeg(flow, "B")
    expect(b.map(shape)).toEqual([
      "expect INVITE",
      "send 100",
      "send 200",
      "send INVITE",
      "expect 200",
      "send ACK"
    ])
    const [, , answered, invite, answer, ack] = b
    // The INVITE carries the far party's offer as the caller took it, and the
    // headers the caller's expect froze: a relayed request is the far party's own.
    expect(invite!.in_dialog).toBe(true)
    expect(invite!.auto).toBeUndefined()
    expect(invite!.msg.body).toEqual({ ref: expect.stringMatching(/\.sdp$/), rewrite: ["c=addr", "m=port"] })
    expect(invite!.msg.headers).toEqual([{ name: "Supported", value: "timer" }])
    // The 2xx is the caller's answer relayed: stated as the session
    // description the caller sent, no frozen headers — the platform mints them
    // — and recorded like every arrival on a leg the platform initiated (§6.4).
    expect(answer!.in_dialog).toBe(true)
    expect(answer!.msg.status).toBe(200)
    expect(answer!.msg["cseq-method"]).toBe("INVITE")
    expect(answer!.msg.headers).toBeUndefined()
    expect(answer!.msg.body).toEqual({ ref: expect.stringMatching(/\.sdp$/), rewrite: ["c=addr", "m=port"], compare: "sdp" })
    expect(answer!.check).toBe("record")
    expect(answer!.confirms_dialog).toBeUndefined()
    expect(ack!.auto).toBe(true)
    expect(ack!.in_dialog).toBe(true)
    expect(ack!.confirms_dialog).toBeUndefined()
    expect(ack!.msg.cseq).toBe(41)
    expect(ack!.msg.body).toBeUndefined()
    expect(answered!.confirms_dialog).toBeUndefined()
  })

  it("orders the derived steps as the relay runs: the INVITE before the caller's expect, the 2xx after the caller's send, the ACK before the caller's expect", () => {
    const flow = flowOf(flows, true)
    const ids = flow.steps.map(
      (s) => `${s.leg}:${s.op}:${s.msg.method ?? `${s.msg.status}/${s.msg["cseq-method"]}`}`
    )
    const last = (needle: string) => ids.lastIndexOf(needle)
    expect(last("B:send:INVITE")).toBeLessThan(last("A:expect:INVITE"))
    expect(last("A:expect:INVITE")).toBeLessThan(last("A:send:200/INVITE"))
    expect(last("A:send:200/INVITE")).toBeLessThan(last("B:expect:200/INVITE"))
    expect(last("B:expect:200/INVITE")).toBeLessThan(last("B:send:ACK"))
    expect(last("B:send:ACK")).toBeLessThan(last("A:expect:ACK"))
    expect(flow.steps.map((s) => s.id)).toEqual(flow.steps.map((_, i) => `s${i + 1}`))
  })

  it("anchors the caller's expects on the derived sends and the derived 2xx on the caller's send", () => {
    const flow = flowOf(flows, true)
    const by = (leg: string, op: string, what: string | number) =>
      flow.steps.filter(
        (s) =>
          s.leg === leg &&
          s.op === op &&
          (s.msg.method ?? s.msg.status) === what &&
          (s.msg.status === undefined || s.msg["cseq-method"] === "INVITE")
      )
    const [bAnswered] = by("B", "send", 200)
    const [bInvite] = by("B", "send", "INVITE")
    const [aInvite] = by("A", "expect", "INVITE")
    const [aAnswer] = by("A", "send", 200)
    const [bAnswer] = by("B", "expect", 200)
    const [bAck] = by("B", "send", "ACK")
    const [aAck] = by("A", "expect", "ACK")
    // The INVITE goes out one measured hop (10 ms, the initial 2xx's) before
    // the caller took it, measured on its own leg from the callee's 2xx; the
    // caller's expect is then its relay, asserted like any relayed content.
    expect(bInvite!.delay).toEqual({ ms: 4000, from: `step:${bAnswered!.id}`, compressible: true, timer_linked: false })
    expect(aInvite!.delay).toEqual({ ms: 0, from: `step:${bInvite!.id}`, compressible: true, timer_linked: false })
    expect(aInvite!.check).toBe("assert")
    expect(bAnswer!.delay).toEqual({ ms: 0, from: `step:${aAnswer!.id}`, compressible: true, timer_linked: false })
    expect(bAck!.delay).toEqual({ ms: 10, from: `step:${bAnswer!.id}`, compressible: true, timer_linked: false })
    expect(aAck!.delay).toEqual({ ms: 0, from: `step:${bAck!.id}`, compressible: true, timer_linked: false })
  })

  it("keeps the coordinate of the caller-leg message each derived step copies, and marks its source", () => {
    const flow = flowOf(flows, true)
    const derived = flow.steps
      .map((s, i) => ({ s, src: flow.sources[i]! }))
      .filter(({ src }) => src.mirrored === true)
    expect(derived.map(({ s }) => `${s.leg}:${shape(s)}`)).toEqual([
      "B:send INVITE",
      "B:expect 200",
      "B:send ACK"
    ])
    // Leg 0, messages 4, 5 and 6: the platform's INVITE, the caller's 2xx and the platform's ACK.
    expect(derived.map(({ src }) => [src.origLeg, src.msgIdx])).toEqual([[0, 4], [0, 5], [0, 6]])
    expect(derived.map(({ s }) => s.observed)).toEqual(
      derived.map(({ src }) => expect.objectContaining({ leg: src.origLeg, msg: src.msgIdx }))
    )
    expect(derived.map(({ src }) => src.emits)).toEqual([true, false, true])
    expect(derived.map(({ src }) => src.auto)).toEqual([false, false, true])
    expect(derived.every(({ s, src }) => s.id === src.id)).toBe(true)
  })

  it("names every derived step by its op, its leg, and the leg whose record ended, in one flag", () => {
    const flow = flowOf(flows, true)
    const flag = flow.flags.find((f) => f.kind === "far-side-reinvite-derived")
    expect(flag).toBeDefined()
    const b = onLeg(flow, "B")
    const a = onLeg(flow, "A")
    const [invite, answer, ack] = b.slice(3)
    const aInvite = a.find((s) => s.op === "expect" && s.msg.method === "INVITE")!
    const aAnswer = a.find((s) => s.op === "send" && s.msg.status === 200 && s.msg["cseq-method"] === "INVITE")!
    const aAck = a.find((s) => s.op === "expect" && s.msg.method === "ACK")!
    expect(flag!.detail).toContain(`leg B (record ends at ${b[2]!.id})`)
    expect(flag!.detail).toContain(`${invite!.id} send INVITE mirrors ${aInvite.id}`)
    expect(flag!.detail).toContain(`${answer!.id} expect 200 mirrors ${aAnswer.id}`)
    expect(flag!.detail).toContain(`${ack!.id} send ACK mirrors ${aAck.id}`)
  })

  it("derives nothing where the policy states no relay", () => {
    const flow = flowOf(flows, false)
    expect(onLeg(flow, "B")).toHaveLength(3)
    expect(flow.flags.some((f) => f.kind.startsWith("far-side-reinvite"))).toBe(false)
    const [aInvite] = flow.steps.filter((s) => s.leg === "A" && s.op === "expect" && s.msg.method === "INVITE")
    expect(aInvite!.check).toBe("record")
  })

  it("derives nothing where the callee leg's record goes on past its 2xx", () => {
    const watched = blindCalleeLeg([
      request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_010, toTag: "callee-tag" }),
      request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: 9_002, toTag: "callee-tag" }),
      response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 9_007, toTag: "callee-tag" })
    ])
    const flow = flowOf(flowsOf(relayedReinviteCallerLeg(), watched), true)
    expect(onLeg(flow, "B").map(shape)).toEqual([
      "expect INVITE",
      "send 100",
      "send 200",
      "expect ACK",
      "expect BYE",
      "send 200"
    ])
    expect(flow.flags.some((f) => f.kind.startsWith("far-side-reinvite"))).toBe(false)
  })

  it("mirrors a delayed offer: the INVITE carries no body, the ACK carries the answer the caller took", () => {
    const flow = flowOf(flowsOf(relayedReinviteCallerLeg({ delayedOffer: true }), blindCalleeLeg()), true)
    const [, , , invite, answer, ack] = onLeg(flow, "B")
    expect(invite!.msg.body).toBeUndefined()
    expect(answer!.msg.body).toEqual({ ref: expect.stringMatching(/\.sdp$/), rewrite: ["c=addr", "m=port"], compare: "sdp" })
    expect(ack!.msg.body).toEqual({ ref: expect.stringMatching(/\.sdp$/), rewrite: ["c=addr", "m=port"] })
  })

  it("derives nothing for a re-INVITE the caller refused, and says so", () => {
    const flow = flowOf(
      flowsOf(relayedReinviteCallerLeg({ answer: { status: 488, reason: "Not Acceptable Here" } }), blindCalleeLeg()),
      true
    )
    expect(onLeg(flow, "B")).toHaveLength(3)
    const flag = flow.flags.find((f) => f.kind === "far-side-reinvite-not-derived")
    expect(flag).toBeDefined()
    expect(flag!.detail).toContain("488")
    expect(flow.flags.some((f) => f.kind === "far-side-reinvite-derived")).toBe(false)
  })

  it("derives both directions on one document, each exchange closed before the next opens", () => {
    const flow = flowOf(flowsOf(relayedReinviteCallerLeg({ callerToo: true }), blindCalleeLeg()), true)
    const b = onLeg(flow, "B")
    expect(b.map(shape)).toEqual([
      "expect INVITE",
      "send 100",
      "send 200",
      "send INVITE",
      "expect 200",
      "send ACK",
      "expect INVITE",
      "send 200",
      "expect ACK"
    ])
    expect(b.filter((s) => s.msg.method === "ACK").map((s) => s.msg.cseq)).toEqual([41, 2])
    expect(b.some((s) => s.confirms_dialog === true)).toBe(false)
    const ids = flow.steps.map((s) => `${s.leg}:${s.op}:${s.msg.method ?? s.msg.status}`)
    expect(ids.indexOf("B:send:ACK")).toBeLessThan(ids.lastIndexOf("A:send:INVITE"))
    const flag = flow.flags.find((f) => f.kind === "far-side-reinvite-derived")
    expect(flag!.detail).toMatch(/^2 in-dialog INVITE exchange/)
    expect(flag!.detail).toContain("send INVITE mirrors")
    expect(flag!.detail).toContain("expect INVITE mirrors")
    expect(flow.flags.some((f) => f.kind === "far-side-reinvite-not-derived")).toBe(false)
  })
})
