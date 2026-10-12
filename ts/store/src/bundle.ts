/**
 * Reading a run bundle off disk into rows.
 *
 * The bundle is self-describing: `campaign.json` names the cells and whether
 * each passed, and every cell carries the `pivot.json` it actually ran, its
 * `verdict.json`, its `classification.json` and its `rfc.json`. So the same reader serves the
 * live recorder, the day-one seed and the rebuild — the DB never learns
 * anything the bundle did not already prove.
 *
 * A missing part is an answer, not an error: a run that died before it wrote
 * its index is `error` with no cases, and stays visible as such. A cell that was
 * SKIPPED has no `verdict.json` either — the index's own `skipped` verdict is
 * what says so, and the case is recorded as neither passed nor failed.
 *
 * Each cell's main error is decided here from what the cell wrote
 * (`main-error.ts`), its failing body rows read off
 * `confrontation.ndjson` for a failing cell, and whether the body gate
 * refused it off `rule-hits.json`; which of the audit's charges STAND is the caller's ruling
 * (`charges`), asked only where the audit holds a charge, and every gating
 * finding stands where the caller has none.
 */
import { Bundle, Confrontation as Records, MainError, StepOrder } from "@sip/contracts"
import * as fs from "node:fs"
import * as path from "node:path"
import type { CaseResult, RunStatus } from "./model.js"

const { BAD_BODY, failingBody, mainErrorOf } = MainError
const { stepOrderOf } = StepOrder
type Confrontation = MainError.Confrontation

export interface BundleReading {
  readonly status: RunStatus
  readonly lane: string | undefined
  readonly mode: string | undefined
  readonly detail: string | undefined
  readonly cases: ReadonlyArray<CaseResult>
}

/**
 * The reader's one open question: of an audit's findings — the whole audit,
 * peer findings included, since they are the evidence a ruling reads — which
 * gating ones does the SUT answer for. Called with the cell directory, so a
 * ruling can read the cell's recording.
 */
export type Charges = (
  cellDir: string,
  findings: ReadonlyArray<Bundle.RfcFinding>
) => ReadonlyArray<Bundle.RfcFinding>

export interface Options {
  readonly charges?: Charges
}

/** Every gating finding stands. */
const everyCharge: Charges = (_cellDir, findings) => findings.filter((finding) => finding.gating)

interface Rollup {
  readonly cells?: ReadonlyArray<{
    readonly cell?: { readonly case?: string; readonly infra?: string }
    readonly passed?: boolean
    readonly dir?: string
    readonly skipped?: string
    readonly error?: string
  }>
}

const readJson = (file: string): unknown => {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8")) as unknown
  } catch {
    return null
  }
}

const asRecord = (value: unknown): Record<string, unknown> | null =>
  typeof value === "object" && value !== null ? value as Record<string, unknown> : null

/** The gating count `rfc.json` states; null where the cell wrote none or was not audited. */
const rfcGatingOf = (rfc: Record<string, unknown> | null): number | null => {
  if (rfc === null || rfc["status"] !== "audited" || !Array.isArray(rfc["findings"])) return null
  return (rfc["findings"] as ReadonlyArray<unknown>)
    .filter((f) => asRecord(f)?.["gating"] === true)
    .length
}

/**
 * The verdict under the strict contract, else the file's own `status`,
 * `failed_step` and `failures` read as they stand: a verdict variant the
 * contract does not know yet still names where the run died.
 */
const verdictOf = (raw: Record<string, unknown> | null): Bundle.RunVerdict | null => {
  if (raw === null) return null
  try {
    return Bundle.decodeRunVerdictSync(raw)
  } catch {
    const failures = Array.isArray(raw["failures"])
      ? (raw["failures"] as ReadonlyArray<unknown>).filter((f) => typeof asRecord(f)?.["failure"] === "string")
      : []
    return {
      case: typeof raw["case"] === "string" ? raw["case"] : "",
      lane: typeof raw["lane"] === "string" ? raw["lane"] : "",
      status: raw["status"] === "ok" || raw["status"] === "ok-negative" ? raw["status"] : "failed",
      failures: failures as ReadonlyArray<Bundle.Failure>,
      ...(typeof raw["failed_step"] === "string" ? { failed_step: raw["failed_step"] } : {})
    }
  }
}

/** The audit's findings, under the strict contract else read as they stand; none where the cell was not audited. */
const findingsOf = (rfc: Record<string, unknown> | null): ReadonlyArray<Bundle.RfcFinding> => {
  if (rfc === null || rfc["status"] !== "audited" || !Array.isArray(rfc["findings"])) return []
  try {
    const audit = Bundle.decodeRunRfcAuditSync(rfc)
    return audit.status === "audited" ? audit.findings : []
  } catch {
    return (rfc["findings"] as ReadonlyArray<unknown>).flatMap((entry) => {
      const finding = asRecord(entry)
      if (finding === null || typeof finding["rule"] !== "string" || typeof finding["detail"] !== "string") return []
      return [{
        rule: finding["rule"],
        lane: typeof finding["lane"] === "string" ? finding["lane"] : "",
        detail: finding["detail"],
        advisory: finding["advisory"] === true,
        gating: finding["gating"] === true,
        ...(typeof finding["offending"] === "number" ? { offending: finding["offending"] } : {}),
        ...(typeof finding["charged"] === "string" ? { charged: finding["charged"] } : {}),
        ...(typeof finding["actor"] === "string" ? { actor: finding["actor"] } : {}),
        ...(typeof finding["cancelled_by"] === "string" ? { cancelled_by: finding["cancelled_by"] } : {}),
        ...(typeof finding["caused_by"] === "string" ? { caused_by: finding["caused_by"] } : {})
      } satisfies Bundle.RfcFinding]
    })
  }
}

