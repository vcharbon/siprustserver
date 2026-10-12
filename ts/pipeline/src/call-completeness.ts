/**
 * An INCOMPLETE source call is no replay case. Completeness is the CALLER's
 * dialog: where the external peer's dialog with the SUT was ESTABLISHED in the
 * capture and never TERMINATED there, or where the capture holds no final
 * answering its INVITE at all, the call never finished and a replay would
 * re-attempt a call the source never closed. A dialog the SUT dialled out
 * itself is exempt — its teardown is the system under test's own at replay —
 * and a FAILED attempt is complete.
 */
import { Flows } from "@sip/contracts"
import type { CaseRule } from "./case-rules.js"
import { type CutVantage, vantageIsInbound, vantageLeg, vantageMsgIdxs } from "./cut.js"
import type { RefusalEvidence } from "./policy.js"
import { type Finding, findingOf } from "./refusal-rule.js"
import type { SutSet } from "./sut.js"

/** The reason token for a call the capture established and never finished. */
export const SOURCE_CALL_INCOMPLETE = "source-call-incomplete"

/**
 * The dialog at one vantage where the capture ESTABLISHED it and never tore it
 * down, and `undefined` for every complete outcome.
 *
 * Read off the datagrams the CUT gives this vantage rather than off the leg's
 * `final_status` / `terminated_by` summary: those are leg-wide, so on a leg the
 * SUT shows at two boundary hops they answer about the wrong dialog.
 *
 * The LAST fresh 2xx is the establishing one, so an attempt retried after a
 * failure is judged on the attempt that succeeded and a 2xx retransmitted after
 * the teardown does not re-open the dialog. Terminated means a BYE after it: RFC
 * 3261 §9.1 gives CANCEL no effect on a dialog whose 2xx is already out, and §15
 * makes BYE the only way such a dialog ends.
 */
const unterminatedDialog = (flows: Flows.FlowsDoc, sut: SutSet, v: CutVantage): RefusalEvidence | undefined => {
  const leg = vantageLeg(flows, v)
  const idxs = vantageMsgIdxs(flows, v.leg, v.hop, sut)
  const answered = idxs.filter((i) => !leg.msgs[i]!.retx && Flows.isSuccessToInvite(leg.msgs[i]!)).pop()
  if (answered === undefined) return undefined
  if (idxs.some((i) => i > answered && Flows.isMethod(leg.msgs[i]!, "BYE"))) return undefined
  const summary = leg.msgs[answered]!.summary
  const status = summary.kind === "response" ? summary.status : 0
  return {
    leg: v.leg,
    hop: v.hop,
    callId: leg.call_id,
    msg: answered,
    detail:
      `took the ${status} answering its INVITE (msg ${answered}) and no BYE ever closed ` +
      `the dialog in the capture`
  }
}

/**
 * The dialog at one vantage whose opening INVITE the capture holds no final
 * for, and `undefined` wherever one arrived.
 *
 * The capture stopped inside the INVITE's own transaction, so the call has no
 * outcome at all: a case cut from it scripts the offer and nothing that answers
 * it, and the replay hangs on a call the source never resolved. A `100 Trying`
 * ends nothing (RFC 3261 §17.1.1.2) and no provisional counts.
 *
 * The LAST fresh dialog-creating INVITE is the attempt under judgement, matching
 * {@link unterminatedDialog}: a call retried after a failure is judged on the
 * retry.
 */
export const unfinalledInvite = (flows: Flows.FlowsDoc, sut: SutSet, v: CutVantage): RefusalEvidence | undefined => {
  const leg = vantageLeg(flows, v)
  const idxs = vantageMsgIdxs(flows, v.leg, v.hop, sut)
  const invited = idxs.filter((i) => !leg.msgs[i]!.retx && Flows.opensDialog(leg.msgs[i]!)).pop()
  if (invited === undefined) return undefined
  if (idxs.some((i) => i > invited && Flows.isFinalToInvite(leg.msgs[i]!))) return undefined
  return {
    leg: v.leg,
    hop: v.hop,
    callId: leg.call_id,
    msg: invited,
    detail: `took the INVITE opening the dialog (msg ${invited}) and no final response to it ` +
      `was ever captured`
  }
}

/**
 * Whether the case cut at `vantages` may be generated, or the source call it
 * would replay never finished.
 *
 * Two ways a call is unfinished, both read at the same vantages: the capture
 * stopped before the INVITE's final, or it stopped after the answer and before
 * the BYE. One evidence list, because either way the outcome is the same — the
 * source never closed the call.
 *
 * Only the CALLER-side ingress vantages are judged — every vantage whose
 * dialog-creating INVITE arrived at the SUT. A vantage the SUT dialled out is
 * exempt whatever it holds.
 */
export const incompleteSourceCall = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  capture: string,
  caseId: string,
  vantages: ReadonlyArray<CutVantage>
): Finding | undefined => {
  const evidence = vantages
    .filter((v) => vantageIsInbound(flows, sut, v))
    .flatMap((v) => {
      const unfinished = unfinalledInvite(flows, sut, v) ?? unterminatedDialog(flows, sut, v)
      return unfinished === undefined ? [] : [unfinished]
    })
  if (evidence.length === 0) return undefined
  return findingOf(
    capture,
    caseId,
    SOURCE_CALL_INCOMPLETE,
    evidence,
    (e) => `caller leg ${e.leg} hop ${e.hop} on '${e.callId}' ${e.detail}`
  )
}

/** The rule: a case whose caller's dialog the capture never finished is refused. */
export const CALL_COMPLETENESS_RULE: CaseRule = {
  id: SOURCE_CALL_INCOMPLETE,
  subject: "source",
  disposition: "refuses",
  why: "the caller's dialog is never finished in the capture, so a replay would attempt a call the source never closed",
  refuses: (input) =>
    incompleteSourceCall(input.flows, input.sut, input.capture, input.caseId, [input.spec.uac, ...input.spec.uas])
}
