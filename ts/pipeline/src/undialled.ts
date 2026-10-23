/**
 * A call the document states with nowhere to go: no attempt, no refusal and no
 * abandon.
 *
 * The system under test took the caller's INVITE (RFC 3261 §16, §17.2.1), so a
 * run of the document drives it to do something with it: forward it, answer it
 * with a final of its own, or see the caller cancel it. A call stating none of
 * the three is a vantage that lost what happened (`pivot-schema lint` refuses it
 * as `call/no-attempts`), and no run of it can be driven to its end.
 */
import type { Call } from "@sip/contracts"
import type { DocumentRule } from "./document-rules.js"
import type { Finding } from "./refusal-rule.js"

/** The reason token a document stating a call that dials nobody is refused by. */
export const DIAL_NOT_CAPTURED = "source-dial-not-captured"

/** The calls stating no attempt, no refusal and no abandon. */
export const undialledCalls = (calls: ReadonlyArray<Call.Call>): ReadonlyArray<Call.Call> =>
  calls.filter((call) => call.attempts.length === 0 && call.refused === undefined && call.abandoned === undefined)

/** The one-line finding, for stderr and the refusal record. */
export const undialledLine = (capture: string, caseId: string, calls: ReadonlyArray<Call.Call>): string =>
  `${capture}: case '${caseId}' EXCLUDED ${DIAL_NOT_CAPTURED} — ` +
  calls.map((call) => `call '${call.id}' (caller leg ${call.caller_leg}) states no attempt, no refusal and no abandon`)
    .join("; ")

/** The rule, read after the transaction rules of `./document-rules.ts`. */
export const DIAL_NOT_CAPTURED_RULE: DocumentRule = {
  id: DIAL_NOT_CAPTURED,
  subject: "source",
  disposition: "unrunnable",
  why: "a call states no attempt, no refusal and no abandon, so no run can say what the system under test " +
    "did with the INVITE it took (RFC 3261 §16, §17.2.1)",
  refuses: (input): Finding | undefined => {
    const charged = undialledCalls(input.calls)
    return charged.length === 0 ? undefined : { line: undialledLine(input.capture, input.caseId, charged) }
  }
}
