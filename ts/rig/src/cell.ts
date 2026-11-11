/**
 * The cell description `cell.v1` (ADR-0060 appendix §6): what every collector
 * and judge reads of one cell of its scope. Its identity, its clock, the keys
 * records join it on, its legs, its timeline on the lane's clock and on the
 * wall clock, the claims its case states, the records joined to it so far,
 * the verification lines answered so far, and its directory.
 */
import { Verification as Verifications } from "@sip/contracts"
import * as Schema from "effect/Schema"

/** The schema version a cell description states. */
export const SCHEMA = "cell.v1"

/** The key every cell carries: every Call-ID the test side sent or received. */
export const CALL_ID = "call_id"

/** The keys every cell carries, beside those its case declares and collectors add. */
export const BASE_KEYS = [CALL_ID, "caller", "callee", "request_uri_user"] as const

/** One instant of a cell, on the lane's clock (milliseconds from the cell's zero) and on the wall clock (Unix milliseconds). */
export const Instant = Schema.Struct({
  lane_ms: Schema.Number,
  wall_ms: Schema.Number
})
export interface Instant extends Schema.Schema.Type<typeof Instant> {}

/** One leg of the cell, as its recording states it; its instants are microseconds on the lane's clock. */
export const Leg = Schema.Struct({
  /** The leg's name in the bundle (`recording/<id>.jsonl`). */
  id: Schema.String,
  /** The kind of the test-side actor on it (`uac`, `uas`). */
  role: Schema.String,
  /** The test-side actor on it. */
  actor: Schema.String,
  call_id: Schema.optionalKey(Schema.String),
  /** The From URI of its first INVITE. */
  from: Schema.optionalKey(Schema.String),
  /** The To URI of its first INVITE. */
  to: Schema.optionalKey(Schema.String),
  request_uri: Schema.optionalKey(Schema.String),
  start_us: Schema.optionalKey(Schema.Number),
  /** The first 2xx to an INVITE on the leg. */
  answer_us: Schema.optionalKey(Schema.Number),
  end_us: Schema.optionalKey(Schema.Number)
})
export interface Leg extends Schema.Schema.Type<typeof Leg> {}

/** A verification line answered so far (ADR-0060 appendix §8, `@sip/contracts` `Verification`). */
export const Verification = Verifications.Verification
export type Verification = Verifications.Verification

export const Cell = Schema.Struct({
  schema: Schema.Literal(SCHEMA),
  id: Schema.String,
  case: Schema.String,
  sub_case: Schema.optionalKey(Schema.String),
  lane: Schema.String,
  batch: Schema.String,
  attempt: Schema.Int,
  clock: Schema.Literals(["real", "simulated"]),
  /** Key name to its values; {@link BASE_KEYS} always stated, empty where the cell has none. */
  keys: Schema.Record(Schema.String, Schema.Array(Schema.String)),
  legs: Schema.Array(Leg),
  timeline: Schema.Struct({ start: Instant, end: Instant }),
  /** Per collector name, what the case expects of it. */
  claims: Schema.Record(Schema.String, Schema.Unknown),
  /** Per collector name, the file of the records joined to the cell so far. */
  records: Schema.Record(Schema.String, Schema.String),
  verifications: Schema.Array(Verification),
  /** The cell's directory: its bundle, its exports, its attachments. */
  dir: Schema.String,
  tags: Schema.Array(Schema.String),
  needs: Schema.Array(Schema.String)
})
export interface Cell extends Schema.Schema.Type<typeof Cell> {}
