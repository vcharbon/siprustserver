/**
 * The verdict read BACK ONTO THE LADDER: every ruling a run made, indexed by
 * the flow step it was made at, so the arrow that failed says so itself.
 *
 * A verdict lists its findings by step id; a ladder keys its rows by its own
 * coordinate. This file is the only place that crosses the two, and it reads
 * `verdict.json` through guards — the file is written by the Rust interpreter
 * and reaches the browser untyped, so a shape this reader does not recognize
 * contributes nothing rather than rendering something invented. The cell's
 * confrontation rows add one ruling: a body row no rule accepts, the session
 * description's included, is a `bad-body` failure on its own step.
 */
import type { LadderMark, MarkTone } from "@sip/viewer-core"
import type { StepIndex } from "./steps.ts"

const record = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined

const str = (value: unknown): string | undefined => typeof value === "string" ? value : undefined

const list = (value: unknown): readonly unknown[] => Array.isArray(value) ? value : []

/** One ruling, at one step. */
export interface Finding {
  readonly tone: MarkTone
  /** The finding's own tag: `retransmit-count-mismatch`, an RFC rule id, … */
  readonly tag: string
  /** Its remaining fields, for the hover. */
  readonly detail: string
}

/** How loudly a tone speaks: the loudest one on a step is what the row states. */
const WEIGHT: Readonly<Record<MarkTone, number>> = {
  failure: 6,
  declared: 5,
  abandoned: 4,
  violation: 3,
  tolerated: 2,
  informative: 1
}

/** A finding's tag and the rest of it, flattened one field per `·`. */
const flatten = (value: unknown, skip: readonly string[]): { tag: string; detail: string } => {
  const entry = record(value)
  if (entry === undefined) return { tag: "finding", detail: JSON.stringify(value) ?? "" }
  const tag = str(entry["failure"]) ?? str(entry["rule"]) ?? str(entry["class"]) ?? "finding"
  const detail = Object.entries(entry)
    .filter(([key]) => key !== "failure" && !skip.includes(key))
    .map(([key, field]) => `${key}: ${typeof field === "string" ? field : JSON.stringify(field)}`)
    .join(" · ")
  return { tag, detail }
}


/** A side's values as lines: a whole-document value split on its line endings, blank lines dropped. */
const linesOf = (value: unknown): readonly string[] =>
  list(value).flatMap((v) => (str(v) ?? "").split(/\r?\n/)).filter((line) => line !== "")

const ending = (text: string): string => (text.includes("\r\n") ? "CRLF" : text.includes("\n") ? "LF" : "none")

/** What two texts with the same lines differ by: their line endings, their blank lines, or other bytes. */
const bytesDelta = (captured: string, replayed: string): string => {
  const a = ending(captured)
  const b = ending(replayed)
  if (a !== b) return `line endings differ (${a} → ${b})`
  const blanks = (text: string): number => text.split(/\r?\n/).filter((line) => line === "").length
  return blanks(captured) !== blanks(replayed) ? "blank lines differ" : "bytes differ"
}

const joined = (value: unknown): string => list(value).map((v) => str(v) ?? "").join("")

/**
 * A row's signature and the first line where its two sides differ, `(none)`
 * for a side that ran out first; where every line is the same, what the bytes
 * differ by. The same reading as the queue's (`@sip/contracts` `main-error.ts`).
 */
const bodyDetail = (row: Record<string, unknown>): string => {
  const captured = linesOf(row["captured"])
  const replayed = linesOf(row["replayed"])
  let at = 0
  while (at < captured.length && at < replayed.length && captured[at] === replayed[at]) at++
  const said = captured[at] === undefined && replayed[at] === undefined
    ? bytesDelta(joined(row["captured"]), joined(row["replayed"]))
    : `${captured[at] ?? "(none)"} → ${replayed[at] ?? "(none)"}`
  return `${str(row["signature"]) ?? ""}: ${said}`
}

/** A body row no rule accepts: neither `accepted` nor `known-bug` (`@sip/contracts` `MainError.failingBody`). */
const isBadBody = (row: Record<string, unknown>): boolean =>
  row["kind"] === "body" &&
  row["class"] !== "accepted" &&
  row["class"] !== "known-bug"

/** `step "s6"` — how `informative` names the step it was evaluated at. */
const stepOfSite = (site: unknown): string | undefined =>
  /^step "([^"]+)"$/.exec(str(site) ?? "")?.[1]

/**
 * Every ruling of a verdict, and every body row of the cell's
 * confrontation no rule accepts, by step. A step neither names carries
 * nothing: silence is the run holding, and must never be drawn as a finding.
 */
