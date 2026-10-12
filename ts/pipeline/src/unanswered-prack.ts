/**
 * The case-document-tier refusal of a PRACK no captured reliable provisional
 * asks for.
 *
 * A PRACK acknowledges one reliable provisional (RFC 3262 §4, §7.2): its RAck
 * is that provisional's RSeq. A scripted party told to send one on a leg the
 * document shows receiving no reliable provisional has nothing to RAck, and
 * its send fails before the wire. The capture lost the provisional the
 * captured party was answering, so the document cannot state the exchange.
 */
import { Flow } from "@sip/contracts"
import type { DocumentRule } from "./document-rules.js"
import type { Finding } from "./refusal-rule.js"

/** The reason token a document stating such a PRACK is refused by. */
export const PRACK_UNANSWERABLE = "source-prack-unanswerable"

/** One PRACK a scripted party is told to send with no reliable provisional before it. */
export interface UnansweredPrack {
  readonly step: string
  readonly leg: string
  readonly observed?: Flow.Observed
}

/** Whether the step arrives as a reliable provisional: a 101–199 stating an RSeq. */
const reliableArrival = (step: Flow.Step): boolean => {
  const status = step.msg.status ?? 0
  if (step.op !== "expect" || status <= 100 || status >= 200) return false
  const named = (n: string) => n.toLowerCase() === "rseq"
  return (step.msg.headers ?? []).some((h) => named(h.name)) || (step.msg["headers-present"] ?? []).some(named)
}

/** Every PRACK send whose leg received no reliable provisional before it. */
export const unansweredPracks = (flow: ReadonlyArray<Flow.FlowNode>): ReadonlyArray<UnansweredPrack> => {
  const steps = flow.flatMap((node) => Flow.flowNodeSteps(node))
  const rang = new Set<string>()
  const out: Array<UnansweredPrack> = []
  for (const step of steps) {
    if (reliableArrival(step)) rang.add(step.leg)
    if (step.op === "send" && (step.msg.method ?? "").toUpperCase() === "PRACK" && !rang.has(step.leg)) {
      out.push({ step: step.id, leg: step.leg, ...(step.observed === undefined ? {} : { observed: step.observed }) })
    }
  }
  return out
}

/** The one-line finding, for stderr and for `excluded.json`. */
export const unansweredLine = (
  capture: string,
  caseId: string,
  charged: ReadonlyArray<UnansweredPrack>
): string =>
  `${capture}: case '${caseId}' EXCLUDED ${PRACK_UNANSWERABLE} — ` +
  charged
    .map((c) => {
      const where = c.observed === undefined ? "" : ` (capture leg ${c.observed.leg} msg ${c.observed.msg})`
      return `leg ${c.leg} step ${c.step}${where} sends a PRACK and the leg received no reliable provisional before it`
    })
    .join("; ")

/** The rule, read after the transaction rules of `./document-rules.ts`. */
export const PRACK_UNANSWERABLE_RULE: DocumentRule = {
  id: PRACK_UNANSWERABLE,
  subject: "source",
  disposition: "refuses",
  why: "a PRACK is scripted on a leg that received no reliable provisional to acknowledge (RFC 3262 §4, §7.2)",
  refuses: (input): Finding | undefined => {
    const charged = unansweredPracks(input.flow)
    return charged.length === 0 ? undefined : { line: unansweredLine(input.capture, input.caseId, charged) }
  }
}
