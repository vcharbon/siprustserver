/**
 * THE CAPTURE'S OWN ORDER: a leg holding a message captured before a message
 * it can only follow.
 *
 * A tap that hands one direction's datagrams over late stamps a message after
 * the peer's reaction to it, and a file in strict time order keeps the
 * inversion: the send time is lost, and no reading of the file restores it. A
 * case cut from such a leg scripts an exchange no conformant party can play,
 * and a replay of it says nothing about the call the wire carried, so the case
 * is dropped under this token and the capture is never reordered.
 *
 * Four signatures, each judged within one leg (one Call-ID), across its hops:
 *
 * - a RESPONSE captured before the request it answers: the leg's earliest
 *   request of the same transaction — CSeq number and method, From-tag, and
 *   top Via branch wherever both carry one (RFC 3261 §17.1.3, §17.2.3) — is
 *   captured after it;
 * - an ACK captured before the final it acknowledges: the leg's earliest final
 *   to the INVITE of its CSeq number and From-tag is captured after it
 *   (§17.1.1.3, §13.2.2.4);
 * - a PRACK captured before the reliable provisional its RAck names: the leg's
 *   earliest 1xx to that request carrying that RSeq is captured after it
 *   (RFC 3262 §7.2);
 * - an in-dialog request captured before the INVITE that opens its dialog: a
 *   request carrying a To-tag whose two tags are the From-tag of a
 *   dialog-opening INVITE and the To-tag a captured response to that INVITE
 *   carries, the INVITE captured only after it (§12.1).
 *
 * Each signature needs the message followed in the capture: a leg that lost it
 * is left to the rules naming the loss. A lost original whose retransmission
 * crosses the reaction to it cannot be told from an inversion, so it reads as
 * misordered and is dropped too, as a capture fault, never repaired. An
 * in-dialog request captured after its INVITE and before the first response
 * carrying its tag is not judged: a lost provisional cannot be told from a late one.
 */
import type { Flows } from "@sip/contracts"
import type { CaseRule, RefusalInput } from "./case-rules.js"
import type { RefusalEvidence } from "./policy.js"
import { findingOf, type Finding } from "./refusal-rule.js"
import { headerValue } from "./wire.js"

/** The reason token for a case one of whose scripted legs the capture misordered. */
export const MISORDERED = "source-capture-misordered"

/** One message captured before the message it follows, both indices into the leg's `msgs`. */
export interface Inversion {
  /** The message captured too early. */
  readonly first: number
  /** The message it follows, captured after it. */
  readonly then: number
  /** What was seen, in one clause. */
  readonly detail: string
}

const isRequest = (m: Flows.Msg): boolean => m.summary.kind === "request"

const methodOf = (m: Flows.Msg): string => (m.summary.kind === "request" ? m.summary.method : "")

const statusOf = (m: Flows.Msg): number => (m.summary.kind === "response" ? m.summary.status : 0)

const topBranch = (m: Flows.Msg): string | null | undefined => m.via?.[0]?.branch

/** Whether `response` answers `request`: one transaction by CSeq, From-tag and, where both carry one, top Via branch. */
const answers = (response: Flows.Msg, request: Flows.Msg): boolean => {
  const r = response.summary
  const q = request.summary
  if (r.cseq.seq !== q.cseq.seq || r.cseq.method !== q.cseq.method || r.from.tag !== q.from.tag) return false
  const a = topBranch(response)
  const b = topBranch(request)
  return a == null || b == null || a === b
}

/** The first index satisfying `test`, or `undefined`. */
const firstWhere = (msgs: ReadonlyArray<Flows.Msg>, test: (m: Flows.Msg) => boolean): number | undefined => {
  const at = msgs.findIndex(test)
  return at < 0 ? undefined : at
}

/** Every response captured before the earliest request of its transaction; one per request. */
const responsesFirst = (msgs: ReadonlyArray<Flows.Msg>): ReadonlyArray<Inversion> => {
  const charged = new Set<number>()
  const out: Array<Inversion> = []
  msgs.forEach((m, i) => {
    if (isRequest(m)) return
    const then = firstWhere(msgs, (q) => isRequest(q) && answers(m, q))
    if (then === undefined || then < i || charged.has(then)) return
    charged.add(then)
    out.push({
      first: i,
      then,
      detail: `the ${statusOf(m)} to ${m.summary.cseq.method} (msg ${i}) is captured before the ` +
        `${m.summary.cseq.method} it answers (msg ${then})`
    })
  })
  return out
}

/** Every ACK captured before the earliest final to the INVITE it acknowledges. */
const acksFirst = (msgs: ReadonlyArray<Flows.Msg>): ReadonlyArray<Inversion> =>
  msgs.flatMap((m, i) => {
    if (methodOf(m) !== "ACK") return []
    const then = firstWhere(msgs, (r) =>
      !isRequest(r) && statusOf(r) >= 200 && r.summary.cseq.method === "INVITE" &&
      r.summary.cseq.seq === m.summary.cseq.seq && r.summary.from.tag === m.summary.from.tag)
    if (then === undefined || then < i) return []
    return [{ first: i, then, detail: `the ACK (msg ${i}) is captured before the final to the INVITE it acknowledges (msg ${then})` }]
  })

