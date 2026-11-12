/**
 * A campaign run's units: a compiled cell (one sub-case of one case, a case
 * directory the replay tool reads), and what it came to on one lane.
 *
 * A pack's cell carries what its cases state beyond these fields (its
 * description, its claims' payloads, how its calls route) as an extension of
 * {@link Cell}; every function here is generic over the cell and hands back
 * the pack's own.
 */
import { Verification } from "@sip/contracts"
import type { Check } from "@sip/store/campaign-results"

/** A lane a campaign run drives, in its short form (its catalog id less the pack's lane prefix). */
export type Lane = string

/** How long a lane holds after a cell, in ms: a real-clock lane the first, the simulated lane the second, on the real clock. */
export interface Heal {
  readonly healMs: number
  readonly healRealClockOnSimulatedMs: number
}

/** One check a cell makes, passing or not. */
export interface Checked {
  /** The flow step it is made at; null for a gate. */
  readonly step: string | null
  readonly anchor: string
  readonly field: string
  readonly op: string
  /** Null for an operator that states no value (`absent`, `exists`). */
  readonly expected: string | null
}

export interface Cell {
  /** The compiled document's case id: the cell's directory name. */
  readonly id: string
  /** The case's stable test id. */
  readonly ref: string
  readonly subcase: string
  /** How many times a cell that ends `failure` on a lane is replayed again there (`run.ts`). */
  readonly retries: number
  /** The campaign tags its case carries: its own and those the campaign puts on its file. */
  readonly tags: ReadonlyArray<string>
  /** Whether it runs with nothing else on its lane: a tag it carries states so. */
  readonly alone: boolean
  /** What its lane holds after it. */
  readonly heal: Heal
  /** Every check the cell makes, passing or not. */
  readonly checked: ReadonlyArray<Checked>
  /** The case file it was compiled from: its path as the run names it, and its text as compiled. */
  readonly source: { readonly file: string; readonly text: string }
  /** The named locks the cell holds on its lane while it runs. */
  readonly locks: ReadonlyArray<string>
  /** The lane capabilities its case needs beyond every campaign case's. */
  readonly needs: ReadonlyArray<string>
  /**
   * The operation names its case asks for as steps of its call: a lane binding
   * none of one does not run it; absent where it asks for none.
   */
  readonly operations?: ReadonlyArray<string>
  /** The case directory: `scenario.json`, `case.json`, its resources. */
  readonly dir: string
  /** The document's endpoint ids. */
  readonly endpoints: ReadonlyArray<string>
  /** The endpoint the first call's routed b-legs land on (its caller's where it dials nobody). */
  readonly egress: string
  /** The lane timers the cell was compiled with, by name: a lane stating another value does not run it. */
  readonly laneTimers?: Readonly<Partial<Record<string, number>>>
  /**
   * The collectors its case claims of that no part of a lane answers
   * (upstream ADR-0060 appendix §4): a run that passes it skips it, naming
   * them; absent where there is none.
   */
  readonly notRequested?: ReadonlyArray<string>
}

/**
 * `pass`, `failure` (the call was judged and a check, a gate such as the
 * cleanup, or the RFC audit failed), `error` (no call was judged: refused,
 * environment, panicked, or a phase that did not run), or `skipped` (the cell
 * is not runnable on the lane, or a check of a cell that passed the rest is
 * not checkable there: never a pass, never an error).
 */
export type Status = "pass" | "failure" | "error" | "skipped"

export interface CellResult<C extends Cell = Cell> {
  readonly lane: Lane
  readonly cell: C
  readonly status: Status
  /** What failed, one line per failed check; empty on a pass. */
  readonly message: string
  /** The cell's bundle, where it wrote one. */
  readonly bundle?: string
  /** `verdict.json`'s own status, `absent` where it wrote none. */
  readonly verdict: string
  /** The step the call died at, where it names one. */
  readonly failedStep: string | null
  /** The gating findings of the RFC audit; null where the cell was not audited. */
  readonly rfcGating: number | null
  /** Every failed check, typed; empty on a pass and on an error. */
  readonly checks: ReadonlyArray<Check>
  /** The lines the lane answered in process (`verifications.json`), where the cell's bundle holds them. */
  readonly answered?: ReadonlyArray<Verification.Verification>
  /** The audit's line (`rfc.json`), where the cell's bundle holds one. */
  readonly audit?: Verification.Verification
  /** The line of a verification the lane could not make, where the cell was skipped as not checkable for it. */
  readonly unchecked?: Verification.Verification
  /** What the rig's collectors answered of the cell, where they answered anything (`withCollected`). */
  readonly collected?: ReadonlyArray<Verification.Verification>
  /** Why a skipped cell was skipped (`not-runnable: …`, `not-checkable: …`, `excluded: …`). */
  readonly skipped?: string
  /** Every attempt of a cell replayed more than once on the lane, in order, the kept one marked; absent for one attempt. */
  readonly attempts?: ReadonlyArray<Attempt>
}

/** One replay of a cell on a lane: its bundle where it wrote one, its status, its failed checks, whether it is the lane's verdict. */
export interface Attempt {
  readonly bundle?: string
  readonly status: Status
  readonly failed: number
  readonly kept: boolean
}

/** `result` skipped for `reason`: a cell that is neither a pass nor an error. */
export const skippedOf = <C extends Cell>(
  result: CellResult<C>,
  reason: string,
  unchecked?: Verification.Verification
): CellResult<C> => ({
  ...result,
  status: "skipped",
  message: reason,
  skipped: reason,
  ...(unchecked === undefined ? {} : { unchecked })
})

/**
 * `result` with `lines`, what one scope's rig collectors answered of its cell:
 * a line answering one of the cell's own `pending` lines of its name and
 * source takes its place (the store's `CellAnswers.merged`), any other is
 * stored after what earlier scopes answered, beside its own lines
 * (`record.ts`); a pass one of them fails is a failure, its message naming
 * each failing line.
 */
export const withCollected = <C extends Cell>(
  result: CellResult<C>,
  lines: ReadonlyArray<Verification.Verification>
): CellResult<C> => {
  if (lines.length === 0) return result
  const answered = [...(result.answered ?? [])]
  const beside: Array<Verification.Verification> = []
  for (const line of lines) {
    const at = answered.findIndex((own) => own.name === line.name && own.source === line.source && own.answer === "pending")
    if (at >= 0) answered[at] = line
    else beside.push(line)
  }
  const collected = [...(result.collected ?? []), ...beside]
  const merged = { ...result, ...(result.answered === undefined ? {} : { answered }), collected }
  const failing = Verification.failing(lines)
  return result.status === "pass" && failing.length > 0
    ? {
      ...merged,
      status: "failure",
      message: failing.map((line) => `${line.name} (${line.source}): ${line.message ?? "failed"}`).join("\n")
    }
    : merged
}

/** A cell no call was judged in on `lane`, for `message`. */
export const errored = <C extends Cell>(lane: Lane, cell: C, message: string): CellResult<C> => ({
  lane,
  cell,
  status: "error",
  message,
  verdict: "absent",
  failedStep: null,
  rfcGating: null,
  checks: []
})
