/**
 * Comparing two run sets: which calls moved, how, and whether
 * what they ran was the same.
 *
 * Only the calls BOTH sides ran are compared, keyed by (capture, case id);
 * what one side alone holds is counted, never classed, so two differently
 * narrowed populations cannot pass for a code change. Each compared call takes
 * one class, the first that applies: a lane block that changed, a verdict that
 * flipped, a failure that moved (its step or its main error), a bless class
 * that moved between clean and `known-bug`, repeats within a side that
 * disagree (`unstable`), or nothing.
 *
 * A move is an ATTRIBUTION only where both cells ran the same code over the
 * same document on the same lane behaviour. A commit sha cannot say that of a
 * dirty tree, so the code ruling reads the tree FINGERPRINTS; every call says
 * whether its document, its code and its lane changed, and a flip with none
 * changed is the harness's own population. Pure: the store reads
 * the sides (`run-sets.ts`).
 */
import { CompareReport } from "@sip/contracts"
import type { MainError, Provenance } from "./model.js"

/** One call's outcome in one side, as `case_results` and its run recorded it. */
export interface Cell {
  readonly capture: string
  readonly caseId: string
  readonly runId: number
  readonly lane: string
  readonly env: string | null
  readonly branch: string | null
  readonly laneHash: string | null
  readonly prepId: number | null
  /** The bundle the run wrote, absolute. */
  readonly bundlePath: string
  readonly passed: boolean
  /** `verdict.json`'s status, or `absent`. */
  readonly verdict: string
  /** The lane verdict that kept it from running; null = it ran. */
  readonly skipped: string | null
  readonly failedStep: string | null
  readonly documentHash: string | null
  readonly mainError: MainError | null
  /** The cell's confrontation counts; null where the cell was not classified. */
  readonly deltas: CellDeltas | null
  readonly code: Provenance
  /** The side's runs of the call on this cell's code, document and lane, this one included; absent is this one alone. */
  readonly repeats?: Repeats
}

/** How many runs repeated a call on one code, document and lane, and how many of them passed. */
export interface Repeats {
  readonly runs: number
  readonly passed: number
}

/** The cell's repeats, itself alone where none were read. */
export const repeatsOf = (cell: Cell): Repeats => cell.repeats ?? { runs: 1, passed: cell.passed ? 1 : 0 }

/** Whether a cell's repeats disagree: some passed and some did not. */
export const isUnstable = (cell: Cell): boolean => {
  const repeats = repeatsOf(cell)
  return repeats.passed > 0 && repeats.passed < repeats.runs
}

/** The confrontation counts a bless class is read from. */
export interface CellDeltas {
  readonly accepted: number
  readonly knownBug: number
  readonly unlisted: number
  readonly unknown: number
}

/** A sweep a side's runs belong to, with the population it stated. */
export interface SideSweep {
  readonly id: number
  readonly universe: string
  /** The predicates the sweep was opened with; null for a sweep recorded before they were. */
  readonly selection: string | null
  readonly captures: number
}

/**
 * What a side holds that the other does not, as the store counted it without
 * loading it: the captures the other side never ran, and within the captures
 * both ran, the calls only this side holds.
 */
export interface Elsewhere {
  readonly captures: number
  readonly calls: number
  /** Those calls by `capture/case`, the first few. */
  readonly keys: ReadonlyArray<string>
}

export const NOWHERE_ELSE: Elsewhere = { captures: 0, calls: 0, keys: [] }

/** One side: the selector as given, the runs it named, the sweeps they came from, and its cells. */
export interface Side {
  readonly selector: string
  readonly runs: number
  readonly sweeps: ReadonlyArray<SideSweep>
  /** The (capture, case) cells the side holds, each at the side's latest run holding it. */
  readonly cells: ReadonlyArray<Cell>
  /** What the side holds beyond `cells` that the other side does not. */
  readonly elsewhere: Elsewhere
}

/**
 * Whether two cells ran the same code. `unknown` wherever a fingerprint is
 * missing: a run recorded before trees were fingerprinted cannot be told apart
 * from any other, and saying so beats reading it as `same`.
 */
export type CodeReading = CompareReport.CodeReading

