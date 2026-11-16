/**
 * The case facts the RFC rows read, and how each is read off a cell's document
 * and run. A deployment's own case facts extend {@link RfcFacts}.
 */
import { type DocumentReading, isRequestOf } from "@sip/delta-rules/document-reading"
import type { Classifier } from "@sip/pipeline"

/** What the RFC rows read beyond the probe. */
export interface RfcFacts {
  /**
   * By PRACK step id, the RSeq the leg's actor sent on the reliable provisional
   * that PRACK acknowledges — see {@link acknowledgedRSeqAtStep}.
   */
  readonly acknowledgedRSeqAtStep: ReadonlyMap<string, number>
}

/**
 * `rack-cseq-stack-owned`: by the system's PRACK step, the RSeq the leg's actor
 * sent on the provisional it acknowledges, paired by position: the PRACK
 * answers the leg's k-th reliable provisional (RFC 3262 §7.2), and the actor's
 * k-th RSeq in run order is that provisional's.
 */
export const acknowledgedRSeqAtStep = (
  { facts, stepOf }: DocumentReading,
  run: Classifier.RunShape
): ReadonlyMap<string, number> => {
  const out = new Map<string, number>()
  for (const leg of facts.legs.values()) {
    let k = 0
    for (const e of leg.entries) {
      const step = stepOf(e.step.id)
      if (!e.bySystem && step?.reliable === true && step.cseqMethod === "INVITE") k += 1
      else if (e.bySystem && isRequestOf(e, "PRACK")) {
        const rseq = run.sentRSeqs.get(leg.leg)?.[k - 1]
        if (rseq !== undefined) out.set(e.step.id, rseq)
      }
    }
  }
  return out
}
