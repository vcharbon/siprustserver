/**
 * The post-run confrontation records: `confrontation.ndjson` (one flat object
 * per confronted difference, every key always present, jq-friendly) and
 * `classification.json` (the per-run classification summary) — both written
 * beside the run bundle by the driver, never by the interpreter.
 *
 * The record's key set and the `class` vocabulary are the delta-record contract
 * the triage tooling groups on: `signature` is the stable grouping key,
 * independent of capture, values and header casing.
 *
 * TS-owned, unlike its sibling modules: the confrontation runs post-run in
 * TypeScript, so no Rust struct mirrors these records — this module IS the
 * contract, and it lives here so both workspaces decode through one copy.
 */
import * as Schema from "effect/Schema"
import { format, formatLine } from "./canonical.js"
import { STRICT } from "./strict.js"

/** How a confronted difference classifies against a lane's rule lists. */
export const RecordClass = Schema.Literals(["accepted", "known-bug", "unlisted", "unknown"])
export type RecordClass = typeof RecordClass.Type

/** Whether classification decides the run's exit or only lands on the record. */
export const ConfrontMode = Schema.Literals(["record", "enforce"])
export type ConfrontMode = typeof ConfrontMode.Type

/**
 * One confronted difference. Flat and total: every key is present on every
 * line, so `jq 'select(.class=="unknown") | .signature'` never misses a field.
 */
export const ConfrontationRecord = Schema.Struct({
  /** The replay lane whose rule lists decided `class`. */
  lane: Schema.String,
  /** The capture the case came from; empty for an authored document. */
  capture: Schema.String,
  case: Schema.String,
  /** Repetition index of this case within one campaign, 0-based. */
  run: Schema.Int,
  /** The flow step the difference was observed at; empty when unattributed. */
  step: Schema.String,
  kind: Schema.Literals(["header", "shape"]),
  /** Stable grouping key, e.g. `header:contact:response:200:INVITE`. */
  signature: Schema.String,
  /** Header name for a header record, empty for a shape record. */
  name: Schema.String,
  /** The message scope, e.g. `initial-invite`; empty when unpinned. */
  scope: Schema.String,
  captured: Schema.Array(Schema.String),
  replayed: Schema.Array(Schema.String),
  /** Whether the capture shows this header reaching the replayed system. */
  inbound: Schema.Boolean,
  /** Set-folded membership gains; empty for every other fold. */
  added: Schema.Array(Schema.String),
  removed: Schema.Array(Schema.String),
  class: RecordClass,
  /** The rule that named the difference; empty when none matched. */
  rule: Schema.String,
  /** The ticket a `known-bug` waits on; empty otherwise. */
  ticket: Schema.String
})
export interface ConfrontationRecord extends Schema.Schema.Type<typeof ConfrontationRecord> {}

/**
 * The per-run classification summary, as `classification.json` beside the
 * bundle. `passed` is the classification verdict alone — whether every record
 * is `accepted` or `known-bug` — which only an enforce-mode driver folds into
 * the cell outcome.
 */
export const ClassificationSummary = Schema.Struct({
  case: Schema.String,
  lane: Schema.String,
  mode: ConfrontMode,
  records: Schema.Int,
  accepted: Schema.Int,
  known_bug: Schema.Int,
  unlisted: Schema.Int,
  unknown: Schema.Int,
  /** Receptions whose captured reference was found and compared. */
  compared: Schema.Int,
  /**
   * Receptions that compared NOTHING — no capture coordinate, or no flows
   * document supplied. Never zero-filled: an unreferenced reception must not
   * read as verified transparency.
   */
  unreferenced: Schema.Int,
  passed: Schema.Boolean
})
export interface ClassificationSummary extends Schema.Schema.Type<typeof ClassificationSummary> {}

export const decodeConfrontationRecord = Schema.decodeUnknownEffect(ConfrontationRecord, STRICT)
export const decodeConfrontationRecordSync = Schema.decodeUnknownSync(ConfrontationRecord, STRICT)
export const decodeClassificationSummary = Schema.decodeUnknownEffect(ClassificationSummary, STRICT)
export const decodeClassificationSummarySync = Schema.decodeUnknownSync(ClassificationSummary, STRICT)

/** One NDJSON line, sorted keys, no trailing newline — the caller owns `\n`. */
export const emitConfrontationRecord = (value: ConfrontationRecord): string =>
  formatLine(Schema.encodeUnknownSync(ConfrontationRecord, STRICT)(value))

export const emitClassificationSummary = (value: ClassificationSummary): string =>
  format(Schema.encodeUnknownSync(ClassificationSummary, STRICT)(value))

/** Every record of one `confrontation.ndjson` text, blank lines skipped. */
export const parseConfrontationLines = (text: string): ReadonlyArray<ConfrontationRecord> =>
  text
    .split("\n")
    .filter((line) => line.trim().length > 0)
    .map((line) => decodeConfrontationRecordSync(JSON.parse(line) as unknown))

/** The classification verdict: no record is `unlisted` or `unknown`. */
export const recordsPass = (records: ReadonlyArray<ConfrontationRecord>): boolean =>
  records.every((r) => r.class === "accepted" || r.class === "known-bug")
