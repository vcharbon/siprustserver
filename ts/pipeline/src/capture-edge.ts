/**
 * THE CAPTURE EDGE: a family the file cut through is no full call.
 *
 * A capture of a live tap starts and stops on the clock, not on a call, so the
 * calls under way at either edge are in it by half. A case cut from one would
 * script a call the wire never finished, and its count would be a full call the
 * population never held — so the family is refused, under a token that names
 * the leg and the termination it is missing, and never dropped.
 *
 * A family is complete when EVERY leg is terminated inside the capture:
 *
 * - a BYE closed the dialog. Where the BYE drew no final, the leg is terminated
 *   only once {@link RETRANSMISSION_TAIL_US} separates its last message from the
 *   end of the capture: a BYE still being retried shows within that tail;
 * - a non-2xx final ended the attempt and its ACK is in the capture;
 * - the source GAVE UP — a CANCEL, a request nothing ever answered, a final
 *   nobody ACKed — and {@link GIVE_UP_SILENCE_US} separates the leg's last
 *   message from the end of the capture, so the silence is the source's own
 *   and not the file's.
 *
 * A dialog the source answered and never closed is refused whatever the
 * silence: a call can be up for hours. A leg that OPENS mid-dialog — a response,
 * or a request carrying a To-tag, before any request opened one — began before
 * the capture did and is refused as truncated at the head.
 *
 * "End of the capture" is the last SIP datagram the document holds, which for
 * a per-call export is the call's own last message: a complete termination is
 * never charged for the file ending on it.
 */
import { Flows } from "@sip/contracts"
import type { CaptureRule, CaptureInput } from "./capture-rules.js"
import { findingOf, type Finding } from "./refusal-rule.js"
import type { RefusalEvidence } from "./policy.js"

/** The reason token for a family the capture edge cut through. */
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
    if (endUs - last.ts_us >= RETRANSMISSION_TAIL_US) return undefined
    return {
      leg: index,
      callId: leg.call_id,
      msg: bye,
      detail: `its BYE (msg ${bye}) drew no final and the leg's last message (msg ${lastIdx}) is ` +
        `${secondsBeforeEnd(last, endUs)}: bye-unanswered-at-edge, inside the ` +
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
  if (status < 300) {
    return {
      leg: index,
      callId: leg.call_id,
      msg: final,
      detail: `took the ${status} (msg ${final}) answering ${attempt} and no BYE closed the ` +
        `dialog: dialog-left-open`
    }
  }
  if (msgs.some((m, i) => i > final && Flows.isMethod(m, "ACK") && m.summary.cseq.seq === seq)) {
    return undefined
  }
  return gaveUp(`its ${status} to INVITE (msg ${final}) was never ACKed`)
}

/**
 * Every leg of the family the capture edge cut through, with what each is
 * missing; empty where the family is a full call.
 */
export const edgeEvidence = (
  flows: Flows.FlowsDoc,
  legs: ReadonlyArray<number>
): ReadonlyArray<RefusalEvidence> => {
  const endUs = captureEndUs(flows)
  return legs.flatMap((index) => {
    const leg = flows.legs[index]
    if (leg === undefined) return []
    return [headTruncation(leg, index), missingTermination(leg, index, endUs)].flatMap((e) =>
      e === undefined ? [] : [e]
    )
  })
}

/** The refusal of a family the capture edge cut through, or `undefined` for a full call. */
export const edgeFinding = (input: CaptureInput): Finding | undefined => {
  const evidence = edgeEvidence(input.flows, input.legs)
  if (evidence.length === 0) return undefined
  return findingOf(
    input.capture,
    input.caseId,
    TRUNCATED_AT_EDGE,
    evidence,
    (e) => `leg ${e.leg} on '${e.callId}' ${e.detail}`
  )
}

/** The rule: a family is refused whole where the capture edge cut through any leg of it. */
export const CAPTURE_EDGE_RULE: CaptureRule = {
  id: TRUNCATED_AT_EDGE,
  subject: "source",
  disposition: "refuses",
  why: "a leg of the call opens before the capture starts or is left unterminated at its end, so the call is not whole",
  refuses: edgeFinding
}
