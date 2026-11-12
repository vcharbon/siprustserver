/**
 * A campaign run's results as the run store holds them (`campaign-runs.ts`):
 * per lane it drove, a store `runs` row whose capture names the campaign; each
 * case × sub-case a case result whose `case_id` is the test ref and the
 * sub-case ({@link caseIdOf}), a cell that ran holding its verification lines,
 * each failed check one of them, kept in the line's detail ({@link detailOf},
 * read back by {@link checkOf}). A cell replayed more than once on its lane
 * states every attempt ({@link Attempt}); its result is the kept one's.
 *
 * A case result reads back as a cell status ({@link statusOf}): the final
 * status its lines give a cell that ran, one skipped `not-checkable: …` (its
 * lane cannot make one of its checks) included; `error` where no call was
 * judged (its main error is `refused`); or why it did not run, from its
 * `skipped` reason (`not-ported` for a case no file ports yet,
 * `not-runnable: …` for a cell its lane or target cannot run, `excluded: …`
 * for one a campaign tag left out). Its outcome ({@link outcomeOf}) is what a
 * JUnit report makes of that status.
 */
import { Verification } from "@sip/contracts"
import type { Provenance } from "./model.js"

/**
 * One failed check of a case result. `S` is the pack's sides: who a check is
 * typed to (the backend's decision, the SUT, both).
 */
export interface Check<S extends string = string> {
  /** Where it was checked: a leg's message (`B1 INVITE`, `A final`), or a gate (`post-run cleanup`, `rfc`). */
  readonly anchor: string
  readonly field: string
  /** Null where the check states no value (`absent`, `exists`). */
  readonly expected: string | null
  /** Null where the wire carried none. */
  readonly seen: string | null
  /** What the backend's decision stated of the field; null where the check reads nothing the backend decides. */
  readonly said: string | null
  /** Null where the data on hand cannot type it. */
  readonly side: S | null
  /**
   * `value`: expected and seen are two values of one thing (a header, a
   * status, a message shape, a count), read side by side character by
   * character; `statement`: the expected is a rule the seen broke.
   */
  readonly compare?: Compare | null
  /** The flow step it was made at (`s3`); null for a gate's or a check no step names. */
  readonly step?: string | null
  /** Its index in `verdict.json`'s failures, where a verdict failure states it. */
  readonly failure?: number
}

/** How a check's expected and seen are read against each other. */
export type Compare = "value" | "statement"

/** The skipped reason of a case no file ports yet. */
export const NOT_PORTED = "not-ported"

/** The skipped reason's head of a cell using what its lane or target does not hold: `not-runnable: <why>`. */
export const NOT_RUNNABLE = "not-runnable"

/** The skipped reason's head of a cell whose every other check passed and one its lane cannot make: `not-checkable: <why>`. */
export const NOT_CHECKABLE = "not-checkable"

/** The skipped reason's head of a cell a campaign left out by a tag of its case: `excluded: <why>`. */
export const EXCLUDED = "excluded"

/** What a run's tags selected: the tags it opted into, and each lane's tags it left out. */
export interface RunSelection {
  readonly allow: ReadonlyArray<string>
  readonly lanes: Readonly<Record<string, ReadonlyArray<string>>>
}

/** The main-error rule of a case no call was judged in. */
export const ERROR_RULE = "cell-error"

/** A case result's `case_id`: the test ref, and the sub-case where one ran. */
export const caseIdOf = (ref: string, subcase?: string): string =>
  subcase === undefined ? ref : `${ref} [${subcase}]`

/** The ref and the sub-case a `case_id` names. */
export const caseNameOf = (caseId: string): { readonly ref: string; readonly subcase: string | undefined } => {
  const match = /^(.*) \[([^\]]*)\]$/.exec(caseId)
  return match === null ? { ref: caseId, subcase: undefined } : { ref: match[1]!, subcase: match[2]! }
}

export type Outcome = "pass" | "failure" | "error" | "skipped"

/** What became of the log the run keeps of a cell's backend: its records counted, or why it is missing. */
export type Log = { readonly records: number } | { readonly missing: string }

/** One replay of a cell on its lane: its bundle directory, its outcome, its failed checks, whether it is the result. */
export interface Attempt {
  /** Null where the attempt wrote no bundle. */
  readonly cell: string | null
  readonly outcome: Outcome
  readonly failed: number
  readonly kept: boolean
}

export interface Result<S extends string = string> {
  readonly caseId: string
  /** The cell's bundle directory under its run's; null for a case that ran no cell. */
  readonly cell: string | null
  /** Null where no log was asked for. */
  readonly log: Log | null
  readonly passed: boolean
  readonly skipped: string | null
  readonly verdict: string
  readonly errorKind: string | null
  readonly errorRule: string | null
  readonly errorDetail: string | null
  /** Its failed checks, read off its lines that decide it. */
  readonly checks: ReadonlyArray<Check<S>>
  /** Its verification lines in their order; empty for a cell that did not run. */
  readonly verifications: ReadonlyArray<Verification.Verification>
  /** Every attempt, in order, where the cell was replayed more than once on its lane; absent for one attempt. */
  readonly attempts?: ReadonlyArray<Attempt>
}

