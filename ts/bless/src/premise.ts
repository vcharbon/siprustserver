/**
 * THE BLESS PREMISE, pure and independent of what the interpreter decided, and
 * the audit that puts the same premise to a cell nobody asked it about.
 *
 * `blessing`: a run the interpreter failed ONLY on datagrams the flow did not
 * claim (`unmatched-datagram`, `unexpected-datagram`, `datagram-after-flow` —
 * one shape under three failure kinds), on a 100 an INVITE final stood in for
 * (`trying-omitted`), on expects that timed out, or on a
 * `must_fail` declaration the run never reached ({@link neverReached}), every
 * refusal answered by a shape record at its own step, every absence by the
 * confrontation's own `missing-message` shape at that step and every omission
 * by its own `trying-omitted` shape there, all of them
 * `accepted` or `known-bug` under a named rule, is inside the lane's stated
 * tolerance. Coverage is a mapping, never a count, and every other record
 * must be `accepted` or `known-bug`.
 *
 * `auditFindings`: a passing cell read against the same premise; what it
 * answers is whether a RULE is written narrowly enough, and the cell is never
 * moved by it (`./audit.ts`).
 *
 * Both are written apart from the reclassifier so the sweep's audit and the
 * per-rule count evaluate ONE copy of each predicate.
 */
import type { Bundle, Confrontation } from "@sip/contracts"
import { Confrontation as ConfrontationRecords } from "@sip/contracts"
import type { Reclassifier } from "@sip/pipeline"
import * as BlessAudit from "./audit.js"
import * as RfcInherit from "./rfc-inherit.js"

/** The shape kind that states a message the capture shows and the run never sent. */
const MISSING_SHAPE = "missing-message"

/** The shape kind that states a 100 an INVITE's final stood in for (RFC 3261 §17.2.1). */
const OMITTED_SHAPE = "trying-omitted"

/**
 * A `must_fail` declaration the run could not have produced: its anchor is a
 * step the run ABANDONED before reaching, so the declaration was never testable
 * and the divergence that stopped the run is what decides the cell.
 *
 * The ANCHOR is the whole of it. A declaration whose anchor ran and drew nothing
 * states this platform's wire behaviour, and no reading of the other failures
 * forgives that; only the verdict's own `abandoned.pending` — the steps the run
 * never reached — tells the two apart.
 */
const neverReached = (
  failure: Bundle.Failure,
  abandoned: Bundle.Abandoned | undefined
): boolean =>
  failure.failure === "declared-failure-not-produced" &&
  (abandoned?.pending ?? []).includes(failure.step)

/**
 * The named rules covering every refused datagram and every absence, beside any
 * declaration the run never reached, or `undefined` where the run is not inside
 * the lane's stated tolerance.
 *
 * An untestable declaration is forgiven BESIDE a divergence a rule names, never
 * on its own: a bless with nothing to name answers nothing. An absence is
 * answered by the missing message ITSELF at its own step, and an omitted 100 by
 * the omission record at its own step, never by whatever else that step recorded.
 */
export const blessing = (
  failures: ReadonlyArray<Bundle.Failure>,
  records: ReadonlyArray<Confrontation.ConfrontationRecord>,
  abandoned?: Bundle.Abandoned
): ReadonlyArray<string> | undefined => {
  const refusals = failures.filter(
    (f) =>
      f.failure === "unmatched-datagram" ||
      f.failure === "unexpected-datagram" ||
      f.failure === "datagram-after-flow"
  )
  const absences = failures.filter((f) => f.failure === "expect-timed-out")
  const omissions = failures.filter((f) => f.failure === "trying-omitted")
  const untestable = failures.filter((f) => neverReached(f, abandoned))
  if (untestable.length === failures.length) return undefined
  if (refusals.length + absences.length + omissions.length + untestable.length !== failures.length) {
    return undefined
  }
  const shapes = records.filter((r) => r.kind === "shape")
  const stepOf = (f: (typeof refusals)[number]): string =>
    f.failure === "unmatched-datagram" ? f.step : ""
  if (!refusals.every((f) => shapes.some((r) => r.step === stepOf(f)))) return undefined
  const missing = shapes.filter((r) => BlessAudit.shapeKindOf(r.signature) === MISSING_SHAPE)
  if (!absences.every((f) => missing.some((r) => r.step === f.step))) return undefined
  const omitted = shapes.filter((r) => BlessAudit.shapeKindOf(r.signature) === OMITTED_SHAPE)
  if (!omissions.every((f) => omitted.some((r) => r.step === f.step))) return undefined
  if (!ConfrontationRecords.recordsPass(records)) return undefined
  return [...new Set(shapes.map((r) => r.rule))].filter((r) => r !== "").sort()
}

/**
 * A passing cell put to the bless: findings against the RULE, none against the cell.
 *
 * - The premise holding at all.
 * - A shape only a VERDICT FAILURE produces, on a cell with no failures: the
 *   shape derivation and the verdict disagree. The held-final cadence is the
 *   one legitimate kind, derived from the recording, never from a failure.
 * - An RFC charge read back to a peer, or one left standing: a gating charge
 *   fails the cell, so a passing cell carries none, and either means the
 *   ruling and the verdict disagree.
 */
export const auditFindings = (
  outcome: Reclassifier.RunOutcome,
  records: ReadonlyArray<Confrontation.ConfrontationRecord>,
  rfc: RfcInherit.Ruling = RfcInherit.NOTHING
): ReadonlyArray<BlessAudit.AuditRecord> => {
  const findings: Array<BlessAudit.AuditRecord> = []
  const rules = blessing(outcome.verdict.failures ?? [], records, outcome.verdict.abandoned)
  if (rules !== undefined) {
    findings.push({
      finding: "bless-fires-on-a-green-cell",
      lane: outcome.lane,
      rules,
      signatures: [],
      caseId: outcome.caseId,
      detail:
        "the bless premise holds on a run the interpreter passed, so it would " +
        "have restated a cell nobody needed restating"
    })
  }
  const stray = records.filter(
    (r) =>
      r.kind === "shape" && BlessAudit.shapeKindOf(r.signature) !== BlessAudit.HELD_FINAL_SHAPE
  )
  if (stray.length > 0) {
    findings.push({
      finding: "failure-shape-on-a-green-cell",
      lane: outcome.lane,
      rules: [...new Set(stray.map((r) => r.rule))].filter((r) => r !== "").sort(),
      signatures: [...new Set(stray.map((r) => r.signature))].sort(),
      caseId: outcome.caseId,
      detail: `${stray.length} failure-derived shape record(s) on a cell the interpreter passed`
    })
  }
  if (rfc.inherited.length > 0) {
    findings.push({
      finding: "rfc-inheritance-on-a-green-cell",
      lane: outcome.lane,
      rules: RfcInherit.rulesOf(rfc.inherited),
      signatures: [],
      caseId: outcome.caseId,
      detail:
        `${rfc.inherited.length} RFC charge(s) read back to a peer on a run the ` +
        "interpreter passed, so the ruling would have answered a charge nobody made"
    })
  }
  if (rfc.charged.length > 0) {
    findings.push({
      finding: "rfc-charge-on-a-green-cell",
      lane: outcome.lane,
      rules: RfcInherit.rulesOf(rfc.charged),
      signatures: [],
      caseId: outcome.caseId,
      detail: `${rfc.charged.length} gating RFC finding(s) stand on a cell that passed`
    })
  }
  return findings
}