/** Whether two cells ran on one lane behaviour (`run-identity.ts` `laneHash`); unknown where either hash is. */
export type LaneReading = CompareReport.LaneReading

/** Whether two cells ran one case document. */
export type DocumentReading = CompareReport.DocumentReading

/**
 * A cell's bless class: what its confrontation records needed. `unknown` and
 * `unlisted` are records no rule accounts for, `known-bug` records a bug rule
 * accounts for, `clean` none of those; `unmeasured` where no classification
 * was recorded.
 */
export type BlessClass = CompareReport.BlessClass

/** The one class a compared call takes (`@sip/contracts` `compare-report.ts`). */
export type MoveClass = CompareReport.MoveClass

export const MOVE_CLASSES: ReadonlyArray<MoveClass> = CompareReport.MOVE_CLASSES

/** One compared call. */
export interface Move {
  readonly class: MoveClass
  readonly capture: string
  readonly caseId: string
  readonly document: DocumentReading
  readonly code: CodeReading
  readonly lane: LaneReading
  readonly before: Cell
  readonly after: Cell
}

/** A class's count, split by whether the document moved under it. */
export interface ClassCount {
  readonly sameDocument: number
  readonly documentChanged: number
}

export interface Comparison {
  readonly before: Side
  readonly after: Side
  /** Calls both sides ran. */
  readonly compared: number
  readonly counts: Readonly<Record<MoveClass, ClassCount>>
  /** Every compared call, in class order then capture and case. */
  readonly moves: ReadonlyArray<Move>
  /** Calls one side alone ran within the captures both ran: their count, and the first few by `capture/case`. */
  readonly onlyBefore: Elsewhere
  readonly onlyAfter: Elsewhere
}

/** Each component's tree fingerprint, by component name. */
const trees = (code: Provenance): ReadonlyMap<string, string | null> =>
  new Map(code.map((component) => [component.name, component.tree]))

/**
 * The code ruling: fingerprints decide it, component by component, and a
 * missing one decides nothing. Two sides naming different components ran
 * different code; a side naming none is unknown.
 */
export const codeReading = (before: Provenance, after: Provenance): CodeReading => {
  if (before.length === 0 || after.length === 0) return "unknown"
  const was = trees(before)
  const is = trees(after)
  if ([...was.values(), ...is.values()].some((tree) => tree === null)) return "unknown"
  if (was.size !== is.size) return "different"
  return [...was].every(([name, tree]) => is.has(name) && is.get(name) === tree) ? "same" : "different"
}

const laneReading = (before: Cell, after: Cell): LaneReading =>
  before.laneHash === null || after.laneHash === null
    ? "unknown"
    : before.laneHash === after.laneHash
    ? "same"
    : "different"

const documentReading = (before: Cell, after: Cell): DocumentReading =>
  before.documentHash === null || after.documentHash === null
    ? "unknown"
    : before.documentHash === after.documentHash
    ? "same"
    : "changed"

/** A cell's bless class off its confrontation counts. */
export const blessClass = (cell: Cell): BlessClass => {
  const deltas = cell.deltas
  if (deltas === null) return "unmeasured"
  if (deltas.unknown > 0) return "unknown"
  if (deltas.unlisted > 0) return "unlisted"
  if (deltas.knownBug > 0) return "known-bug"
  return "clean"
}

/** What a failure is said to be, for telling a moved failure from the same one. */
const failureOf = (cell: Cell): string => {
  const error = cell.mainError
  return [
    cell.failedStep ?? "",
    error?.kind ?? "",
    error?.rule ?? "",
    error?.step ?? "",
    error?.expected ?? ""
  ].join("|")
}

/** The one class of a call both sides ran. */
export const classOf = (before: Cell, after: Cell): MoveClass => {
  const wasSkipped = before.skipped !== null
  const isSkipped = after.skipped !== null
  if (wasSkipped !== isSkipped) return wasSkipped ? "skipped→ran" : "ran→skipped"
  if (wasSkipped) return "unchanged"
  if (before.passed !== after.passed) return after.passed ? "red→green" : "green→red"
  if (!after.passed && failureOf(before) !== failureOf(after)) return "red→red-moved"
  const was = blessClass(before)
  const is = blessClass(after)
  if (was === "known-bug" && is === "clean") return "known-bug→clean"
  if (was === "clean" && is === "known-bug") return "clean→known-bug"
  return isUnstable(before) || isUnstable(after) ? "unstable" : "unchanged"
}

