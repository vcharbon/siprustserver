/**
 * The capture coordinates a REFUSED case is drawn on.
 *
 * A generated case cites the capture through its steps (`../flows/selection.ts`);
 * a refused one has no steps, and its exclusion record is what names the call:
 * the case's legs, and the datagrams the refusal charged. Read as a selection,
 * the same ladder draws the refused call the way it draws a cut one — its legs
 * alone, the charged messages marked — instead of the capture's first window.
 */
import { messageCoordinate, type CaptureSelection } from "../flows/selection.ts"
import type { ExclusionRecord } from "./parse.ts"

/**
 * The selection the records make: every leg they name, and every evidence
 * datagram with a leg and a message. No step cites anything, so nothing is
 * indexed by step. Empty where the records name no leg at all.
 */
export const exclusionSelection = (records: readonly ExclusionRecord[]): CaptureSelection => {
  const legs = new Set<number>()
  const messages = new Set<string>()
  for (const record of records) {
    for (const leg of record.legs) legs.add(leg)
    for (const entry of record.evidence) {
      if (entry.leg === undefined) continue
      legs.add(entry.leg)
      if (entry.msg !== undefined) messages.add(messageCoordinate(entry.leg, entry.msg))
    }
  }
  return { legs, messages, stepByMessage: new Map(), opByMessage: new Map(), empty: legs.size === 0 }
}

/** The SUT addresses the records state, deduplicated, in the order they appear. */
export const exclusionSut = (records: readonly ExclusionRecord[]): readonly string[] =>
  [...new Set(records.flatMap((record) => record.sut))]
