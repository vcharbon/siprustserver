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

/** The caller's leg: answered, ACKed, then a re-INVITE the platform answers with a 2xx of the callee's. */
const callerLeg = (
  answer: { readonly status: number; readonly reason: string } = { status: 200, reason: "OK" }
): Flows.Leg =>
  leg(CALLER_CALL_ID, oneHop(caller, sut), [
    request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0, body: OFFER }),
    response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
    response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag", sdp: ANSWER_SDP }),
    request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_005, toTag: "sut-tag" }),
    request({ callId: CALLER_CALL_ID, seq: 2, method: "INVITE", src: caller, dst: sut, ts_ms: 5_000, toTag: "sut-tag", body: OFFER }),
    response({ callId: CALLER_CALL_ID, seq: 2, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5_030 }),
    response({ callId: CALLER_CALL_ID, seq: 2, ...answer, cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5_090, toTag: "sut-tag", headers: ["Server: far-party/1.0"], sdp: MOVED_SDP }),
    request({ callId: CALLER_CALL_ID, seq: 2, method: "ACK", src: caller, dst: sut, ts_ms: 5_095, toTag: "sut-tag" }),
    request({ callId: CALLER_CALL_ID, seq: 3, method: "BYE", src: caller, dst: sut, ts_ms: 9_000, toTag: "sut-tag" }),
    response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 9_005, toTag: "sut-tag" })
  ])

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

  it("derives nothing for a re-INVITE the platform refused, and says so", () => {
    const flow = flowOf(
      flowsOf(callerLeg({ status: 488, reason: "Not Acceptable Here" }), blindCalleeLeg()),
      true
    )
    expect(onLeg(flow, "B")).toHaveLength(3)
    const flag = flow.flags.find((f) => f.kind === "far-side-reinvite-not-derived")
    expect(flag).toBeDefined()
    expect(flag!.detail).toContain("488")
  })
})