const keyOf = (cell: Pick<Cell, "capture" | "caseId">): string => `${cell.capture}/${cell.caseId}`

const ZERO: ClassCount = { sameDocument: 0, documentChanged: 0 }

/** What changed between two sides, call by call. */
export const compare = (before: Side, after: Side): Comparison => {
  const was = new Map(before.cells.map((cell) => [keyOf(cell), cell]))
  const seen = new Set<string>()
  const moves: Array<Move> = []
  const onlyAfter: Array<string> = []
  const counts = Object.fromEntries(MOVE_CLASSES.map((one) => [one, ZERO])) as Record<MoveClass, ClassCount>
  for (const cell of after.cells) {
    const key = keyOf(cell)
    const then = was.get(key)
    if (then === undefined) {
      onlyAfter.push(key)
      continue
    }
    seen.add(key)
    const move: Move = {
      class: classOf(then, cell),
      capture: cell.capture,
      caseId: cell.caseId,
      document: documentReading(then, cell),
      code: codeReading(then.code, cell.code),
      lane: laneReading(then, cell),
      before: then,
      after: cell
    }
    moves.push(move)
    const held = counts[move.class]
    counts[move.class] = move.document === "changed"
      ? { ...held, documentChanged: held.documentChanged + 1 }
      : { ...held, sameDocument: held.sameDocument + 1 }
  }
  const order = new Map(MOVE_CLASSES.map((one, at) => [one, at]))
  moves.sort((a, b) =>
    (order.get(a.class)! - order.get(b.class)!) ||
    keyOf(a).localeCompare(keyOf(b))
  )
  return {
    before,
    after,
    compared: moves.length,
    counts,
    moves,
    onlyBefore: only(before.cells.map(keyOf).filter((key) => !seen.has(key)), before.elsewhere),
    onlyAfter: only(onlyAfter, after.elsewhere)
  }
}

/** The calls a side alone holds: those its loaded cells show, and those the store counted. */
const only = (loaded: ReadonlyArray<string>, counted: Elsewhere): Elsewhere => ({
  captures: counted.captures,
  calls: loaded.length + counted.calls,
  keys: [...loaded, ...counted.keys].sort()
})

/**
 * What nothing but the harness explains: a verdict flip on the same code, the
 * same document and the same lane, or a call whose repeats within a side disagree.
 */
export const isUnexplained = (move: Move): boolean =>
  move.class === "unstable" ||
  ((move.class === "red→green" || move.class === "green→red") &&
    move.code === "same" && move.document === "same" && move.lane === "same")

/** How many calls moved with nothing to explain them but the harness itself. */
export const unexplained = (comparison: Comparison): number => comparison.moves.filter(isUnexplained).length

/** The code reading of the comparison whole: one reading where every call agrees, else `different` or `unknown`. */
export const overallCode = (comparison: Comparison): CodeReading => {
  const readings = new Set(comparison.moves.map((move) => move.code))
  if (readings.has("unknown") || readings.size === 0) return "unknown"
  return readings.has("different") ? "different" : "same"
}

/** What a comparison may be read as, given what its two sides had in common and what its repeats said. */
export const headline = (comparison: Comparison): string => {
  const unstable = comparison.moves.filter((move) => move.class === "unstable").length
  const flipped = unexplained(comparison) - unstable
  const read = ((): string => {
    switch (overallCode(comparison)) {
      case "different":
        return "the two sides ran DIFFERENT code — every move below is a code change until one " +
          "of them is reproduced on ONE tree"
      case "unknown":
        return "a side did not fingerprint its tree — nothing below can be attributed to either " +
          "the code or the harness"
      case "same":
        return flipped === 0
          ? "same code, and no verdict flipped on an unchanged document and lane"
          : `same code, document and lane on ${flipped} call(s) whose verdict flipped anyway — that is the harness`
    }
  })()
  return unstable === 0 ? read : `${read}; ${unstable} call(s) UNSTABLE: their repeats on one tree disagree`
}
