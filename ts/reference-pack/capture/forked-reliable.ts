/**
 * The case the reference capture records (`README.md`, "The reference
 * capture"): Alice calls Bob, whose side forks the INVITE. Each fork rings
 * reliably (RFC 3262: a 18x requiring `100rel`, its own RSeq, PRACKed and the
 * PRACK answered 200) on an early dialog of its own To-tag; the first fork
 * answers, the system ACKs it, and the second fork's early dialog ends with the
 * INVITE's transaction, no final reaching the system from it, as the forking
 * proxy cancelled it (RFC 3261 §13.2.2.4, §16.7). Alice hangs up.
 *
 * Leg `B` is the callee side as the system sees it: one INVITE out, the two
 * forks back (`f1`, `f2`). Leg `A` is the caller, who sees the forks under the
 * tags the system mints for them (`r1`, `r2`).
 */
import { ANSWER, callDocumentOf, CallFlow, idOf, OFFER, RESOURCES, REWRITE } from "@sip/basic-case"
import type { CompiledCell } from "@sip/rig"

/** A message as the step builder states it. */
type MsgSpec = Parameters<CallFlow["send"]>[1]

/** The case's title, and the cell's name. */
export const TITLE = "a forking callee rings reliably twice and answers once"

const CALLER = "sip:alice@atlanta.example.com"
const CALLEE = "sip:bob@biloxi.example.com"

/** The methods each party accepts (RFC 3261 §20.5) and its support of reliable provisionals (RFC 3262 §3). */
const CAPABILITIES: NonNullable<MsgSpec["headers"]> = [
  { name: "Allow", value: "INVITE, ACK, CANCEL, BYE, PRACK, OPTIONS" },
  { name: "Supported", value: "100rel" }
]

/** How long each fork takes to ring and to answer, and how long the answered call is held, in ms. */
const RING_MS = 20
const ANSWER_MS = 40
const HOLD_MS = 100

/** A reliable provisional of a fork: `status` requiring `100rel`, numbered `rseq` in the fork's own space (RFC 3262 §3). */
const reliable = (status: 180 | 183, rseq: number): MsgSpec => ({
  status,
  reason: status === 180 ? "Ringing" : "Session Progress",
  "cseq-method": "INVITE",
  headers: [{ name: "Require", value: "100rel" }, { name: "RSeq", value: String(rseq) }]
})

/**
 * One fork ringing reliably from `after`: its provisional on its own early
 * dialog `fork`, relayed to the caller on `relayed`, the caller's PRACK on
 * that dialog reaching the fork, and the fork's 200 to it reaching the
 * caller. The id of the caller's step receiving that 200.
 */
const ringReliably = (
  flow: CallFlow,
  after: string,
  fork: string,
  relayed: string,
  provisional: MsgSpec
): string => {
  const rung = flow.send("B", provisional, { from: after, ms: RING_MS, early: fork })
  const heard = flow.expect("A", { status: provisional.status!, reason: provisional.reason!, "cseq-method": "INVITE" }, {
    from: rung,
    early: relayed
  })
  const prack = flow.send("A", { method: "PRACK" }, { from: heard, early: relayed })
  const pracked = flow.expect("B", { method: "PRACK" }, { from: prack, early: fork })
  const ok = flow.send("B", { status: 200, reason: "OK", "cseq-method": "PRACK" }, { from: pracked, early: fork })
  return flow.expect("A", { status: 200, reason: "OK", "cseq-method": "PRACK" }, { from: ok, early: relayed })
}

/** The steps of the call. */
const flowOf = (): CallFlow => {
  const flow = new CallFlow()
  const invite = flow.send("A", {
    method: "INVITE",
    ruri: { frozen: CALLEE },
    from: { frozen: CALLER },
    to: { frozen: CALLEE },
    headers: [...CAPABILITIES],
    body: { ref: OFFER, rewrite: [...REWRITE] }
  })
  flow.expect("A", { status: 100, reason: "Trying", "cseq-method": "INVITE", cseq: 1 }, { from: invite, auto: true })
  const reached = flow.expect("B", { method: "INVITE", body: { mode: "sdp-present" } }, { from: invite })
  const first = ringReliably(flow, reached, "f1", "r1", reliable(183, 1))
  const second = ringReliably(flow, first, "f2", "r2", reliable(180, 101))
  const ok = flow.send("B", {
    status: 200,
    reason: "OK",
    "cseq-method": "INVITE",
    headers: [...CAPABILITIES],
    body: { ref: ANSWER, rewrite: [...REWRITE] }
  }, { from: second, ms: ANSWER_MS, early: "f1" })
  const answered = flow.expect("A", { status: 200, reason: "OK", "cseq-method": "INVITE", body: { mode: "sdp-present" } }, {
    from: ok
  })
  const ack = flow.send("A", { method: "ACK", cseq: 1 }, { from: answered, auto: true })
  const acked = flow.expect("B", { method: "ACK", cseq: 1 }, { from: ack, auto: true, early: "f1" })
  const bye = flow.send("A", { method: "BYE" }, { from: acked, ms: HOLD_MS })
  const byeIn = flow.expect("B", { method: "BYE" }, { from: bye })
  const byeOk = flow.send("B", { status: 200, reason: "OK", "cseq-method": "BYE" }, { from: byeIn })
  flow.expect("A", { status: 200, reason: "OK", "cseq-method": "BYE" }, { from: byeOk })
  return flow
}

/** The cell, claiming the one call record the system writes of the call. */
const cell: CompiledCell.CompiledCell = {
  id: idOf(TITLE),
  case: TITLE,
  title: TITLE,
  document: callDocumentOf(
    { title: TITLE, caller: CALLER, callee: CALLEE, note: "a forking callee side, reliable provisionals" },
    flowOf().steps
  ),
  resources: RESOURCES,
  claims: { "call-records": { count: 1 } }
}

export default [cell]
