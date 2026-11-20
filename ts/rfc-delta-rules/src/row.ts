/**
 * What a generic row is: a delta rule over {@link RfcFacts}, with no section
 * and no lane note. A deployment places it by name in one of its own sections
 * and adds its lane notes there.
 */
import type { RuleSpec } from "@sip/delta-rules/rule"
import type { RfcFacts } from "./facts.js"

/** A generic row, over the facts {@link RfcFacts} names. */
export type RfcRow = RuleSpec<RfcFacts>