/** Whether a campaign tag left `result`'s cell out of its lane. */
export const isExcluded = (result: Pick<Result, "skipped">): boolean => result.skipped?.startsWith(`${EXCLUDED}:`) === true

/**
 * Whether a cell skipped for `skipped` ran: one skipped as not checkable ran
 * every check its lane could make, and holds the line of the one it could not
 * (none for a claim not requested, ADR-0060 appendix §8).
 */
export const ranUnchecked = (skipped: string): boolean => skipped.startsWith(`${NOT_CHECKABLE}:`)

/** Why a cell with `skipped` as its reason did not run: a reason no other status names is a need its lane lacks. */
export const notRunOf = (skipped: string): Verification.NotRun =>
  skipped === NOT_PORTED ? "not ported" : skipped.startsWith(`${EXCLUDED}:`) ? "excluded" : "not runnable"

/**
 * A case result's status: why it did not run, `error` where no call was
 * judged, else its lines' final status (a cell skipped as not checkable ran:
 * its lines' too).
 */
export const statusOf = (result: Pick<Result, "skipped" | "errorKind" | "verifications">): Verification.CellStatus =>
  result.skipped !== null && !ranUnchecked(result.skipped)
    ? notRunOf(result.skipped)
    : result.errorKind === "refused"
    ? "error"
    : Verification.finalStatusOf(result.verifications)

/**
 * A case result's outcome, its status as a JUnit report reads it: a cell
 * still verifying at the report stands as a failure where a line already
 * failed it, else as an error (nothing verified it whole).
 */
export const outcomeOf = (result: Pick<Result, "skipped" | "errorKind" | "verifications">): Outcome => {
  const status = statusOf(result)
  switch (Verification.junitOf(status)) {
    case "passed":
      return "pass"
    case "failure":
      return "failure"
    case "skipped":
      return "skipped"
    case "error":
      return "error"
    case "open":
      return status === "fail, still verifying" ? "failure" : "error"
  }
}

/** `check` as the detail of its failing line: every field it states, none it leaves null. */
export const detailOf = (check: Check): Verification.Detail => ({
  anchor: check.anchor,
  field: check.field,
  ...(check.step === undefined || check.step === null ? {} : { step: check.step }),
  ...(check.expected === null ? {} : { expected: check.expected }),
  ...(check.seen === null ? {} : { seen: check.seen }),
  ...(check.side === null ? {} : { side: check.side }),
  ...(check.said === null ? {} : { said: check.said }),
  ...(check.compare === undefined || check.compare === null ? {} : { compare: check.compare }),
  ...(check.failure === undefined ? {} : { failure: check.failure })
})

const text = (value: unknown): string | null => (typeof value === "string" ? value : null)

/**
 * The failed check a failing line states ({@link detailOf} read back), its
 * side read as one of `S` unchecked; undefined for a line no check wrote (it
 * names no anchor and field).
 */
export const checkOf = <S extends string = string>(line: Verification.Verification): Check<S> | undefined => {
  const detail = line.detail
  if (line.answer !== "fail" || detail === undefined) return undefined
  const anchor = text(detail["anchor"])
  const field = text(detail["field"])
  if (anchor === null || field === null) return undefined
  const compare = text(detail["compare"]) as Compare | null
  const failure = detail["failure"]
  return {
    anchor,
    field,
    expected: detail.expected ?? null,
    seen: detail.seen ?? null,
    said: text(detail["said"]),
    side: (detail.side ?? null) as S | null,
    ...(compare === null ? {} : { compare }),
    ...(detail.step === undefined ? {} : { step: detail.step }),
    ...(typeof failure === "number" ? { failure } : {})
  }
}

/** A result's failed checks: those its deciding lines state (a waived or informative failure fails no check). */
export const checksOf = <S extends string = string>(
  lines: ReadonlyArray<Verification.Verification>
): ReadonlyArray<Check<S>> => lines.filter(Verification.decides).flatMap((line) => checkOf<S>(line) ?? [])

/** One lane of a campaign run: its store run row and its case results. */
export interface Lane<S extends string = string> {
  readonly runId: number
  readonly lane: string
  readonly status: string
  readonly detail: string | null
  /** Absolute. */
  readonly bundlePath: string
  readonly components: Provenance
  readonly results: ReadonlyArray<Result<S>>
}

/** A campaign run as the store reads it back: what a pack's record adds (its target, its cases' descriptions) beside it. */
export interface CampaignRun<S extends string = string> {
  readonly id: number
  readonly capture: string
  readonly env: string | null
  readonly branch: string | null
  /** The provisioning the run seeded from; null where it states none. */
  readonly seed: string | null
  /** The run's own directory: its compiled cells, dump and reports. */
  readonly dir: string
  /** What the run covered where it was not the whole campaign (`<ref> [<sub-case>]`, a glob); null for a full run. */
  readonly scope: string | null
  /** What its tags selected; null where the run states none. */
  readonly selection: RunSelection | null
  readonly startedAt: string
  readonly finishedAt: string | null
  readonly lanes: ReadonlyArray<Lane<S>>
}
