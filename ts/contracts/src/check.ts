/**
 * The one check vocabulary (`PCAP2TEST_PIVOT_V3.md` §9) and its check classes
 * (§9.1), mirroring `pivot_schema::check` and `pivot_schema::scoping`.
 *
 * A check is `{ field, op, value }`. It appears inline on an `expect`, where it
 * asserts over the matched message, and in `postconditions`, where it asserts
 * over what the run left behind. The field grammar and the observable names are
 * DEPLOYMENT vocabulary: this contract fixes the shape and the four operators.
 */
import * as Schema from "effect/Schema"

/**
 * What vocabulary an assertion reads. Closed: a class is a promise that one
 * named downgrade rule applies to it, and a rule nothing implements is not a
 * promise.
 */
export const CheckClass = Schema.Literals(["origin-platform-header", "cdr-vocabulary"])
export type CheckClass = typeof CheckClass.Type

/**
 * A defect one lane's system under test is KNOWN to produce, mirroring
 * `pivot_schema::known_bug`. Closed for the reason the classes above are: a
 * token is a promise that one named gate stands down for it. It is a LANE fact
 * — never stated by a document — and it is not an acceptance: what the waived
 * check found is recorded in the verdict's `waived` list.
 */
export const KnownBug = Schema.Literals(["provisional-rewrite-not-applied"])
export type KnownBug = typeof KnownBug.Type

/** The assertion operator. `exists` and `absent` take no `value`. */
export const CheckOp = Schema.Literals(["eq", "regex", "exists", "absent"])
export type CheckOp = typeof CheckOp.Type

/** Whether the operator compares against a stated value. */
export const checkOpTakesValue = (op: CheckOp): boolean => op === "eq" || op === "regex"

/** One field assertion. */
export const Check = Schema.Struct({
  field: Schema.String,
  op: CheckOp,
  value: Schema.optionalKey(Schema.String),
  class: Schema.optionalKey(CheckClass)
})
export interface Check extends Schema.Schema.Type<typeof Check> {}

/** Whether `value` is stated exactly where the operator takes one. */
export const checkValueIsDeclarable = (check: Check): boolean =>
  checkOpTakesValue(check.op) === (check.value !== undefined)
