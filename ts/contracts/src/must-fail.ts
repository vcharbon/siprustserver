/**
 * `must_fail` (`PCAP2TEST_PIVOT_V3.md` §11.2), mirroring
 * `pivot_schema::must_fail`: the failure this document's run MUST produce.
 *
 * A negative case replays a source whose non-compliance this platform does not
 * share. The run cannot pass by behaving well, and it must not be excused
 * either: it fails, and the document says in advance exactly HOW.
 *
 * `failure` is CLOSED for the same reason {@link RfcRule} is; `derived_from`
 * names the rule whose violation in the SOURCE predicts it, so a declaration is
 * traceable to the evidence rather than hand-guessed.
 */
import * as Schema from "effect/Schema"
import { RfcRule } from "./violation.js"

/** The failures this format can declare, each predictable from a detector's hit. */
export const DeclaredFailure = Schema.Literals([
  "unexpected-ack",
  "unexpected-prack",
  "unexpected-cancel"
])
export type DeclaredFailure = typeof DeclaredFailure.Type

/** One failure this run must produce, at or immediately after one step. */
export const MustFail = Schema.Struct({
  failure: DeclaredFailure,
  step: Schema.String,
  derived_from: RfcRule
})
export interface MustFail extends Schema.Schema.Type<typeof MustFail> {}
