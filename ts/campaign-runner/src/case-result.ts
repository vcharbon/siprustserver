/**
 * A cell's case result as the run store holds it (`@sip/store` `Model`): its
 * `case_id` the ref and its sub-case, the verification lines of a cell that
 * ran (each failed check one of them, over the checks the pack types itself,
 * `check-lines.ts`), what the rig's collectors answered of it after them, and
 * the attachments its bundle lists; a ref no case file covers a
 * skipped `not-ported` result; a cell replayed more than once its attempts.
 * A run's exit code reads each cell as its stored lines read it
 * ({@link storedOutcomeOf}).
 */
import { Verification } from "@sip/contracts"
import { Attachments, type Model } from "@sip/store"
import * as Effect from "effect/Effect"
import * as path from "node:path"
import type { CellResult } from "./cell.js"
import * as CheckLines from "./check-lines.js"
import * as Results from "@sip/store/campaign-results"

const firstLine = (text: string): string => text.split("\n")[0] ?? ""

/**
 * The lines of a cell judged a failure: where no line fails it (a failure no
 * check typed), its deciding `call` line fails with `message`, or a failing
 * `call` line follows them where they hold none; so the lines never pass a
 * failed cell.
 */
export const failedLines = (
  lines: ReadonlyArray<Verification.Verification>,
  message: string
): ReadonlyArray<Verification.Verification> => {
  if (Verification.failing(lines).length > 0) return lines
  const at = lines.findIndex((line) => line.name === CheckLines.CALL && Verification.decides(line))
  return at < 0
    ? [...lines, { name: CheckLines.CALL, source: Verification.BASE_SOURCES.interpreter, answer: "fail", message }]
    : lines.map((line, i) => (i === at ? { ...line, answer: "fail" as const, message } : line))
}

/**
 * A cell's case result as the store holds it, its failed checks typed over
 * `own`: the lines of a cell that ran end with what the rig's collectors
 * answered of it.
 */
export const caseResultOf = (own: CheckLines.Own, result: CellResult): Model.CaseResult => {
  const caseId = Results.caseIdOf(result.cell.ref, result.cell.subcase)
  const base = {
    caseId,
    verdict: result.verdict,
    skipped: null,
    failedStep: result.failedStep,
    rfcGating: result.rfcGating,
    classification: null,
    document: null
  }
  const collected = result.collected ?? []
  switch (result.status) {
    case "pass":
      return {
        ...base,
        passed: true,
        mainError: null,
        verifications: [...CheckLines.linesOf(own, [], result.answered, result.audit), ...collected]
      }
    case "skipped": {
      const skipped = result.skipped ?? result.message
      const lines = Results.ranUnchecked(skipped)
        ? [
          ...CheckLines.linesOf(own, [], result.answered, result.audit),
          ...(result.unchecked === undefined ? [] : [result.unchecked]),
          ...collected
        ]
        : undefined
      return { ...base, passed: false, skipped, mainError: null, ...(lines === undefined ? {} : { verifications: lines }) }
    }
    case "error":
      return {
        ...base,
        passed: false,
        mainError: { kind: "refused", rule: Results.ERROR_RULE, rfc: null, step: null, expected: null, detail: result.message }
      }
    case "failure": {
      const first = result.checks[0]
      return {
        ...base,
        passed: false,
        mainError: {
          kind: "failure",
          rule: first === undefined ? null : `${first.anchor} ${first.field}`,
          rfc: null,
          step: result.failedStep,
          expected: first?.expected ?? null,
          detail: firstLine(result.message)
        },
        verifications: failedLines(
          [...CheckLines.linesOf(own, result.checks, result.answered, result.audit), ...collected],
          firstLine(result.message)
        )
      }
    }
  }
}

/** A cell's case result with the attachments its bundle lists; none stated for a cell that wrote no bundle. */
export const attachedResultOf = (own: CheckLines.Own, result: CellResult) =>
  result.bundle === undefined
    ? Effect.succeed(caseResultOf(own, result))
    : Effect.map(Attachments.listed(result.bundle), (attachments): Model.CaseResult => ({
      ...caseResultOf(own, result),
      attachments
    }))

/** A cell's outcome as its stored case result reads back (`Results.outcomeOf` over its lines). */
export const storedOutcomeOf = (own: CheckLines.Own, result: CellResult): Results.Outcome => {
  const stored = caseResultOf(own, result)
  return Results.outcomeOf({
    skipped: stored.skipped,
    errorKind: stored.mainError?.kind ?? null,
    verifications: stored.verifications ?? []
  })
}

/** A ref no case file covers, skipped. */
export const notPortedOf = (ref: string): Model.CaseResult => ({
  caseId: Results.caseIdOf(ref),
  passed: false,
  verdict: "absent",
  skipped: Results.NOT_PORTED,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null
})

/** A cell's attempts as the store holds them: each bundle by its directory name under the lane's. */
export const attemptsOf = (result: CellResult): ReadonlyArray<Results.Attempt> | undefined =>
  result.attempts?.map((a) => ({
    cell: a.bundle === undefined ? null : path.basename(a.bundle),
    outcome: a.status,
    failed: a.failed,
    kept: a.kept
  }))
