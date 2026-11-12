/**
 * How a run's reports state a cell's verification lines: a line's side (its
 * detail's, null where it states none), its label (its name and its message),
 * its expected and seen, and the type of a failure, which the pack's
 * {@link FailureType} reads off the sides of the lines failing the cell.
 */
import { Verification } from "@sip/contracts"
import * as CheckLines from "./check-lines.js"

type Line = Verification.Verification

/** A pack's type of a failure, off the sides of the lines failing the cell (null for a line typed to none). */
export type FailureType = (sides: ReadonlyArray<string | null>) => string

const text = (value: unknown): string | null => (typeof value === "string" ? value : null)

/** The side a line's detail types it to; null where it states none. */
export const sideOf = (line: Line): string | null => text(line.detail?.["side"])

/** `name: message`, or the name alone. */
export const labelOf = (line: Line): string => (line.message === undefined ? line.name : `${line.name}: ${line.message}`)

/** The type `failureType` gives a failure off the sides of the lines failing the cell. */
export const typeOf = (failureType: FailureType, lines: ReadonlyArray<Line>): string =>
  failureType(Verification.failing(lines).map(sideOf))

/** The expected and seen a line states: a failed check's own, else its detail's; null where it states none. */
export const valuesOf = (line: Line): { readonly expected: string | null; readonly seen: string | null } => {
  const check = CheckLines.checkOf(line)
  return check !== undefined
    ? { expected: check.expected, seen: check.seen }
    : { expected: text(line.detail?.["expected"]), seen: text(line.detail?.["seen"]) }
}
