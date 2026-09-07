/**
 * The **post-run RFC audit** as `rfc.json`, mirroring
 * `pivot_schema::bundle::rfc`: what the lane's recording fabric found when the
 * full RFC suite ran over the run's wire, or the stated fact that no fabric
 * recorded it.
 *
 * A gating finding fails the cell, and only what the SUT side EMITTED gates: a
 * document actor's own deviation is the capture's, replayed as scripted. An
 * advisory finding is written so a triage session can read it, never counted. `not-audited` is a value of its own, so a
 * lane that recorded nothing can never be read as a clean audit.
 */
import * as Schema from "effect/Schema"

/** One RFC-suite finding over the run's recorded wire. */
export const RfcFinding = Schema.Struct({
  /** The rule id (e.g. `cseq-in-dialog-order`). */
  rule: Schema.String,
  /** The bind (lane) the finding is attributed to. */
  lane: Schema.String,
  detail: Schema.String,
  /** Informational only, never gating. */
  advisory: Schema.Boolean,
  /** Fails the cell: non-advisory, unwaived, and not a document actor's own emission. */
  gating: Schema.Boolean,
  /** The 1-based audit wire-entry index of the offending message, where the rule pinpoints one. */
  offending: Schema.optionalKey(Schema.Int),
  /** The socket the rule holds responsible: emitted the offending message, or owed the missing one. `lane` is where it was reported. */
  charged: Schema.optionalKey(Schema.String),
  /** The document endpoint `charged` is, when it is one: a scripted peer's own deviation. */
  actor: Schema.optionalKey(Schema.String)
})
export interface RfcFinding extends Schema.Schema.Type<typeof RfcFinding> {}

export const RunRfcAudit = Schema.Union([
  /** No recording fabric carried the run, so the suite had no input. */
  Schema.Struct({ status: Schema.Literal("not-audited"), reason: Schema.String }),
  /** The full suite ran over the recorded wire; every finding, advisory included. */
  Schema.Struct({ status: Schema.Literal("audited"), findings: Schema.Array(RfcFinding) })
])
export type RunRfcAudit = typeof RunRfcAudit.Type

/** The findings that fail the cell. */
export const rfcGating = (audit: RunRfcAudit): ReadonlyArray<RfcFinding> =>
  audit.status === "audited" ? audit.findings.filter((f) => f.gating) : []

/** Audited with no gating finding, or not audited at all — an absence the lane states, never a pass it claims. */
export const rfcAuditPassed = (audit: RunRfcAudit): boolean => rfcGating(audit).length === 0
