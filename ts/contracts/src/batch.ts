/**
 * The BATCH PROTOCOL's wire formats: what a replay and a batch interpreter —
 * one long-lived process running every cell of a batch on a real lane — say
 * to each other, and what a real-lane cell leaves for a reader of the SUT's
 * own records.
 *
 * - The batch-spec the process is started on: the lane, its scratch and the
 *   one side-row directory of the batch; a lane's own wiring rides beside it.
 * - The lines: a run-spec path per line in; per line out one of `ready` (the
 *   process serves its lane), `cell` (one run-spec ended, in the `replay
 *   <run-spec>` exit vocabulary) and `end` (the batch's cleanup gate, once its
 *   input closed). A JSON object carrying one of their keys that does not
 *   decode is malformed, never talk; a line that is none of them is the
 *   process's own talk.
 * - The contexts report: what the gate read of the SUT's call contexts.
 * - `sut-join.json`: the Call-IDs a real-lane cell minted and the wall-clock
 *   window it drove the SUT in.
 *
 * TS-declared, the interpreter conforms; extra keys are ignored.
 */
import * as Schema from "effect/Schema"

/** The batch-spec's own fields; a lane's wiring rides beside them. */
export const BatchSpec = Schema.Struct({
  lane: Schema.String,
  /** The batch's scratch. */
  work_dir: Schema.String,
  /** The one directory holding a side row for every case the batch's cells run. */
  ledger_dir: Schema.optional(Schema.String)
})
export interface BatchSpec extends Schema.Schema.Type<typeof BatchSpec> {}

/** A call context the SUT held after the batch and not before it. */
export const HeldContext = Schema.Struct({
  ref_id: Schema.String,
  /** The Call-IDs of the legs that name it. */
  call_ids: Schema.Array(Schema.String),
  /**
   * The bundle of every cell whose minted Call-ID it serves: one is the
   * context's cell, charged with it; none or several leave it the batch's.
   */
  cells: Schema.Array(Schema.String)
})
export interface HeldContext extends Schema.Schema.Type<typeof HeldContext> {}

/** What the batch's gate read of the SUT's call contexts. */
export const ContextsReport = Schema.Struct({
  /** The context store, `host:port`. */
  store: Schema.String,
  /** How many contexts it held before the batch's first cell. */
  before: Schema.optional(Schema.Number),
  /** How many it held once the batch ended. */
  after: Schema.optional(Schema.Number),
  held: Schema.Array(HeldContext),
  /** Why the store could not be read, where it could not. */
  unread: Schema.optional(Schema.String)
})
export interface ContextsReport extends Schema.Schema.Type<typeof ContextsReport> {}

/** One run-spec ended in the process. */
export const CellLine = Schema.Struct({
  spec: Schema.String,
  exit: Schema.Number,
  /** The one line stating the cell's verdict, cleanup and audit. */
  summary: Schema.String,
  /** What the cell wrote on stderr. */
  stderr: Schema.optional(Schema.String)
})
export interface CellLine extends Schema.Schema.Type<typeof CellLine> {}

/** The batch's gate, once its input closed. */
export const EndLine = Schema.Struct({
  /** What the SUT still held; `null` where it let go of everything. */
  dirty: Schema.NullOr(Schema.String),
  contexts: Schema.optional(Schema.NullOr(ContextsReport))
})
export interface EndLine extends Schema.Schema.Type<typeof EndLine> {}

/** One line the process writes: exactly one of its keys is set. */
export const Line = Schema.Struct({
  ready: Schema.optional(Schema.Unknown),
  cell: Schema.optional(CellLine),
  end: Schema.optional(EndLine)
})
export interface Line extends Schema.Schema.Type<typeof Line> {}

/** The keys that make a JSON object a protocol line. */
const KEYS = ["ready", "cell", "end"] as const

/**
 * How one line the process wrote reads: a protocol `line`; `malformed`, a JSON
 * object carrying a protocol key whose value does not decode, with the key and
 * the run-spec path it names where it names one; or the process's own `talk`.
 */
export type Read =
  | { readonly _tag: "line"; readonly line: Line }
  | {
    readonly _tag: "malformed"
    readonly key: (typeof KEYS)[number]
    readonly spec: string | undefined
    readonly why: string
  }
  | { readonly _tag: "talk" }

/** `text` read as one line of the process's stdout. */
export const readLine = (text: string): Read => {
  let parsed: unknown
  try {
    parsed = JSON.parse(text)
  } catch {
    return { _tag: "talk" }
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return { _tag: "talk" }
  const key = KEYS.find((name) => name in parsed)
  if (key === undefined) return { _tag: "talk" }
  const decoded = Schema.decodeUnknownResult(Line)(parsed)
  if (decoded._tag === "Success") return { _tag: "line", line: decoded.success }
  const named = (parsed as { readonly cell?: { readonly spec?: unknown } }).cell?.spec
  return { _tag: "malformed", key, spec: typeof named === "string" ? named : undefined, why: String(decoded.failure) }
}

/** The bundle file a real-lane cell states its join to the SUT's records in. */
export const SUT_JOIN_FILE = "sut-join.json"

/** What joins a real-lane cell to the SUT's own records. */
export const SutJoin = Schema.Struct({
  /** The Call-ID of every call the cell placed, in document order. */
  call_ids: Schema.Array(Schema.String),
  started_at_ms: Schema.Number,
  ended_at_ms: Schema.Number
})
export interface SutJoin extends Schema.Schema.Type<typeof SutJoin> {}

export const decodeSutJoinOption = Schema.decodeUnknownOption(SutJoin)
