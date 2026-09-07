/**
 * `rfc_violations` (`PCAP2TEST_PIVOT_V3.md` §11.1), mirroring
 * `pivot_schema::violation`: a rule a message the flow ALREADY carries breaks,
 * stated as a fact about the run.
 *
 * Distinct from `deviations` (§11), which changes what an emission looks like. A
 * violation here is behavioural: the message is byte-compliant and its TIMING or
 * its context is what breaks the rule.
 *
 * `rule` is CLOSED, unlike a deviation `kind`: a rule nothing detects is a rule
 * nothing can be held to, so the vocabulary grows one detector at a time. It is
 * `rfc_rules::RuleId::WIRE` position for position.
 */
import * as Schema from "effect/Schema"

/** The token naming the system under test as an emitter. */
export const SUT_EMITTER = "sut"

/** The rules this format can state. Each member names a rule a detector decides off the wire. */
export const RFC_RULES = [
  "no-200-after-cancel",
  "unacked-reliable-provisional",
  "no-ack-to-dialog-creating-2xx",
  "no-cancel-after-final",
  "second-answer-repeats-the-first"
] as const

export const RfcRule = Schema.Literals(RFC_RULES)
export type RfcRule = typeof RfcRule.Type

/** One RFC rule a message of this flow breaks. */
export const RfcViolation = Schema.Struct({
  rule: RfcRule,
  step: Schema.String,
  emitter: Schema.String
})
export interface RfcViolation extends Schema.Schema.Type<typeof RfcViolation> {}

/** Whether the system under test is the emitter — the one case that gates. */
export const violationSutEmitted = (violation: RfcViolation): boolean => violation.emitter === SUT_EMITTER
