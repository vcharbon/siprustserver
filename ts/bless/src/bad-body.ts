/**
 * THE BODY GATE: a body row no lane rule accepts fails its cell, whatever the
 * interpreter decided and whatever the confrontation mode.
 *
 * The replay reproduces every body byte for byte, the session description
 * included; the only values a rule may set aside are the ones the system under
 * test owns, each named by a deployment's delta rule (ADR-0062). Every
 * other `body:` row is a `bad-body` error. Where the gate is what refused the
 * cell (it passed, or the bless held on every other record), the cell counts
 * {@link GATE_HIT}, which the triage store reads to lead with `bad-body`
 * (`@sip/contracts` `main-error.ts`).
 */
import type { CellHits } from "@sip/contracts"
import { Confrontation, MainError } from "@sip/contracts"

export const BAD_BODY = MainError.BAD_BODY
export const failingBody = MainError.failingBody

/** The count a cell carries where the body gate is what refused it. */
export const GATE_HIT: CellHits.Hit = { family: "bless", rule: BAD_BODY, bucket: "fired", hits: 1 }

/** The records without the body rows no rule accepts. */
export const withoutFailingBody = (
  records: ReadonlyArray<Confrontation.ConfrontationRecord>
): ReadonlyArray<Confrontation.ConfrontationRecord> => {
  const failing = new Set(failingBody(records))
  return records.filter((record) => !failing.has(record))
}

/**
 * Why a cell with failing body rows fails, the row it leads with named — the
 * earliest by the document's step order (`steps`) a rule names, else the
 * earliest, as the main error reads it; undefined where it has none.
 */
export const badBody = (
  records: ReadonlyArray<Confrontation.ConfrontationRecord>,
  steps: ReadonlyArray<string>
): string | undefined => {
  const failing = Confrontation.inStepOrder(failingBody(records), steps)
  const first = MainError.leadingBody(failing)
  if (first === undefined) return undefined
  const at = first.step === "" ? "" : ` at ${first.step}`
  const rule = first.rule === "" ? "" : `${first.rule}: `
  return `${BAD_BODY}: ${failing.length} body row(s) no rule accepts, first ${rule}${first.signature}${at}`
}