export const findingsByStep = (
  verdict: unknown,
  confrontation: readonly unknown[] = []
): ReadonlyMap<string, readonly Finding[]> => {
  const found = new Map<string, Finding[]>()
  const add = (step: string | undefined, finding: Finding): void => {
    if (step === undefined || step === "") return
    const at = found.get(step)
    if (at === undefined) found.set(step, [finding])
    else at.push(finding)
  }
  const read = record(verdict)
  if (read === undefined) return withBody(found, confrontation)

  for (const failure of list(read["failures"])) {
    const { tag, detail } = flatten(failure, [])
    add(str(record(failure)?.["step"]), { tone: "failure", tag, detail })
  }
  for (const declared of list(read["must_fail"])) {
    const entry = record(declared)
    const failure = str(entry?.["failure"]) ?? "must-fail"
    const outcome = entry?.["observed"] !== undefined
      ? `produced: ${flatten(entry["observed"], []).detail}`
      : str(entry?.["recorded"]) ?? "the run did not produce it"
    add(str(entry?.["step"]), {
      tone: "declared",
      tag: `must fail: ${failure}`,
      detail: outcome
    })
  }
  for (const tolerated of list(read["tolerated"])) {
    const { tag, detail } = flatten(tolerated, [])
    add(str(record(tolerated)?.["step"]), { tone: "tolerated", tag, detail })
  }
  for (const violation of list(read["rfc_violations"])) {
    const entry = record(violation)
    const emitter = str(entry?.["emitter"]) ?? "?"
    add(str(entry?.["step"]), {
      tone: "violation",
      tag: str(entry?.["rule"]) ?? "rfc-violation",
      detail: `emitted by ${emitter} · ${entry?.["gating"] === true ? "gates" : "gates nothing"}`
    })
  }
  for (const note of list(read["informative"])) {
    const entry = record(note)
    const finding = record(entry?.["finding"])
    const { tag, detail } = flatten(entry?.["finding"], ["site"])
    add(stepOfSite(finding?.["site"]), {
      tone: "informative",
      tag: `${str(entry?.["class"]) ?? "informative"}: ${tag}`,
      detail
    })
  }
  const abandoned = record(read["abandoned"])
  if (abandoned !== undefined) {
    add(str(abandoned["step"]), {
      tone: "abandoned",
      tag: "script ended here",
      detail: `leg ${str(abandoned["leg"]) ?? "?"} could not go on`
    })
    for (const pending of list(abandoned["pending"])) {
      add(str(pending), { tone: "abandoned", tag: "not run", detail: "the script ended before this step" })
    }
  }
  // The step the run DIED at, said even where no finding names it: a run that
  // stopped without raising must still show where it stopped.
  const failedStep = str(read["failed_step"])
  if (failedStep !== undefined && !found.has(failedStep)) {
    add(failedStep, { tone: "failure", tag: "died here", detail: "the replay stopped at this step" })
  }
  return withBody(found, confrontation)
}

/** The rulings with a `bad-body` failure added on the step of each failing body row. */
const withBody = (
  found: Map<string, Finding[]>,
  confrontation: readonly unknown[]
): ReadonlyMap<string, readonly Finding[]> => {
  for (const entry of confrontation) {
    const row = record(entry)
    const step = str(row?.["step"])
    if (row === undefined || step === undefined || step === "" || !isBadBody(row)) continue
    const finding: Finding = { tone: "failure", tag: "bad-body", detail: bodyDetail(row) }
    const at = found.get(step)
    if (at === undefined) found.set(step, [finding])
    else at.push(finding)
  }
  return found
}

/** The loudest finding on a step, as the ladder draws it. */
const markOf = (findings: readonly Finding[]): LadderMark | undefined => {
  const ranked = [...findings].sort((a, b) => WEIGHT[b.tone] - WEIGHT[a.tone])
  const first = ranked[0]
  if (first === undefined) return undefined
  const rest = ranked.length - 1
  return {
    tone: first.tone,
    text: `${first.tone === "failure" ? "✗ " : ""}${first.tag}${rest > 0 ? ` +${rest}` : ""}`,
    title: ranked.map((finding) => `${finding.tag}${finding.detail === "" ? "" : ` — ${finding.detail}`}`)
      .join("\n")
  }
}

/**
 * The findings placed on ONE ladder: a step the ladder does not draw is
 * silently dropped, because a mark with no row is a mark on the wrong row.
 */
export const marksOnLadder = (
  findings: ReadonlyMap<string, readonly Finding[]>,
  index: StepIndex
): Readonly<Record<string, LadderMark>> => {
  const marks: Record<string, LadderMark> = {}
  for (const [step, at] of findings) {
    const eventId = index.get(step)
    const mark = markOf(at)
    if (eventId !== undefined && mark !== undefined) marks[eventId] = mark
  }
  return marks
}
