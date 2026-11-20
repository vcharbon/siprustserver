/**
 * How the pages read verification lines and cell statuses: an answer's mark,
 * a status's tone, the key a line is filtered by (its name, its answer and its
 * mark), the statuses in the order the pages list them, and the four detail
 * fields a line may state.
 */
import type { CellStatus, VerificationAnswer, VerificationLine } from "./api.ts"

/** Each answer's mark: `✓ pass · ✗ fail · ? not checkable · … pending`. */
export const ANSWER_MARK: Readonly<Record<VerificationAnswer, string>> = {
  pass: "✓",
  fail: "✗",
  "not-checkable": "?",
  pending: "…"
}

/** Every status, in the order the pages list them. */
export const STATUSES: readonly CellStatus[] = [
  "pass",
  "fail",
  "fail, still verifying",
  "ok so far",
  "unverified",
  "error",
  "excluded",
  "not runnable",
  "not ported"
]

/** A status's tone, as a `cp-dot` class: `pass`, `failure`, `error`, else `none`. */
export const statusTone = (status: CellStatus): "pass" | "failure" | "error" | "none" =>
  status === "pass"
    ? "pass"
    : status === "fail" || status === "fail, still verifying"
    ? "failure"
    : status === "unverified" || status === "error"
    ? "error"
    : "none"

/** An answer's tone, as a `cp-dot` class; a waived or informative failure decides nothing and reads `none`. */
export const answerTone = (line: Pick<VerificationLine, "answer" | "waived" | "informative">): string =>
  line.answer === "pass"
    ? "pass"
    : line.answer === "fail"
    ? line.waived || line.informative ? "none" : "failure"
    : line.answer === "not-checkable"
    ? "error"
    : "none"

/** The key a line is filtered by: its name and its answer, and its mark where it has one (`call fail waived`). */
export const lineKey = (line: Pick<VerificationLine, "name" | "answer" | "waived" | "informative">): string =>
  `${line.name} ${line.answer}${line.waived ? " waived" : line.informative ? " informative" : ""}`

const text = (value: unknown): string | null => (typeof value === "string" ? value : null)

/** The four detail fields a line may state; null where it does not. */
export const detailOf = (line: Pick<VerificationLine, "detail">): {
  readonly step: string | null
  readonly expected: string | null
  readonly seen: string | null
  readonly side: string | null
} => ({
  step: text(line.detail?.["step"]),
  expected: text(line.detail?.["expected"]),
  seen: text(line.detail?.["seen"]),
  side: text(line.detail?.["side"])
})
