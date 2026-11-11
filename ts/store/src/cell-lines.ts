/**
 * The verification lines a cell's bundle states (ADR-0060 appendix §8), for a
 * cell that ran:
 * - `call`, the interpreter's: `verdict.json`'s status, its first failure
 *   where it failed, a failure where it wrote none;
 * - `rfc-audit`, the audit's, where `rfc.json` audited the cell: a failure
 *   where a charge stands, the source faults kept as informative;
 * - `run`, the driver's, where the rollup failed the cell and no other line
 *   fails: what failed it beyond the call and the audit (a gate of its batch).
 *
 * A cell that did not run (skipped, or never driven) holds no line. On a
 * bundle the replay wrote whole, the lines fail the cell where the rollup does.
 */
import type { Bundle, Verification } from "@sip/contracts"

type Line = Verification.Verification

/** What a cell's bundle says of it, as `bundle.ts` read it. */
export interface CellReading {
  readonly passed: boolean
  /** `verdict.json`, null where the cell wrote none. */
  readonly verdict: Bundle.RunVerdict | null
  /** The audit's findings, empty where the cell was not audited; null where `rfc.json` is absent or not audited. */
  readonly audit: ReadonlyArray<Bundle.RfcFinding> | null
  /** The gating findings the SUT answers for. */
  readonly charged: ReadonlyArray<Bundle.RfcFinding>
}

/** The line names and sources this reading writes. */
export const CALL = "call"
export const RFC_AUDIT = "rfc-audit"
export const RUN = "run"

const OK = new Set(["ok", "ok-negative"])

/** One failure as a message: its kind, then what it names. */
const failureLine = (failure: Bundle.Failure): string => {
  const { failure: kind, ...rest } = failure as { readonly failure: string } & Record<string, unknown>
  const named = Object.entries(rest)
    .filter(([, value]) => typeof value === "string" || typeof value === "number")
    .map(([key, value]) => `${key}=${String(value)}`)
  return named.length === 0 ? kind : `${kind}: ${named.join(", ")}`
}

const callOf = (verdict: Bundle.RunVerdict | null): Line => {
  if (verdict === null) return { name: CALL, source: "interpreter", answer: "fail", message: "the cell wrote no verdict" }
  if (OK.has(verdict.status)) return { name: CALL, source: "interpreter", answer: "pass" }
  const first = verdict.failures?.[0]
  return {
    name: CALL,
    source: "interpreter",
    answer: "fail",
    message: first === undefined ? "the call failed" : failureLine(first),
    ...(verdict.failed_step === undefined ? {} : { detail: { step: verdict.failed_step } })
  }
}

const auditOf = (audit: ReadonlyArray<Bundle.RfcFinding>, charged: ReadonlyArray<Bundle.RfcFinding>): ReadonlyArray<Line> => {
  const first = charged[0]
  const own: Line = first === undefined
    ? { name: RFC_AUDIT, source: "rfc audit", answer: "pass" }
    : {
      name: RFC_AUDIT,
      source: "rfc audit",
      answer: "fail",
      message: `${charged.length} charge(s): ${first.rule}: ${first.detail}`,
      detail: { seen: first.detail, side: "sut" }
    }
  const faults = audit.filter((finding) => finding.source_fault === true).map((finding): Line => ({
    name: RFC_AUDIT,
    source: "rfc audit",
    answer: "fail",
    message: `source fault: ${finding.rule}: ${finding.detail}`,
    informative: true
  }))
  return [own, ...faults]
}

/** The lines of a cell that ran, in the order call, audit, run. */
export const linesOf = (reading: CellReading): ReadonlyArray<Line> => {
  const lines = [callOf(reading.verdict), ...(reading.audit === null ? [] : auditOf(reading.audit, reading.charged))]
  const failing = lines.some((line) => line.answer === "fail" && line.informative !== true)
  if (reading.passed || failing) return lines
  return [...lines, {
    name: RUN,
    source: "driver",
    answer: "fail",
    message: "the run failed the cell beyond its call and its audit"
  }]
}
