/**
 * The verification lines a cell's bundle states (ADR-0060 appendix §8), for a
 * cell that ran:
 * - the lines the lane's in-process verifications answered, where the cell
 *   wrote `verifications.json` (ADR-0060 item 13): its call and each gate;
 *   else `call`, the interpreter's: `verdict.json`'s status, its first failure
 *   where it failed, a failure where it wrote none, and each check a lane's
 *   known bug stood down as a waived failure of it;
 * - `rfc-audit`, the audit's, where `rfc.json` audited the cell: a failure
 *   where a charge stands, the source faults kept as informative; not
 *   checkable, for its reason, where `rfc.json` states the cell not audited;
 * - `run`, the driver's, where the rollup failed the cell and no other line
 *   fails or is pending: what failed it beyond the lines above.
 *
 * Where the deployment's reclassifier restated the failed run as passed,
 * every failure deciding the cell stands waived, the reclassifier its source
 * and its reason (the rules it stood on) in its message, and the
 * reclassifier's own `reclassifier` line passes: the lines and the rollup
 * agree.
 *
 * A cell that did not run (skipped, or never driven) holds no line. On a
 * bundle the replay wrote whole, the lines leave the cell short of a pass
 * where the rollup does: failed, or still pending.
 */
import { type Bundle, Verification } from "@sip/contracts"
import * as Option from "effect/Option"
import * as Schema from "effect/Schema"

type Line = Verification.Verification

/** The cell file holding the lines its lane answered in process. */
export const VERIFICATIONS_FILE = Verification.FILE

const decodeAnswered = Schema.decodeUnknownOption(Schema.Array(Verification.Verification))

/** `verifications.json` as read: its lines, null where the file is absent or holds no lines. */
export const answeredOf = (json: unknown): ReadonlyArray<Line> | null =>
  json === null ? null : Option.getOrNull(decodeAnswered(json))

/** What a cell's bundle says of it, as `bundle.ts` read it. */
export interface CellReading {
  readonly passed: boolean
  /** `verdict.json`, null where the cell wrote none. */
  readonly verdict: Bundle.RunVerdict | null
  /** The audit's findings, empty where the cell was not audited; null where `rfc.json` is absent or not audited. */
  readonly audit: ReadonlyArray<Bundle.RfcFinding> | null
  /** Why `rfc.json` states the cell not audited; null where it audited the cell or is absent. */
  readonly unaudited: string | null
  /** The gating findings the SUT answers for; every gating finding of a cell the reclassifier restated. */
  readonly charged: ReadonlyArray<Bundle.RfcFinding>
  /** `verifications.json`: the lines the lane answered in process; null where the cell wrote none. */
  readonly answered: ReadonlyArray<Line> | null
  /** The reclassifier's reason where it restated the failed run as passed; null where it did not. */
  readonly restated: string | null
}

/** The line names and sources this reading writes. */
export const CALL = "call"
export const RFC_AUDIT = "rfc-audit"
export const RUN = "run"
export const RECLASSIFIER = "reclassifier"

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

/** The audit's line of a cell `rfc.json` states not audited, for `reason`. */
const unauditedOf = (reason: string): Line => ({
  name: RFC_AUDIT,
  source: "rfc audit",
  answer: "not-checkable",
  message: `not audited: ${reason}`
})

/** Each check `verdict` states a lane's known bug stood down, as a waived failure of the call. */
const waivedOf = (verdict: Bundle.RunVerdict | null): ReadonlyArray<Line> =>
  (verdict?.waived ?? []).map((waived) => ({
    name: CALL,
    source: "interpreter",
    answer: "fail",
    message: `known bug ${waived.bug}: ${failureLine(waived.finding)}`,
    waived: true
  }))

/**
 * Each failure of `lines` that decides the cell, waived by the reclassifier
 * that restated it as passed for `reason`, and the reclassifier's own line.
 */
const restatedOf = (lines: ReadonlyArray<Line>, reason: string): ReadonlyArray<Line> => [
  ...lines.map((line): Line =>
    line.answer === "fail" && Verification.decides(line)
      ? {
        ...line,
        source: RECLASSIFIER,
        message: `restated as passed (${reason}): ${line.message ?? `the ${line.name} line failed`}`,
        waived: true
      }
      : line
  ),
  { name: RECLASSIFIER, source: RECLASSIFIER, answer: "pass", message: reason }
]

/** The lines of a cell that ran, in the order the lane's (or the call's), audit, run. */
export const linesOf = (reading: CellReading): ReadonlyArray<Line> => {
  const own = reading.answered ?? [callOf(reading.verdict), ...waivedOf(reading.verdict)]
  const audit = reading.audit !== null
    ? auditOf(reading.audit, reading.charged)
    : reading.unaudited !== null
    ? [unauditedOf(reading.unaudited)]
    : []
  const read = [...own, ...audit]
  const lines = reading.restated === null ? read : restatedOf(read, reading.restated)
  const short = lines.some((line) => (line.answer === "fail" || line.answer === "pending") && Verification.decides(line))
  if (reading.passed || short) return lines
  return [...lines, {
    name: RUN,
    source: "driver",
    answer: "fail",
    message: "the run failed the cell beyond its lines"
  }]
}