/** RAck's three fields (RFC 3262 §7.2): the RSeq acknowledged, and the CSeq number and method it answered. */
const rackOf = (m: Flows.Msg): { readonly rseq: number; readonly seq: number; readonly method: string } | undefined => {
  const parts = /^\s*(\d+)\s+(\d+)\s+(\S+)/.exec(headerValue(m, "RAck") ?? "")
  return parts === null ? undefined : { rseq: Number(parts[1]), seq: Number(parts[2]), method: parts[3]! }
}

const rseqOf = (m: Flows.Msg): number | undefined => {
  const value = /^\s*(\d+)/.exec(headerValue(m, "RSeq") ?? "")
  return value === null ? undefined : Number(value[1])
}

/** Every PRACK captured before the earliest reliable provisional its RAck names. */
const pracksFirst = (msgs: ReadonlyArray<Flows.Msg>): ReadonlyArray<Inversion> =>
  msgs.flatMap((m, i) => {
    if (methodOf(m) !== "PRACK") return []
    const rack = rackOf(m)
    if (rack === undefined) return []
    const then = firstWhere(msgs, (r) => {
      const status = statusOf(r)
      return !isRequest(r) && status > 100 && status < 200 && r.summary.cseq.seq === rack.seq &&
        r.summary.cseq.method === rack.method && r.summary.from.tag === m.summary.from.tag &&
        rseqOf(r) === rack.rseq
    })
    if (then === undefined || then < i) return []
    return [{ first: i, then, detail: `the PRACK (msg ${i}) is captured before the reliable provisional its RAck names (msg ${then})` }]
  })

/**
 * Whether the dialog-opening INVITE `invite` opened the dialog `tags` names:
 * its From-tag is one of them, and a captured response to it carries the other.
 */
const opened = (msgs: ReadonlyArray<Flows.Msg>, invite: Flows.Msg, tags: ReadonlyArray<string | null>): boolean => {
  const s = invite.summary
  if (s.kind !== "request" || s.method !== "INVITE" || s.to.tag !== null || s.from.tag === null) return false
  if (!tags.includes(s.from.tag)) return false
  return msgs.some((r) =>
    !isRequest(r) && r.summary.to.tag !== null && r.summary.to.tag !== s.from.tag &&
    tags.includes(r.summary.to.tag) && answers(r, invite))
}

/** Every in-dialog request captured before the earliest dialog-opening INVITE of the dialog it names. */
const dialogFirst = (msgs: ReadonlyArray<Flows.Msg>): ReadonlyArray<Inversion> =>
  msgs.flatMap((m, i) => {
    const s = m.summary
    if (s.kind !== "request" || s.to.tag === null || s.method === "ACK") return []
    const tags = [s.from.tag, s.to.tag]
    const then = firstWhere(msgs, (q) => opened(msgs, q, tags))
    if (then === undefined || then < i) return []
    return [{ first: i, then, detail: `the in-dialog ${s.method} (msg ${i}) is captured before the INVITE that opens its dialog (msg ${then})` }]
  })

/** Computed once per leg: the capture edge and this rule both ask, case after case. */
const known = new WeakMap<Flows.Leg, ReadonlyArray<Inversion>>()

/** Every causal inversion one leg holds, in capture order of the early message; empty where it holds none. */
export const inversionsOf = (leg: Flows.Leg): ReadonlyArray<Inversion> => {
  const cached = known.get(leg)
  if (cached !== undefined) return cached
  const msgs = leg.msgs
  const found = [...responsesFirst(msgs), ...acksFirst(msgs), ...pracksFirst(msgs), ...dialogFirst(msgs)]
    .sort((a, b) => a.first - b.first || a.then - b.then)
  known.set(leg, found)
  return found
}

/** The legs the case's vantages script that hold an inversion, one coordinate per inversion. */
export const misorderEvidence = (input: RefusalInput): ReadonlyArray<RefusalEvidence> => {
  const legs = [...new Set([input.spec.uac, ...input.spec.uas].map((v) => v.leg))].sort((a, b) => a - b)
  return legs.flatMap((index) => {
    const leg = input.flows.legs[index]!
    return inversionsOf(leg).map((inv) => ({
      leg: index,
      hop: leg.msgs[inv.first]!.hop,
      msg: inv.first,
      callId: leg.call_id,
      detail: inv.detail
    }))
  })
}

/** The refusal of a case whose scripted legs the capture misordered, or `undefined` for one in order. */
export const misorderFinding = (input: RefusalInput): Finding | undefined => {
  const evidence = misorderEvidence(input)
  if (evidence.length === 0) return undefined
  return findingOf(input.capture, input.caseId, MISORDERED, evidence, (e) => `leg ${e.leg} on '${e.callId}' ${e.detail}`)
}

/**
 * The rule: a case is dropped whole where a leg it scripts holds a causal
 * inversion. `untrusted`, because the drop claims nothing about whether the
 * call can run, so a green replay of the document it leaves contradicts
 * nothing it said.
 */
export const CAPTURE_ORDER_RULE: CaseRule = {
  id: MISORDERED,
  subject: "source",
  disposition: "untrusted",
  why: "a leg the case scripts holds a message captured before one it can only follow (a response before its request by branch and CSeq, an ACK before its final, a PRACK before the provisional its RAck names, an in-dialog request before its dialog's INVITE), so the capture's order of the call cannot be trusted",
  refuses: misorderFinding
}
