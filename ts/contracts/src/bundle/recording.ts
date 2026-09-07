/**
 * One line of the **verbatim per-leg recording** (`PCAP2TEST_PIVOT_V3.md` §14
 * item 10), mirroring `pivot_schema::bundle::recording`: the record kind
 * `recording/<leg>.jsonl` holds, one JSON object per line, in wire order.
 *
 * Attribution is best-effort by design — a datagram no step claimed carries no
 * `step`, and is recorded rather than dropped — but the datagram itself never
 * is. The leg is the FILE NAME, never a field.
 */
import * as Schema from "effect/Schema"

/** Which way a datagram crossed the leg's vantage. */
export const Dir = Schema.Literals(["out", "in"])
export type Dir = typeof Dir.Type

/** One recorded datagram. */
export const RecordedMessage = Schema.Struct({
  /** Order within the leg, 1-based. */
  seq: Schema.Int,
  dir: Dir,
  /** Microseconds from the start of the run. */
  at_us: Schema.Int,
  step: Schema.optionalKey(Schema.String),
  raw: Schema.String,
  /**
   * The `seq` of the earliest datagram on this leg, in this direction, that this
   * one repeats byte for byte (friction H8). Set by the caller that KNOWS it is
   * a repeat, never derived here.
   */
  repeat_of: Schema.optionalKey(Schema.Int),
  note: Schema.optionalKey(Schema.String)
})
export interface RecordedMessage extends Schema.Schema.Type<typeof RecordedMessage> {}
