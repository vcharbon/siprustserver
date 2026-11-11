/**
 * The **verification line** (ADR-0060 appendix §8): one named question asked
 * of a cell, a batch or a job, who answered it and how. A cell that ran holds
 * N lines, and its final status is read off them ({@link finalStatusOf}),
 * never kept apart from them.
 *
 * A line's `detail` is open: the site renders `step`, `expected`, `seen` and
 * `side` where present, and a pack may add keys of its own beside them.
 */
import * as Schema from "effect/Schema"

/** What a line answered. `pending`: a collector that has not answered yet. */
export const Answer = Schema.Literals(["pass", "fail", "not-checkable", "pending"])
export type Answer = typeof Answer.Type

/** Where a failing line points: the flow step, what was expected and seen, and whose fault it is typed to. */
export const Detail = Schema.StructWithRest(
  Schema.Struct({
    step: Schema.optionalKey(Schema.String),
    expected: Schema.optionalKey(Schema.String),
    seen: Schema.optionalKey(Schema.String),
    side: Schema.optionalKey(Schema.String)
  }),
  [Schema.Record(Schema.String, Schema.Unknown)]
)
export type Detail = typeof Detail.Type

export const Verification = Schema.Struct({
  name: Schema.String,
  /** Who answered it: the interpreter, the RFC audit, a backend double, a collector, a judge. */
  source: Schema.String,
  answer: Answer,
  message: Schema.optionalKey(Schema.String),
  detail: Schema.optionalKey(Detail),
  /** The line never decides the cell. */
  informative: Schema.optionalKey(Schema.Boolean),
  /** A failure the lane lists as a known bug: it never decides the cell. */
  waived: Schema.optionalKey(Schema.Boolean)
})
export interface Verification extends Schema.Schema.Type<typeof Verification> {}

/** The final status of a cell that ran, read off its lines. */
export const FinalStatus = Schema.Literals(["pass", "fail", "fail, still verifying", "ok so far", "unverified"])
export type FinalStatus = typeof FinalStatus.Type

/** Whether the line takes part in the cell's final status: neither informative nor waived. */
export const decides = (line: Pick<Verification, "informative" | "waived">): boolean =>
  line.informative !== true && line.waived !== true

/**
 * The final status of a cell that ran, over the lines that decide it: a
 * `fail` fails it (still verifying while a line is `pending`), a `pending`
 * leaves it `ok so far`, a `not-checkable` leaves it `unverified`, and only
 * lines that all pass make it `pass`.
 */
export const finalStatusOf = (lines: ReadonlyArray<Pick<Verification, "answer" | "informative" | "waived">>): FinalStatus => {
  const deciding = lines.filter(decides)
  const has = (answer: Answer): boolean => deciding.some((line) => line.answer === answer)
  if (has("fail")) return has("pending") ? "fail, still verifying" : "fail"
  if (has("pending")) return "ok so far"
  if (has("not-checkable")) return "unverified"
  return "pass"
}
