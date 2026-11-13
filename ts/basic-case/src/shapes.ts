/**
 * The two shapes a basic case plays, written on the step builder
 * (`flow.ts`). Leg `A` is the caller's, toward the system under test; leg `B`
 * the callee's, from it. Both open with the caller's INVITE (offer in its
 * body), the system's 100 Trying recorded, and the INVITE reaching the callee.
 *
 * - `basic_call`: the callee rings (180), answers (200, its answer in the
 *   body), both ACKs pass, and after the hold the party `ends` names sends
 *   BYE, which the other answers 200 (RFC 3261 §13, §15).
 * - `invite_reject`: the callee answers the INVITE with the shape's status,
 *   the system ACKs it on leg B, the caller receives the final and ACKs it
 *   (RFC 3261 §17.1.1.3).
 *
 * The caller's final carries the case's final checks. An operation step is
 * injected at its point: `reached` after the callee's INVITE, `answered` after
 * the callee's ACK of its 200; the next message sent waits for it. A step
 * that forgets the call ends it on 481s: the hanging-up party's BYE answered
 * 481, then the other party's own BYE answered 481 (RFC 3261 §15.1.2).
 */
import type { Check, Msg } from "@sip/contracts"
import { OperationStep } from "@sip/rig"
import { type BasicCall, type BasicCase, DEFAULT_REJECT, type InviteReject, type Point, REJECTS } from "./case.js"
import { CallFlow } from "./flow.js"
import { ANSWER, OFFER, REWRITE } from "./media.js"

/**
 * The methods each party accepts (RFC 3261 §20.5) and the extensions it
 * supports, none (§20.37), stated on its INVITE and its 2xx (§13.2.1).
 */
const CAPABILITIES: ReadonlyArray<Msg.Header> = [
  { name: "Allow", value: "INVITE, ACK, CANCEL, BYE, OPTIONS" },
  { name: "Supported", value: "" }
]

/** How long the callee rings before it answers, and how long the answered call is held, in ms. */
const RING_MS = 40
const HOLD_MS = 200

/** How long the callee takes to answer an INVITE it received, in ms. */
const ANSWER_DELAY_MS = 20

/** The reason phrase of a 481 (RFC 3261 §21.4.19). */
const GONE = "Call/Transaction Does Not Exist"

/** The operation step `c` states at `point`, injected once the step `reached` completed. */
const stepAt = (flow: CallFlow, c: BasicCase, point: Point, reached: string): void => {
  for (const step of c.steps ?? []) {
    if (step.at === point) flow.inject(OperationStep.actionOf(step.operation), reached)
  }
}

/** The checks the case states on the caller's final. */
const finalChecksOf = (c: BasicCase): ReadonlyArray<Check.Check> =>
  c.final.map((check) => ({ field: check.field, op: "eq", value: String(check.eq) }))

/** The INVITE reaching the callee from the caller, through the system: the id of the callee's INVITE step. */
const dial = (flow: CallFlow, c: BasicCase): string => {
  const invite = flow.send("A", {
    method: "INVITE",
    ruri: { frozen: c.callee },
    from: { frozen: c.caller },
    to: { frozen: c.callee },
    headers: [...CAPABILITIES],
    body: { ref: OFFER, rewrite: [...REWRITE] }
  })
  flow.expect("A", { status: 100, reason: "Trying", "cseq-method": "INVITE", cseq: 1 }, { from: invite, auto: true })
  const reached = flow.expect("B", { method: "INVITE", body: { mode: "sdp-present" } }, { from: invite })
  stepAt(flow, c, "reached", reached)
  return reached
}

const basicCall = (flow: CallFlow, c: BasicCall): void => {
  const reached = dial(flow, c)
  const ringing = flow.send("B", { status: 180, reason: "Ringing", "cseq-method": "INVITE" }, { from: reached, ms: ANSWER_DELAY_MS })
  flow.expect("A", { status: 180, reason: "Ringing", "cseq-method": "INVITE" }, { from: ringing })
  const ok = flow.send("B", {
    status: 200,
    reason: "OK",
    "cseq-method": "INVITE",
    headers: [...CAPABILITIES],
    body: { ref: ANSWER, rewrite: [...REWRITE] }
  }, { from: ringing, ms: RING_MS })
  const answered = flow.expect("A", { status: 200, reason: "OK", "cseq-method": "INVITE", body: { mode: "sdp-present" } }, {
    from: ok,
    checks: finalChecksOf(c)
  })
  const ack = flow.send("A", { method: "ACK", cseq: 1 }, { from: answered, auto: true })
  const acked = flow.expect("B", { method: "ACK", cseq: 1 }, { from: ack, auto: true })
  stepAt(flow, c, "answered", acked)
  const [ender, other] = (c.params?.ends ?? "caller") === "caller" ? ["A", "B"] : ["B", "A"]
  const bye = flow.send(ender, { method: "BYE" }, { from: acked, ms: HOLD_MS })
  if ((c.steps ?? []).some((step) => step.at === "answered" && step.forgets === true)) return forgotten(flow, ender, other, bye)
  const byeIn = flow.expect(other, { method: "BYE" }, { from: bye })
  const byeOk = flow.send(other, { status: 200, reason: "OK", "cseq-method": "BYE" }, { from: byeIn })
  flow.expect(ender, { status: 200, reason: "OK", "cseq-method": "BYE" }, { from: byeOk })
}

/**
 * The end of a call the system forgot: the `ender`'s BYE (step `bye`) is
 * answered 481, then the `other` party ends its own leg the same way.
 */
const forgotten = (flow: CallFlow, ender: string, other: string, bye: string): void => {
  const gone = flow.expect(ender, { status: 481, reason: GONE, "cseq-method": "BYE" }, { from: bye })
  const otherBye = flow.send(other, { method: "BYE" }, { from: gone })
  flow.expect(other, { status: 481, reason: GONE, "cseq-method": "BYE" }, { from: otherBye })
}

const inviteReject = (flow: CallFlow, c: InviteReject): void => {
  const reached = dial(flow, c)
  const status = c.params?.status ?? DEFAULT_REJECT
  const reject = flow.send("B", { status, reason: REJECTS[status], "cseq-method": "INVITE" }, { from: reached, ms: ANSWER_DELAY_MS })
  flow.expect("B", { method: "ACK", cseq: 1 }, { from: reject, auto: true })
  const final = flow.expect("A", { status, reason: REJECTS[status], "cseq-method": "INVITE" }, {
    from: reject,
    checks: finalChecksOf(c)
  })
  flow.send("A", { method: "ACK", cseq: 1 }, { from: final, auto: true })
}

/** The steps of `c`'s call. */
export const flowOf = (c: BasicCase): CallFlow => {
  const flow = new CallFlow()
  if (c.shape === "basic_call") basicCall(flow, c)
  else inviteReject(flow, c)
  return flow
}
