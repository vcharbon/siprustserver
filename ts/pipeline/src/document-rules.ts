/**
 * The CASE-DOCUMENT tier: a refusal decided on the assembled flow, and the
 * pipeline's own roster of them.
 *
 * {@link DocumentInput} holds the step list and nothing else — no flows
 * document, no SUT set, no vantages. Step ids, `op`, `auto` and `observed` exist
 * nowhere earlier, and everything earlier has already had its say, so a rule
 * here reads the document it will refuse and no other evidence.
 *
 * The four rules below are the PIPELINE's own correctness contract, not a
 * deployment's reading: a document that ACKs a final it never captured, or
 * answers a request it never captured, is unrunnable against ANY conformant SUT.
 * A deployment adds its own through {@link Policy.CasePolicy.refuseOnDocument},
 * and those run after these.
 *
 * The array is read in ON-DISK order, not in match order: refusals are
 * ALL-THAT-APPLY, so a document holding two of these holes is recorded under
 * both tokens.
 *
 * A hole the DECLARATION lane already names is not one of them ({@link
 * undeclared}).
 */
import type { Flow, MustFail } from "@sip/contracts"
import { checkRefusalRoster } from "./refusal-roster.js"
import type { Finding, RuleIdentity } from "./refusal-rule.js"
import {
  ACK_NOT_CAPTURED,
  ACTOR_ACK_NOT_CAPTURED,
  FINAL_NOT_CAPTURED,
  orphanLine,
  orphanResponses,
  REQUEST_NOT_CAPTURED,
  unackedFinals,
  unackedLine,
  unackedTakenFinals,
  unackedTakenLine,
  unfinalledAcks,
  unfinalledLine
} from "./runnable.js"

/** What a CASE-DOCUMENT-TIER refusal is decided from: the assembled flow. */
export interface DocumentInput {
  readonly capture: string
  readonly caseId: string
  /** The pivot's assembled steps, as the document states them. */
  readonly flow: ReadonlyArray<Flow.FlowNode>
  /**
   * The divergences the document already OWES (`must_fail`), each at the step
   * that predicts it. Empty on a positive case.
   */
  readonly declared: ReadonlyArray<MustFail.MustFail>
}

/** One rule of the case-document tier. */
export interface DocumentRule extends RuleIdentity {
  readonly refuses: (input: DocumentInput) => Finding | undefined
}

/**
 * The charges no DECLARATION anchors.
 *
 * A hole the document already declares is not a refusal: the case is generated
 * as a NEGATIVE one owing exactly that failure at exactly that step, so the
 * divergence is stated in advance instead of replayed undeclared — which keeps
 * the coverage and states the outcome, where a refusal keeps neither. Only the
 * residue no declaration can anchor holds the refusal, the same ruling the
 * deferred capture-tier refusals stand on, taken at the tier that can read the
 * document.
 *
 * Matched on the CHARGED STEP, never on the failure alone: a document declaring
 * one un-ACKed 2xx says nothing about a second one it does not name.
 */
const undeclared = <T extends { readonly final: string }>(
  charged: ReadonlyArray<T>,
  declared: ReadonlyArray<MustFail.MustFail>,
  failure: MustFail.DeclaredFailure
): ReadonlyArray<T> =>
  charged.filter((c) => !declared.some((d) => d.failure === failure && d.step === c.final))

/** A charged list, or `undefined` where the rule found nothing. */
const found = <T>(
  charged: ReadonlyArray<T>,
  line: (charged: ReadonlyArray<T>) => string
): Finding | undefined => (charged.length === 0 ? undefined : { line: line(charged) })

/** Every case-document refusal the pipeline states on its own account. */
export const DOCUMENT_RULES: ReadonlyArray<DocumentRule> = [
  {
    id: FINAL_NOT_CAPTURED,
    subject: "source",
    disposition: "refuses",
    refuses: (input) =>
      found(unfinalledAcks(input.flow), (c) => unfinalledLine(input.capture, input.caseId, c))
  },
  {
    id: ACK_NOT_CAPTURED,
    subject: "source",
    disposition: "refuses",
    refuses: (input) =>
      found(undeclared(unackedFinals(input.flow), input.declared, "unexpected-ack"), (c) =>
        unackedLine(input.capture, input.caseId, c))
  },
  {
    id: ACTOR_ACK_NOT_CAPTURED,
    subject: "source",
    disposition: "refuses",
    refuses: (input) =>
      found(
        undeclared(unackedTakenFinals(input.flow), input.declared, "unexpected-ack"),
        (c) => unackedTakenLine(input.capture, input.caseId, c)
      )
  },
  {
    id: REQUEST_NOT_CAPTURED,
    subject: "source",
    disposition: "refuses",
    refuses: (input) =>
      found(orphanResponses(input.flow), (c) => orphanLine(input.capture, input.caseId, c))
  }
]

checkRefusalRoster(DOCUMENT_RULES, {
  where: "@sip/pipeline document rules",
  deploymentFree: true
})
