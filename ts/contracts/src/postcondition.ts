/**
 * `postconditions` (`PCAP2TEST_PIVOT_V3.md` §10), mirroring
 * `pivot_schema::postcondition`: what must hold once the flow has run and the
 * settle phase has completed.
 *
 * The settle phase itself is not optional and is not stated here, and it never
 * waits for a claim. What this block adds is the case's own evidence: its
 * claims (how many CDRs the run must have written, among them), and any
 * deployment observable checked once at settle.
 *
 * **CDR checking is default-on.** A document that genuinely has no CDR oracle
 * says so with a reason token, which makes the gap greppable instead of
 * invisible.
 */
import * as Schema from "effect/Schema"
import { Check } from "./check.js"

/** The CDR count the run must produce, plus any field assertions over them. */
export const CdrCheck = Schema.Struct({
  count: Schema.Int,
  checks: Schema.optionalKey(Schema.Array(Check))
})
export interface CdrCheck extends Schema.Schema.Type<typeof CdrCheck> {}

/** A stated absence of a CDR assertion, with an open reason token. */
export const CdrAbsent = Schema.Struct({
  absent: Schema.String
})
export interface CdrAbsent extends Schema.Schema.Type<typeof CdrAbsent> {}

/**
 * What the run must have billed, or why nothing can be said about it. UNTAGGED:
 * a `count` or an `absent`, never both.
 */
export const CdrExpectation = Schema.Union([CdrCheck, CdrAbsent])
export type CdrExpectation = typeof CdrExpectation.Type

/** The collector `cdr` claims on: the records the system writes for each call it handled. */
export const CALL_RECORDS = "call-records"

/**
 * Assertions evaluated once, after settle, and claims answered after the
 * call: per collector name, what the case expects, opaque to the runner. A
 * stated `cdr` count is read as the `call-records` claim; a document states
 * that claim once.
 */
export const Postconditions = Schema.Struct({
  cdr: Schema.optionalKey(CdrExpectation),
  claims: Schema.optionalKey(Schema.Record(Schema.String, Schema.Unknown)),
  checks: Schema.optionalKey(Schema.Array(Check))
})
export interface Postconditions extends Schema.Schema.Type<typeof Postconditions> {}

/**
 * Every claim `postconditions` states, by collector name: its `claims`, and a
 * `cdr` count as the {@link CALL_RECORDS} claim where `claims` does not name
 * that collector. A stated absence claims nothing.
 */
export const statedClaims = (postconditions: Postconditions | undefined): Readonly<Record<string, unknown>> => {
  const claims = { ...postconditions?.claims }
  const cdr = postconditions?.cdr
  if (cdr !== undefined && "count" in cdr && !(CALL_RECORDS in claims)) claims[CALL_RECORDS] = cdr
  return claims
}
