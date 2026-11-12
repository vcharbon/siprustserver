/**
 * A campaign cell's failed checks as verification lines (`@sip/store`
 * `Verifications`), and back. Each failed check is a `fail` line of the
 * verification it belongs to ({@link nameOf}), the check kept whole in the
 * line's detail (`@sip/store` `CampaignResults.detailOf`, read back by
 * `checkOf`).
 * A cell that ran holds the lines its lane answered in process and its audit's
 * line, each failing one standing as its typed checks; where the lane answered
 * none, a `call` line in their place, failing where a check of the call failed.
 *
 * The checks a pack types itself, off no lane line, are its {@link Own} list:
 * each function takes it first, after the audit's own entry.
 */
import { Verification } from "@sip/contracts"
import { type Check, detailOf } from "@sip/store/campaign-results"

type Line = Verification.Verification

export { checkOf } from "@sip/store/campaign-results"

/** The verification the interpreter answers for the call itself. */
export const CALL = "call"

/** The audit's verification, answered off `rfc.json`. */
export const RFC_AUDIT = "rfc-audit"

/** A check a pack types itself, off no lane line: the verification it belongs to, its source, which checks it holds. */
export interface OwnCheck {
  readonly name: string
  readonly source: string
  readonly holds: (check: Check) => boolean
}

/** The checks a pack types itself, in the order they are asked; the audit's comes before them. */
export type Own = ReadonlyArray<OwnCheck>

/** The audit's own check: a gate failure anchored at `rfc`. */
const AUDIT: OwnCheck = { name: RFC_AUDIT, source: "rfc audit", holds: (check) => check.anchor === "rfc" }

/** The verdict failures a lane line states it was charged with (`detail.failures`). */
const chargedOf = (line: Line): ReadonlyArray<number> => {
  const failures = line.detail?.["failures"]
  return Array.isArray(failures) ? failures.filter((at): at is number => typeof at === "number") : []
}

/**
 * The verification `check` belongs to and its source: the lane line charged
 * with the verdict failure it states, else the audit's or the one `own`
 * types it to, else the call's.
 */
export const nameOf = (
  own: Own,
  check: Check,
  answered: ReadonlyArray<Line> = []
): { readonly name: string; readonly source: string } => {
  const failure = check.failure
  const charged = failure === undefined
    ? undefined
    : answered.find((line) => Verification.decides(line) && chargedOf(line).includes(failure))
  if (charged !== undefined) return { name: charged.name, source: charged.source }
  const typed = [AUDIT, ...own].find((one) => one.holds(check))
  return typed === undefined ? { name: CALL, source: "interpreter" } : { name: typed.name, source: typed.source }
}

/** `check` as a failing line of its verification, among the lines its lane `answered`. */
export const lineOf = (own: Own, check: Check, answered: ReadonlyArray<Line> = []): Line => {
  const { name, source } = nameOf(own, check, answered)
  return {
    name,
    source,
    answer: "fail",
    message: `${check.anchor} ${check.field}`,
    detail: detailOf(check)
  }
}

/**
 * The lines of a cell that ran with `checks` failed: the lines its lane
 * `answered` (else a `call` line, failing where a check of the call failed),
 * then its `audit` line where it states one; each that decides the cell
 * standing as the failed checks that belong to it (its source kept) where
 * there are any, and the checks no such line names following.
 */
export const linesOf = (
  own: Own,
  checks: ReadonlyArray<Check>,
  answered?: ReadonlyArray<Line>,
  audit?: Line
): ReadonlyArray<Line> => {
  const failed = checks.map((check) => lineOf(own, check, answered))
  const lane = answered ?? [{
    name: CALL,
    source: "interpreter",
    answer: failed.some((line) => line.name === CALL) ? "fail" : "pass"
  } satisfies Line]
  const stated = audit === undefined ? lane : [...lane, audit]
  const deciding = stated.filter(Verification.decides)
  const kept = stated.flatMap((line): ReadonlyArray<Line> => {
    const typed = Verification.decides(line) ? failed.filter((check) => check.name === line.name) : []
    return typed.length === 0 ? [line] : typed.map((check) => ({ ...check, source: line.source }))
  })
  return [...kept, ...failed.filter((check) => !deciding.some((line) => line.name === check.name))]
}

/**
 * The audit's line of a cell off its `rfc.json`: a pass where it audited the
 * cell and nothing gates, a failure counting the gating findings (each typed
 * as its check), not checkable where it states the cell not audited; none
 * where the cell wrote no `rfc.json`.
 */
export const auditLineOf = (
  rfc: { readonly status: string; readonly reason?: string | undefined; readonly gating: number } | undefined
): Line | undefined => {
  if (rfc === undefined) return undefined
  const line = { name: RFC_AUDIT, source: AUDIT.source }
  if (rfc.status !== "audited") {
    return { ...line, answer: "not-checkable", message: `not audited: ${rfc.reason ?? "no reason stated"}` }
  }
  return rfc.gating === 0 ? { ...line, answer: "pass" } : { ...line, answer: "fail", message: `${rfc.gating} charge(s)` }
}

