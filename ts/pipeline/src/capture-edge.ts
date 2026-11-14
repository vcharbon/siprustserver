/**
 * THE CAPTURE EDGE: a case the file cut through is no full call.
 *
 * A capture of a live tap starts and stops on the clock, not on a call, so the
 * calls under way at either edge are in it by half. A case cut from one would
 * script a call the wire never finished, and its count would be a full call the
 * population never held — so the case is refused, under a token that names
 * the leg and the termination it is missing, and never dropped.
 *
 * The legs judged are the ones the case's vantages SCRIPT: a leg the
 * correlation joined to the family and no vantage plays is in no replay. A case
 * is complete when every such leg is terminated inside the capture:
 *
 * - a BYE closed the dialog, with or without its final. The leg's last fresh
 *   BYE is the one judged; retransmitted without a final of its own, it is
 *   still being retried, so it terminates the leg only
 *   once {@link RETRANSMISSION_TAIL_US} separates the leg's last message from the
 *   end of the capture;
 * - a non-2xx final ended the attempt, whatever its ACK (RFC 3261 §17.1.1.3:
 *   that ACK belongs to the final's transaction and ends nothing the final did
 *   not), unless a fresh 2xx answered the same attempt. An attempt the leg
 *   relays over several hops is answered where any of them answered it;
 * - the source GAVE UP — a CANCEL, or a request nothing ever answered — and
 *   {@link GIVE_UP_SILENCE_US} separates the leg's last message from the end of
 *   the capture, so the silence is the source's own and not the file's.
 *
 * A dialog the source answered and never closed is refused whatever the
 * silence: a call can be up for hours. A leg that OPENS mid-dialog — a response,
 * or a request carrying a To-tag, before any request opened one — began before
 * the capture did and is refused as truncated at the head, unless the leg
 * captures what that first message follows later on, which is a misordered
 * leg (`./capture-order.ts`) and not a truncated one.
 *
 * A leg the SUT dialled before the call's teardown is not judged at its tail
 * once a BYE closed every caller-side dialog of the case: its teardown is the
 * SUT's own at replay, the exemption `./call-completeness.ts` grants it.
 *
 * "End of the capture" is the last SIP datagram the document holds, which for
 * a per-call export is the call's own last message: a complete termination is
 * never charged for the file ending on it.
 */
import type { Flows } from "@sip/contracts"
import { closingBye } from "./call-completeness.js"
import * as CallFacts from "./call-facts.js"
import { captureEndUs, GIVE_UP_SILENCE_US } from "./call-facts.js"
import { inversionsOf } from "./capture-order.js"
import type { CaseRule, RefusalInput } from "./case-rules.js"
import { type CutVantage, vantageIsInbound, vantageIsOutbound } from "./cut.js"
import { findingOf, type Finding } from "./refusal-rule.js"
import type { RefusalEvidence } from "./policy.js"
import type { SutSet } from "./sut.js"

/** The reason token for a case the capture edge cut through. */
export const TRUNCATED_AT_EDGE = "source-truncated-at-edge"

/**
 * T2 (RFC 3261 §17.1.2.2): the longest interval between two retransmissions of
 * a request, so a BYE whose transaction was still alive at the end of the
 * capture has a retransmission inside this tail.
 */
export const RETRANSMISSION_TAIL_US = 4_000_000

export { captureEndUs, GIVE_UP_SILENCE_US }

const lastDatagramOf = (leg: Flows.Leg): Flows.Msg | undefined => leg.msgs[leg.msgs.length - 1]

const secondsBeforeEnd = (m: Flows.Msg, endUs: number): string =>
  `${((endUs - m.ts_us) / 1_000_000).toFixed(3)} s before the capture ends`

/**
 * The leg opened by something other than a fresh request: its first message
 * answers, or continues, a dialog the capture never saw open. A first message
 * captured before the message it follows is misordered (`./capture-order.ts`),
 * not truncated: what it follows is in the capture.
 */
