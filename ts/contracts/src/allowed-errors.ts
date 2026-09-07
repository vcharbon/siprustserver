/**
 * The allowed-errors registry (`allowed-errors.json`): the committed memory of
 * the RFC violations a SOURCE capture is known to carry.
 *
 * A violation that changes nothing about the bytes cannot be a `deviations`
 * kind (`PCAP2TEST_PIVOT_V3.md` §11.1), so the registry is the memory instead: a
 * capture named here has its matching violations stamped into every case cut
 * from the calls it lists, and a regeneration remembers what was already
 * decided.
 *
 * Entries reach it two ways — a human writes one, or a census sweep adds one
 * mechanically for a hit it places on the source side — and both read the same
 * here. It is versioned data a human maintains, so a field the format does not
 * define, or a rule outside {@link RfcRule}'s closed vocabulary, is a LOAD
 * failure: ignoring either is how a typo turns a flagged capture back into a
 * clean one.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { STRICT } from "./strict.js"
import { RfcRule } from "./violation.js"

/** One known violation of one capture: what broke, who emitted it, in which calls. */
export const AllowedViolation = Schema.Struct({
  rule: RfcRule,
  /** The emitting endpoint as the census names it: `ip:port`. */
  originator: Schema.String,
  call_ids: Schema.NonEmptyArray(Schema.String),
  note: Schema.String
})
export interface AllowedViolation extends Schema.Schema.Type<typeof AllowedViolation> {}

/** `allowed-errors.json`: capture file name → the violations it is known to carry. */
export const AllowedErrors = Schema.Struct({
  $comment: Schema.optionalKey(Schema.String),
  captures: Schema.Record(Schema.String, Schema.Array(AllowedViolation))
})
export interface AllowedErrors extends Schema.Schema.Type<typeof AllowedErrors> {}

export const decodeAllowedErrors = Schema.decodeUnknownEffect(AllowedErrors, STRICT)
export const decodeAllowedErrorsSync = Schema.decodeUnknownSync(AllowedErrors, STRICT)

/** Parse a registry from its text. */
export const parseAllowedErrors = (text: string) =>
  Effect.suspend(() => decodeAllowedErrors(JSON.parse(text) as unknown))

/** The entries the registry holds for one capture. Absent reads as none. */
export const violationsOf = (
  registry: AllowedErrors | undefined,
  capture: string
): ReadonlyArray<AllowedViolation> => registry?.captures[capture] ?? []
