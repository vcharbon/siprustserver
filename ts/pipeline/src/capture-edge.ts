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
 * - a BYE closed the dialog, with or without its final. A BYE retransmitted
 *   without a final is one still being retried, so it terminates the leg only
 *   once {@link RETRANSMISSION_TAIL_US} separates the leg's last message from the
 *   end of the capture;
 * - a non-2xx final ended the attempt, whatever its ACK (RFC 3261 §17.1.1.3:
 *   that ACK belongs to the final's transaction and ends nothing the final did
 *   not);
 * - the source GAVE UP — a CANCEL, or a request nothing ever answered — and
 *   {@link GIVE_UP_SILENCE_US} separates the leg's last message from the end of
 *   the capture, so the silence is the source's own and not the file's.
 *
 * A dialog the source answered and never closed is refused whatever the
 * silence: a call can be up for hours. A leg that OPENS mid-dialog — a response,
 * or a request carrying a To-tag, before any request opened one — began before
 * the capture did and is refused as truncated at the head.
 *
 * A leg the SUT dialled is not judged at its tail once a BYE closed every
 * caller-side dialog of the case: its teardown is the SUT's own at replay, the
 * exemption `./call-completeness.ts` grants it.
 *
 * "End of the capture" is the last SIP datagram the document holds, which for
 * a per-call export is the call's own last message: a complete termination is
 * never charged for the file ending on it.
 */
import { Flows } from "@sip/contracts"
import { closedByBye } from "./call-completeness.js"
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

/**
 * 64·T1 (RFC 3261 §17.1.1.2 Timer B, §17.1.2.2 Timer F, §17.2.1 Timer H): the
 * longest any transaction waits for an answer before its own stack gives up. A
 * leg silent this long before the end of the capture is one the source gave up
 * on; one silent less than this may only be one the file cut short.
 */
export const GIVE_UP_SILENCE_US = 32_000_000

const lastDatagramOf = (leg: Flows.Leg): Flows.Msg | undefined => leg.msgs[leg.msgs.length - 1]

/**
 * The last SIP datagram of the whole capture, computed once per document: a
 * rule asked once per family that re-walked every leg each time would make the
 * cut quadratic in the number of calls.
 */
const captureEnds = new WeakMap<Flows.FlowsDoc, number>()

export const captureEndUs = (flows: Flows.FlowsDoc): number => {
  const known = captureEnds.get(flows)
  if (known !== undefined) return known
  let end = 0
  for (const leg of flows.legs) {
    const last = lastDatagramOf(leg)
    if (last !== undefined && last.ts_us > end) end = last.ts_us
  }
  captureEnds.set(flows, end)
  return end
}

const isRequest = (m: Flows.Msg): boolean => m.summary.kind === "request"

/** A final response to the transaction `method`, whatever the CSeq number. */
const isFinalTo = (m: Flows.Msg, method: string): boolean =>
  Flows.isResponseTo(m, method) && (Flows.statusOf(m) ?? 0) >= 200

const secondsBeforeEnd = (m: Flows.Msg, endUs: number): string =>
  `${((endUs - m.ts_us) / 1_000_000).toFixed(3)} s before the capture ends`

/**
 * The leg opened by something other than a fresh request: its first message
 * answers, or continues, a dialog the capture never saw open.
 */
