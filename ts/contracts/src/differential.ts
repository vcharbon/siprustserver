/**
 * The **differential record kind** — one call driven identically at two (or
 * more) stacks, folded into per-lane outcome + RFC audit + the verdict the
 * process exit rides. Mirrors `e2e_core::differential`; Rust writes the file
 * (`runner differential --json <path>`), this side reads it.
 */
import * as Schema from "effect/Schema"
import { STRICT } from "./strict.js"

export const LaneOutcome = Schema.Struct({
  answered: Schema.Boolean,
  rerouted: Schema.optionalKey(Schema.Boolean),
  primaryRuri: Schema.optionalKey(Schema.String),
  alternateRuri: Schema.optionalKey(Schema.String),
  bLegRuri: Schema.optionalKey(Schema.String),
  summary: Schema.String
})
export interface LaneOutcome extends Schema.Schema.Type<typeof LaneOutcome> {}

export const AuditFinding = Schema.Struct({
  rule: Schema.String,
  lane: Schema.String,
  detail: Schema.String,
  advisory: Schema.Boolean
})
export interface AuditFinding extends Schema.Schema.Type<typeof AuditFinding> {}

/** An empty or absent trace is not clean — nothing was audited. */
export const LaneAudit = Schema.Struct({
  recorded: Schema.Boolean,
  entries: Schema.Int,
  findings: Schema.Array(AuditFinding),
  clean: Schema.Boolean
})
export interface LaneAudit extends Schema.Schema.Type<typeof LaneAudit> {}

export const LaneProbe = Schema.Struct({
  label: Schema.String,
  outcome: LaneOutcome,
  audit: LaneAudit
})
export interface LaneProbe extends Schema.Schema.Type<typeof LaneProbe> {}

export const DifferentialVerdict = Schema.Struct({
  agree: Schema.Boolean,
  rfcClean: Schema.Boolean,
  passed: Schema.Boolean,
  disagreement: Schema.optionalKey(Schema.String)
})
export interface DifferentialVerdict extends Schema.Schema.Type<typeof DifferentialVerdict> {}

export const DifferentialResult = Schema.Struct({
  dialed: Schema.String,
  /** The probe shape — an open token (e.g. `basic`, `reroute`). */
  mode: Schema.String,
  lanes: Schema.Array(LaneProbe),
  verdict: DifferentialVerdict
})
export interface DifferentialResult extends Schema.Schema.Type<typeof DifferentialResult> {}

export const decodeDifferentialResult = Schema.decodeUnknownEffect(DifferentialResult, STRICT)
export const decodeDifferentialResultSync = Schema.decodeUnknownSync(DifferentialResult, STRICT)
