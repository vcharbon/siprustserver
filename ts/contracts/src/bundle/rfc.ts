/**
 * The **post-run RFC audit** as `rfc.json`, mirroring
 * `pivot_schema::bundle::rfc`: what the lane's recording fabric found when the
 * full RFC suite ran over the run's wire, or the stated fact that no fabric
 * recorded it.
 *
 * A gating finding fails the cell, whichever party committed it. A document
 * actor's violation is named as that actor's and is cancelled only by the
 * document's statement of the same violation on the same transaction
 * (`cancelled_by`). An advisory finding is written so a triage session can read
 * it, never counted. `not-audited` is a value of its own, so a lane that
 * recorded nothing can never be read as a clean audit.
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
  /** Fails the cell: non-advisory, unwaived, and not cancelled. */
  gating: Schema.Boolean,
  /** The 1-based audit wire-entry index of the offending message, where the rule pinpoints one. */
  offending: Schema.optionalKey(Schema.Int),
  /** The socket the rule holds responsible: emitted the offending message, or owed the missing one. `lane` is where it was reported. */
  charged: Schema.optionalKey(Schema.String),
  /** The document actor `charged` names (through the leg the offending call rides), when it is a document endpoint: the violation is the scripted peer's. */
  actor: Schema.optionalKey(Schema.String),
  /** What cancels an actor's finding: the document's statement of the same violation on the same transaction. */
  cancelled_by: Schema.optionalKey(Schema.String),
  /** The scripted party whose violation the system under test relayed onward, where the document states both: the finding is the SUT's, caused by that party. */
  caused_by: Schema.optionalKey(Schema.String)
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
