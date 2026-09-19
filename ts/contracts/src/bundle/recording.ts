/**
 * One line of the **verbatim per-leg recording** (`PCAP2TEST_PIVOT_V3.md` §14
 * item 12), mirroring `pivot_schema::bundle::recording`: the record kind
 * `recording/<leg>.jsonl` holds, one JSON object per line, in wire order.
 *
 * A recorded datagram is BYTES (ADR-0035), written in exactly one of the
 * extractor's three arms (`Wire`) beside the body's layout, so a reader
 * locates a MIME part by offset and never splits on a boundary. Attribution
 * is best-effort by design — a datagram no step claimed carries no `step`,
 * and is recorded rather than dropped — but the datagram itself never is. The
 * leg is the FILE NAME, never a field.
 */
import * as Schema from "effect/Schema"
import { MsgBody } from "../flows.js"
import * as Wire from "../wire.js"

/** Which way a datagram crossed the leg's vantage. */
export const Dir = Schema.Literals(["out", "in"])
export type Dir = typeof Dir.Type

const lineFields = {
  /** Order within the leg, 1-based. */
  seq: Schema.Int,
  dir: Dir,
  /** Microseconds from the start of the run. */
  at_us: Schema.Int,
  step: Schema.optionalKey(Schema.String),
  /** The body's layout, present iff the datagram carries a body. */
  body: Schema.optionalKey(MsgBody),
  /**
   * The `seq` of the earliest datagram on this leg, in this direction, that this
   * one repeats byte for byte (friction H8). Set by the caller that KNOWS it is
   * a repeat, never derived here.
   */
  repeat_of: Schema.optionalKey(Schema.Int),
  note: Schema.optionalKey(Schema.String)
} as const

export const TextRecordedMessage = Schema.Struct({ ...lineFields, ...Wire.textArm })
export const HeadBodyRecordedMessage = Schema.Struct({ ...lineFields, ...Wire.headBodyArm })
export const OpaqueRecordedMessage = Schema.Struct({ ...lineFields, ...Wire.opaqueArm })

/** One recorded datagram, in exactly one of the three arms. */
export const RecordedMessage = Schema.Union([TextRecordedMessage, HeadBodyRecordedMessage, OpaqueRecordedMessage])
export type RecordedMessage = typeof RecordedMessage.Type
