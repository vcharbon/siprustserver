/**
 * The SIP facts both models read, and the one claim they share: a fact read off
 * a captured message and the same fact read off the step transcribed from it
 * agree, message for message.
 */
import { describe, expect, it } from "vitest"
import * as Flow from "../src/flow.js"
import * as Flows from "../src/flows.js"

const party = { uri: "sip:a@h", tag: null }
const tagged = { uri: "sip:b@h", tag: "t1" }

const request = (method: string, toTag: string | null): Flows.Msg =>
  ({
    ts_us: 1,
    src: "1.1.1.1:5060",
    dst: "2.2.2.2:5060",
    hop: 0,
    retx: false,
    probe: 0,
    summary: { kind: "request", method, uri: "sip:b@h", cseq: { seq: 1, method }, from: party, to: { ...party, tag: toTag } },
    identities: { from: {}, to: {} },
    raw: "X"
  }) as unknown as Flows.Msg

const response = (status: number, cseqMethod: string): Flows.Msg =>
  ({
    ts_us: 1,
    src: "2.2.2.2:5060",
    dst: "1.1.1.1:5060",
    hop: 0,
    retx: false,
    probe: 0,
    summary: { kind: "response", status, reason: "R", cseq: { seq: 1, method: cseqMethod }, from: party, to: tagged },
    identities: { from: {}, to: {} },
    raw: "X"
  }) as unknown as Flows.Msg

/** The step a document transcribes `msg` into, as the cut states it. */
const stepOf = (msg: Flows.Msg): Flow.Step => {
  const s = msg.summary
  const spec =
    s.kind === "request"
      ? { method: s.method, cseq: s.cseq.seq }
      : { status: s.status, "cseq-method": s.cseq.method, cseq: s.cseq.seq }
  return {
    id: "s1",
    leg: "A",
    op: "send",
    ...(s.kind === "request" && s.method === "INVITE" && s.to.tag !== null ? { in_dialog: true } : {}),
    msg: spec,
    delay: { ms: 0, from: "start" }
  } as unknown as Flow.Step
}

const corpus: ReadonlyArray<Flows.Msg> = [
  request("INVITE", null),
  request("INVITE", "t1"),
  request("ACK", "t1"),
  request("BYE", "t1"),
  response(100, "INVITE"),
  response(180, "INVITE"),
  response(183, "INVITE"),
  response(180, "UPDATE"),
  response(200, "INVITE"),
  response(202, "INVITE"),
  response(200, "BYE"),
  response(486, "INVITE"),
  response(491, "INVITE")
]

describe("the facts read off a captured message", () => {
  it("a relayable provisional is 180–189 to an INVITE and nothing else", () => {
    expect(Flows.isProvisionalToInvite(response(180, "INVITE"))).toBe(true)
    expect(Flows.isProvisionalToInvite(response(100, "INVITE"))).toBe(false)
    expect(Flows.isProvisionalToInvite(response(180, "UPDATE"))).toBe(false)
  })

  it("a final ends the INVITE transaction, a 2xx establishes the dialog", () => {
    expect(Flows.isFinalToInvite(response(486, "INVITE"))).toBe(true)
    expect(Flows.isSuccessToInvite(response(486, "INVITE"))).toBe(false)
    expect(Flows.isSuccessToInvite(response(200, "INVITE"))).toBe(true)
    expect(Flows.isFinalToInvite(response(200, "BYE"))).toBe(false)
  })

  it("only an INVITE with no To-tag opens a dialog", () => {
    expect(Flows.opensDialog(request("INVITE", null))).toBe(true)
    expect(Flows.opensDialog(request("INVITE", "t1"))).toBe(false)
    expect(Flows.opensDialog(request("ACK", null))).toBe(false)
  })
})

describe("the two models state one claim", () => {
  it.each(corpus.map((msg) => [msg.summary.kind === "request" ? msg.summary.method : `${msg.summary.status} ${msg.summary.cseq.method}`, msg] as const))(
    "%s reads the same off the message and off its step",
    (_label, msg) => {
      const step = stepOf(msg)
      expect(Flow.isProvisionalToInvite(step)).toBe(Flows.isProvisionalToInvite(msg))
      expect(Flow.isFinalToInvite(step)).toBe(Flows.isFinalToInvite(msg))
      expect(Flow.isSuccessToInvite(step)).toBe(Flows.isSuccessToInvite(msg))
      expect(Flow.opensDialog(step)).toBe(Flows.opensDialog(msg))
      expect(Flow.inviteStatus(step)).toBe(Flows.inviteStatus(msg))
    }
  )
})