/** The confrontation counts `classification.json` states; none where the cell wrote no counts. */
const confrontationOf = (classification: Record<string, unknown> | null): Confrontation | undefined => {
  const unknown = classification?.["unknown"]
  const unlisted = classification?.["unlisted"]
  return typeof unknown === "number" && typeof unlisted === "number" ? { unknown, unlisted } : undefined
}

/** The file's lines as text; none where it is absent or unreadable. */
const linesOfFile = (file: string): ReadonlyArray<string> => {
  try {
    return fs.readFileSync(file, "utf8").split("\n").filter((line) => line.trim() !== "")
  } catch {
    return []
  }
}

/** One confrontation line as a record; undefined where it does not decode. */
const recordOf = (line: string): Records.ConfrontationRecord | undefined => {
  try {
    return Records.decodeConfrontationRecordSync(JSON.parse(line) as unknown)
  } catch {
    return undefined
  }
}

/**
 * The cell's body rows no rule accepts, earliest document step
 * first; a line that does not decode is skipped, and a cell with no readable
 * `confrontation.ndjson` has none.
 */
const failingBodyOf = (dir: string, document: unknown): ReadonlyArray<Records.ConfrontationRecord> => {
  const records = linesOfFile(path.join(dir, "confrontation.ndjson")).flatMap((line) => recordOf(line) ?? [])
  return Records.inStepOrder(failingBody(records), stepOrderOf(document))
}

/** Whether the cell's `rule-hits.json` counts the body gate as what refused it. */
const bodyGatedOf = (dir: string): boolean => {
  const hits = readJson(path.join(dir, "rule-hits.json"))
  return Array.isArray(hits) &&
    hits.some((hit) => {
      const one = asRecord(hit)
      return one?.["family"] === "bless" && one["rule"] === BAD_BODY && one["bucket"] === "fired"
    })
}

/** Reads `<runDir>` into the rows one run occupies. */
export const read = (runDir: string, options?: Options): BundleReading => {
  const charges = options?.charges ?? everyCharge
  const rollup = asRecord(readJson(path.join(runDir, "campaign.json"))) as Rollup | null
  if (rollup === null) {
    return {
      status: "error",
      lane: undefined,
      mode: undefined,
      detail: "no campaign.json — the driver wrote no rollup",
      cases: []
    }
  }
  const cells = rollup.cells ?? []
  const cases: Array<CaseResult> = []
  let lane: string | undefined
  let mode: string | undefined
  for (const cell of cells) {
    const dir = cell.dir === undefined ? undefined : path.join(runDir, cell.dir)
    const verdict = dir === undefined ? null : asRecord(readJson(path.join(dir, "verdict.json")))
    const classification = dir === undefined ? null : asRecord(readJson(path.join(dir, "classification.json")))
    const rfc = dir === undefined ? null : asRecord(readJson(path.join(dir, "rfc.json")))
    const document = dir === undefined ? null : readJson(path.join(dir, "pivot.json"))
    const caseId = cell.cell?.case ??
      (typeof verdict?.["case"] === "string" ? verdict["case"] as string : undefined)
    if (caseId === undefined) continue
    lane ??= cell.cell?.infra ??
      (typeof verdict?.["lane"] === "string" ? verdict["lane"] as string : undefined)
    mode ??= typeof classification?.["mode"] === "string" ? classification["mode"] as string : undefined
    const passed = cell.passed === true
    const skipped = typeof cell.skipped === "string" ? cell.skipped : null
    const findings = findingsOf(rfc)
    // The ruling costs a recording read, so it is asked only of a failing,
    // audited cell that holds a charge to rule on.
    const charged = passed || skipped !== null || dir === undefined || !findings.some((f) => f.gating)
      ? []
      : charges(dir, findings)
    // Only a failing cell can carry one: the body gate fails every other.
    const body = passed || skipped !== null || dir === undefined ? [] : failingBodyOf(dir, document)
    cases.push({
      caseId,
      passed,
      verdict: typeof verdict?.["status"] === "string" ? verdict["status"] as string : "absent",
      skipped,
      failedStep: typeof verdict?.["failed_step"] === "string" ? verdict["failed_step"] as string : null,
      rfcGating: rfcGatingOf(rfc),
      bodyFailing: body.length,
      mainError: mainErrorOf({
        passed,
        skipped,
        ...(typeof cell.error === "string" ? { error: cell.error } : {}),
        verdict: verdictOf(verdict),
        charged,
        confrontation: confrontationOf(classification),
        body,
        bodyGated: dir !== undefined && body.length > 0 && bodyGatedOf(dir)
      }),
      classification,
      document
    })
  }
  return {
    status: cases.length === 0 ? "no-cases" : "done",
    lane,
    mode,
    detail: cases.length === 0 ? "the rollup names no cell" : undefined,
    cases
  }
}