const headTruncation = (leg: Flows.Leg, index: number): RefusalEvidence | undefined => {
  const first = leg.msgs[0]
  if (first === undefined) return undefined
  if (inversionsOf(leg).some((inv) => inv.first === 0)) return undefined
  const summary = first.summary
  if (summary.kind === "response") {
    return {
      leg: index,
      callId: leg.call_id,
      msg: 0,
      detail: `opens on the ${summary.status} to ${summary.cseq.method} (msg 0): head-truncated, ` +
        `the request it answers precedes the capture`
    }
  }
  if (summary.to.tag !== null) {
    return {
      leg: index,
      callId: leg.call_id,
      msg: 0,
      detail: `opens on an in-dialog ${summary.method} (msg 0): head-truncated, the dialog ` +
        `began before the capture`
    }
  }
  return undefined
}

/**
 * The transaction a leg's termination is judged on: its outcome, or, on a leg
 * that opens mid-dialog, the INVITE transaction its first answer names, with
 * the index its reading starts at.
 */
const attemptOf = (
  facts: CallFacts.LegFacts
): { readonly transaction: CallFacts.Transaction; readonly from: number; readonly what: string } | undefined => {
  const outcome = facts.outcome
  if (outcome?.request !== undefined) {
    return { transaction: outcome, from: outcome.request.msg + 1, what: `its INVITE (msg ${outcome.request.msg})` }
  }
  const answer = facts.timeline.entries.find((e) => e.method === "INVITE" && e.status !== undefined)
  if (answer === undefined) return undefined
  return {
    transaction: facts.timeline.transactions[answer.transaction]!,
    from: answer.msg,
    what: "the INVITE before the capture"
  }
}

/**
 * The final a leg's attempt drew. A leg relays its INVITE hop by hop under one
 * CSeq number (RFC 3261 §16.6), and a capture may hold the answer of one hop
 * and not of another: the attempt is every dialog-opening transaction that
 * carries the judged one's number, and its final is the first 2xx any of them
 * drew from `from` on (§16.7), else a final one of them drew from there.
 */
const attemptFinal = (
  facts: CallFacts.LegFacts,
  transaction: CallFacts.Transaction,
  from: number
): CallFacts.Entry | undefined => {
  const relayed = transaction.opening ? facts.openings.filter((t) => t.seq === transaction.seq) : [transaction]
  const finals = relayed
    .flatMap((t) => t.responses)
    .filter((r) => (r.status ?? 0) >= 200 && r.msg >= from)
    .sort((a, b) => a.msg - b.msg)
  return finals.find((r) => (r.status ?? 0) < 300) ?? finals[finals.length - 1]
}

/** What one leg needed and the capture did not hold; `undefined` where it is terminated. */
const missingTermination = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  index: number
): RefusalEvidence | undefined => {
  const facts = CallFacts.atLeg(flows, sut, index)
  const { leg, entries, transactions } = facts.timeline
  const last = lastDatagramOf(leg)
  if (last === undefined) return undefined
  const lastIdx = leg.msgs.length - 1
  const endUs = captureEndUs(flows)
  const gaveUp = (what: string): RefusalEvidence | undefined =>
    endUs - last.ts_us >= GIVE_UP_SILENCE_US
      ? undefined
      : {
        leg: index,
        callId: leg.call_id,
        msg: lastIdx,
        detail: `${what}, and its last message (msg ${lastIdx}) is ` +
          `${secondsBeforeEnd(last, endUs)}: gave-up-at-edge, less than the ` +
          `${GIVE_UP_SILENCE_US / 1_000_000} s give-up silence`
      }

  const attempt = attemptOf(facts)
  if (attempt === undefined) {
    const request = entries.findLast((e) => e.status === undefined && e.method !== "ACK")
    if (request === undefined) return undefined
    if (transactions[request.transaction]!.final !== undefined) return undefined
    return gaveUp(`its ${methodOf(leg, request)} (msg ${request.msg}) drew no final`)
  }
  const { transaction, from, what } = attempt

  const bye = transactions.findLast((t) => t.method === "BYE" && t.request !== undefined && t.request.msg >= from)
  if (bye?.request !== undefined) {
    if (bye.final !== undefined || bye.request.retransmissions === 0) return undefined
    if (endUs - last.ts_us >= RETRANSMISSION_TAIL_US) return undefined
    return {
      leg: index,
      callId: leg.call_id,
      msg: bye.request.msg,
      detail: `its BYE (msg ${bye.request.msg}) drew no final and was retransmitted, and the leg's last message ` +
        `(msg ${lastIdx}) is ${secondsBeforeEnd(last, endUs)}: bye-unanswered-at-edge, inside the ` +
        `${RETRANSMISSION_TAIL_US / 1_000_000} s retransmission tail`
    }
  }

  const final = attemptFinal(facts, transaction, from)
  if (final === undefined) {
    const cancel = transaction.cancel ?? entries.find((e) => e.method === "CANCEL" && e.status === undefined && e.msg >= from)
    return gaveUp(
      cancel !== undefined
        ? `${what} was cancelled (msg ${cancel.msg}) and drew no final`
        : `${what} drew no final`
    )
  }
  const status = final.status ?? 0
  if (status >= 300) return undefined
  return {
    leg: index,
    callId: leg.call_id,
    msg: final.msg,
    detail: `took the ${status} (msg ${final.msg}) answering ${what} and no BYE closed the ` +
      `dialog: dialog-left-open`
  }
}