const headTruncation = (leg: Flows.Leg, index: number): RefusalEvidence | undefined => {
  const first = leg.msgs[0]
  if (first === undefined) return undefined
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

/** What one leg needed and the capture did not hold; `undefined` where it is terminated. */
const missingTermination = (
  leg: Flows.Leg,
  index: number,
  endUs: number
): RefusalEvidence | undefined => {
  const msgs = leg.msgs
  const last = lastDatagramOf(leg)
  if (last === undefined) return undefined
  const lastIdx = msgs.length - 1
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

  // The establishing attempt: the last fresh INVITE opening a dialog, or, on a
  // leg that opens mid-dialog, the INVITE transaction its first answer names.
  // What the attempt drew decides the leg; one retried after a failure is
  // judged on the retry, and a dialog closed then re-opened on the one left
  // standing. A re-INVITE's answer, on the same dialog, decides nothing.
  let open = -1
  msgs.forEach((m, i) => {
    if (!m.retx && Flows.opensDialog(m)) open = i
  })
  const answer = open >= 0 ? -1 : msgs.findIndex((m) => Flows.isResponseTo(m, "INVITE"))
  if (open < 0 && answer < 0) {
    // No INVITE transaction here at all: the last fresh request is the
    // transaction the leg ends on.
    let req = -1
    msgs.forEach((m, i) => {
      if (!m.retx && isRequest(m) && !Flows.isMethod(m, "ACK")) req = i
    })
    if (req < 0) return undefined
    const request = msgs[req]!
    const method = request.summary.kind === "request" ? request.summary.method : ""
    if (msgs.some((m, i) => i > req && isFinalTo(m, method))) return undefined
    return gaveUp(`its ${method} (msg ${req}) drew no final`)
  }
  const from = open >= 0 ? open + 1 : answer
  const attempt = open >= 0 ? `its INVITE (msg ${open})` : "the INVITE before the capture"
  const seq = msgs[open >= 0 ? open : answer]!.summary.cseq.seq

  const after = (test: (m: Flows.Msg) => boolean): number => {
    for (let i = from; i < msgs.length; i++) if (test(msgs[i]!)) return i
    return -1
  }
  const bye = after((m) => Flows.isMethod(m, "BYE"))
  if (bye >= 0) {
    if (msgs.some((m, i) => i > bye && isFinalTo(m, "BYE"))) return undefined
    if (!msgs.some((m, i) => i > bye && m.retx && Flows.isMethod(m, "BYE"))) return undefined
    if (endUs - last.ts_us >= RETRANSMISSION_TAIL_US) return undefined
    return {
      leg: index,
      callId: leg.call_id,
      msg: bye,
      detail: `its BYE (msg ${bye}) drew no final and was retransmitted, and the leg's last message ` +
        `(msg ${lastIdx}) is ${secondsBeforeEnd(last, endUs)}: bye-unanswered-at-edge, inside the ` +
        `${RETRANSMISSION_TAIL_US / 1_000_000} s retransmission tail`
    }
  }

  let final = -1
  msgs.forEach((m, i) => {
    if (i >= from && !m.retx && Flows.isFinalToInvite(m) && m.summary.cseq.seq === seq) final = i
  })
  if (final < 0) {
    const cancel = after((m) => Flows.isMethod(m, "CANCEL"))
    return gaveUp(
      cancel >= 0
        ? `${attempt} was cancelled (msg ${cancel}) and drew no final`
        : `${attempt} drew no final`
    )
  }
  const status = Flows.statusOf(msgs[final]!) ?? 0
  if (status >= 300) return undefined
  return {
    leg: index,
    callId: leg.call_id,
    msg: final,
    detail: `took the ${status} (msg ${final}) answering ${attempt} and no BYE closed the ` +
      `dialog: dialog-left-open`
  }
}

/**
 * The legs the SUT dialled whose tail is exempt: every leg a vantage anchors
 * outbound and none anchors inbound, where the case has a caller-side vantage
 * and a BYE closed the dialog at each.
 */
const exemptTails = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  vantages: ReadonlyArray<CutVantage>
): ReadonlySet<number> => {
  const inbound = vantages.filter((v) => vantageIsInbound(flows, sut, v))
  if (inbound.length === 0 || !inbound.every((v) => closedByBye(flows, sut, v))) return new Set()
  const callerLegs = new Set(inbound.map((v) => v.leg))
  return new Set(
    vantages.filter((v) => !callerLegs.has(v.leg) && vantageIsOutbound(flows, sut, v)).map((v) => v.leg)
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
  const endUs = captureEndUs(flows)
  const exempt = exemptTails(flows, sut, vantages)
  const legs = [...new Set(vantages.map((v) => v.leg))].sort((a, b) => a - b)
  return legs.flatMap((index) => {
    const leg = flows.legs[index]!
    const tail = exempt.has(index) ? undefined : missingTermination(leg, index, endUs)
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
