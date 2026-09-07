/**
 * `deviations` (`PCAP2TEST_PIVOT_V3.md` §11), mirroring
 * `pivot_schema::deviation`: named, reviewed, grep-able non-compliance the
 * replay must REPRODUCE.
 *
 * Every violation lives here and nowhere else. There are no inline tier-1
 * overrides on a step, so the flow reads as intent and what breaks the rules is
 * greppable in one block. `kind` stays an OPEN token — a document naming a kind
 * a given interpreter does not implement must still parse so lint can say so.
 */
import * as Schema from "effect/Schema"
import { Computed } from "./tokens.js"

/**
 * A `cseq-override`'s value: the number to emit outright, or relative to a CSeq
 * the run observed. UNTAGGED — a number or a `{ from, delta }` object.
 */
export const CseqValue = Schema.Union([Schema.Int, Computed])
export type CseqValue = typeof CseqValue.Type

/** One reproduced non-compliance. */
export const Deviation = Schema.Struct({
  id: Schema.String,
  kind: Schema.String,
  leg: Schema.optionalKey(Schema.String),
  step: Schema.optionalKey(Schema.String),
  header: Schema.optionalKey(Schema.String),
  preserve: Schema.optionalKey(Schema.Array(Schema.String)),
  retransmits: Schema.optionalKey(Schema.Int),
  races: Schema.optionalKey(Schema.String),
  value: Schema.optionalKey(CseqValue)
})
export interface Deviation extends Schema.Schema.Type<typeof Deviation> {}