/** The method of a request entry as the wire spells it. */
const methodOf = (leg: Flows.Leg, entry: CallFacts.Entry): string => {
  const summary = leg.msgs[entry.msg]!.summary
  return summary.kind === "request" ? summary.method : entry.method
}

/**
 * The legs the SUT dialled whose tail is exempt: every leg a vantage anchors
 * outbound and none anchors inbound, where the case has a caller-side vantage,
 * a BYE closed the dialog at each, and the leg's first message precedes the
 * earliest of those BYEs — a leg dialled once the call was being torn down is
 * no part of the call the teardown ends.
 */
const exemptTails = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  vantages: ReadonlyArray<CutVantage>
): ReadonlySet<number> => {
  const inbound = vantages.filter((v) => vantageIsInbound(flows, sut, v))
  if (inbound.length === 0) return new Set()
  const byes = inbound.map((v) => closingBye(flows, sut, v))
  if (byes.some((b) => b === undefined)) return new Set()
  const teardownUs = Math.min(...byes.map((b) => b!.ts_us))
  const callerLegs = new Set(inbound.map((v) => v.leg))
  return new Set(
    vantages
      .filter((v) => !callerLegs.has(v.leg) && vantageIsOutbound(flows, sut, v))
      .filter((v) => (flows.legs[v.leg]!.msgs[0]?.ts_us ?? Infinity) < teardownUs)
      .map((v) => v.leg)
  )
}

/**
 * Every leg the vantages script that the capture edge cut through, with what
 * each is missing; empty where the case is a full call.
 */
export const edgeEvidence = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  vantages: ReadonlyArray<CutVantage>
): ReadonlyArray<RefusalEvidence> => {
  const exempt = exemptTails(flows, sut, vantages)
  const legs = [...new Set(vantages.map((v) => v.leg))].sort((a, b) => a - b)
  return legs.flatMap((index) => {
    const leg = flows.legs[index]!
    const tail = exempt.has(index) ? undefined : missingTermination(flows, sut, index)
    return [headTruncation(leg, index), tail].flatMap((e) => (e === undefined ? [] : [e]))
  })
}

/** The refusal of a case the capture edge cut through, or `undefined` for a full call. */
export const edgeFinding = (input: RefusalInput): Finding | undefined => {
  const evidence = edgeEvidence(input.flows, input.sut, [input.spec.uac, ...input.spec.uas])
  if (evidence.length === 0) return undefined
  return findingOf(
    input.capture,
    input.caseId,
    TRUNCATED_AT_EDGE,
    evidence,
    (e) => `leg ${e.leg} on '${e.callId}' ${e.detail}`
  )
}

/** The rule: a case is refused whole where the capture edge cut through any leg it scripts. */
export const CAPTURE_EDGE_RULE: CaseRule = {
  id: TRUNCATED_AT_EDGE,
  subject: "source",
  disposition: "refuses",
  why: "a leg the case scripts opens before the capture starts or is left unterminated at its end, so the call is not whole",
  refuses: edgeFinding
}
