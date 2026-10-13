/**
 * The main error of one cell: which of everything a failing cell wrote a
 * reader leads with. Pure — the bundle reader hands it what it read, and this
 * decides nothing about disk or SQL.
 *
 * The precedence is the one {@link MainError} states: the interpreter's
 * refusal, else the charged audit finding on the earliest offending message,
 * else a scripted party's harness fault, else the step the run died waiting
 * at, else the first failure under its own tag, else the earliest body row no
 * rule accepts. Where the body gate is
 * what refused the cell (every verdict failure a named rule accepts), that row
 * leads right after the refusal and the charges. A cell that passed or was
 * skipped has no main error. A failing cell with nothing to read is said to
 * have none rather than left blank, so a row never shows a failure with no
 * name.
 */
import * as Schema from "effect/Schema"
import * as Bundle from "./bundle/index.js"
import * as Records from "./confrontation.js"

export const MainErrorKind = Schema.Literals(["rfc", "harness-fault", "died", "failure", "bad-body", "refused"])
export type MainErrorKind = typeof MainErrorKind.Type

/**
 * The ONE thing a failing cell is said to have failed on, so a row leads with
 * it and a chip can count it. Precedence, for a cell that did not pass and was
 * not skipped: `refused` (the interpreter refused the cell — `rule`
 * `interpreter-refused`, `detail` its first line), else `rfc` (the charged
 * audit finding on the earliest offending message — `rule` the rule id, `rfc`
 * its citation), else `harness-fault` (a scripted party's violation the
 * document does not state, on the earliest offending message — `rule` and
 * `rfc` as for `rfc`, `detail` led by the party), else `died` (the first
 * `unmatched-datagram` /
 * `expect-timed-out` — `step` where, `expected` the message it waited for),
 * else `failure` (the first failure, `rule` its tag), else `bad-body` (the first
 * body row no rule accepts, the session description's included — `rule`
 * `bad-body`, `step` the row's, `detail` its signature and the first differing
 * captured → replayed line). A cut's own refusal rides the same shape: `rule`
 * is the first refusal reason.
 */
export const MainError = Schema.Struct({
  kind: MainErrorKind,
  rule: Schema.NullOr(Schema.String),
  /** The RFC the rule cites, `RFC 3581 §4`; null where the detail cites none. */
  rfc: Schema.NullOr(Schema.String),
  step: Schema.NullOr(Schema.String),
  expected: Schema.NullOr(Schema.String),
  detail: Schema.String
})
export interface MainError extends Schema.Schema.Type<typeof MainError> {}

export interface Input {
  readonly passed: boolean
  readonly skipped: string | null
  /** The campaign cell's own `error`, set where the cell crashed or was refused. */
  readonly error?: string | undefined
  readonly verdict?: Bundle.RunVerdict | null | undefined
  /** The gating findings that STAND against the SUT after the inheritance ruling. */
  readonly charged: ReadonlyArray<Bundle.RfcFinding>
  /** The audit's harness faults: scripted parties' violations the document does not state. */
  readonly harness?: ReadonlyArray<Bundle.RfcFinding> | undefined
  /** The cell's confrontation classification counts, where it wrote one. */
  readonly confrontation?: Confrontation | undefined
  /** The body rows no rule accepts, earliest step first ({@link failingBody}). */
  readonly body?: ReadonlyArray<Records.ConfrontationRecord> | undefined
  /** The body gate refused the cell: without its rows, the bless would have held (`rule-hits.json`). */
  readonly bodyGated?: boolean | undefined
}

/** The two confrontation classes that refuse a cell whose verdict is clean. */
export interface Confrontation {
  readonly unknown: number
  readonly unlisted: number
}

/** The tag a cell the interpreter refused or crashed on carries as its rule. */
export const INTERPRETER_REFUSED = "interpreter-refused"

/** The tag a body delta carries as its rule and its kind. */
export const BAD_BODY = "bad-body"

/**
 * The body rows no lane rule answers for, in record order: every `body:` row,
 * the session description's `body:sdp:` rows included, whatever its media type.
 */
export const failingBody = (
  records: ReadonlyArray<Records.ConfrontationRecord>
): ReadonlyArray<Records.ConfrontationRecord> => records.filter((r) => r.kind === "body" && !Records.recordPasses(r))

/** `RFC 3261`, or `RFC 3261 §17.2.1`; a sentence's full stop is not part of the section. */
const CITATION = /RFC \d+(?: §\d+(?:\.\d+)*)?/

/** The first RFC a finding's detail cites, section included; null where it cites none. */
export const citation = (detail: string): string | null => CITATION.exec(detail)?.[0] ?? null

/** A gate as the run spells it: the method, `<status> to <method>`, or the bare status. */
export const gatedOnText = (gatedOn: Bundle.GatedOn | string): string => {
  if (typeof gatedOn === "string") return gatedOn
  if (gatedOn.kind === "request") return gatedOn.method
  return gatedOn.cseq_method === undefined ? String(gatedOn.status) : `${gatedOn.status} to ${gatedOn.cseq_method}`
}

const firstLine = (text: string): string => text.split("\n")[0] ?? text

const none = (kind: MainError["kind"]): MainError => ({
  kind,
  rule: null,
  rfc: null,
  step: null,
  expected: null,
  detail: ""
})

const isDeath = (failure: Bundle.Failure): failure is Extract<
  Bundle.Failure,
  { failure: "unmatched-datagram" | "expect-timed-out" }
> => failure.failure === "unmatched-datagram" || failure.failure === "expect-timed-out"

/** The earliest-offending charge; an unplaced one sorts after every placed one. */
const leading = (charged: ReadonlyArray<Bundle.RfcFinding>): Bundle.RfcFinding | undefined =>
  [...charged].sort((a, b) =>
    (a.offending ?? Number.MAX_SAFE_INTEGER) - (b.offending ?? Number.MAX_SAFE_INTEGER)
  )[0]

/** A failure's fields on one line: its `detail` or `reason` where it has one, else every field it carries. */
const render = (failure: Bundle.Failure): string => {
  const fields = failure as unknown as Record<string, unknown>
  for (const key of ["detail", "reason"]) {
    if (typeof fields[key] === "string") return firstLine(fields[key] as string)
  }
  return Object.entries(fields)
    .filter(([key]) => key !== "failure")
    .map(([key, value]) => `${key}=${typeof value === "string" ? value : JSON.stringify(value)}`)
    .join(" ")
    .replace(/\n/g, " ")
}

/**
 * A death names its step; the gate and the reason are read only where the
 * verdict states them, because a verdict the contract could not decode is
 * still a death at a step (`bundle.ts`).
 */
const died = (failure: Extract<Bundle.Failure, { failure: "unmatched-datagram" | "expect-timed-out" }>): MainError => {
  const gate = failure.gated_on as Bundle.GatedOn | string | undefined
  const detail = failure.failure === "unmatched-datagram"
    ? (typeof failure.reason === "string" ? firstLine(failure.reason) : "")
    : (typeof failure.within_ms === "number" ? `nothing matched within ${failure.within_ms} ms` : "")
  return {
    ...none("died"),
    step: failure.step,
    expected: gate === undefined ? null : gatedOnText(gate),
    detail
  }
}

/** A side's values as lines: a whole-document value is split on its line endings, blank lines dropped. */
const linesOf = (values: ReadonlyArray<string>): ReadonlyArray<string> =>
  values.flatMap((value) => value.split(/\r?\n/)).filter((line) => line !== "")

const ending = (text: string): string => (text.includes("\r\n") ? "CRLF" : text.includes("\n") ? "LF" : "none")

/** What two texts with the same lines differ by: their line endings, their blank lines, or other bytes. */
const bytesDelta = (captured: string, replayed: string): string => {
  const a = ending(captured)
  const b = ending(replayed)
  if (a !== b) return `line endings differ (${a} → ${b})`
  const blanks = (text: string): number => text.split(/\r?\n/).filter((line) => line === "").length
  if (blanks(captured) !== blanks(replayed)) return "blank lines differ"
  return "bytes differ"
}

/**
 * A body row: its step, and its signature with the first line where the two
 * sides differ, `(none)` for a side that ran out of lines first; where every
 * line is the same, what the bytes differ by.
 */
const badBody = (row: Records.ConfrontationRecord): MainError => {
  const captured = linesOf(row.captured)
  const replayed = linesOf(row.replayed)
  let at = 0
  while (at < captured.length && at < replayed.length && captured[at] === replayed[at]) at++
  const said = captured[at] === undefined && replayed[at] === undefined
    ? bytesDelta(row.captured.join(""), row.replayed.join(""))
    : `${captured[at] ?? "(none)"} → ${replayed[at] ?? "(none)"}`
  return {
    ...none(BAD_BODY),
    rule: BAD_BODY,
    step: row.step === "" ? null : row.step,
    detail: `${row.signature}: ${said}`
  }
}

/** What one cell failed on, or null where it did not fail. */
export const mainErrorOf = (input: Input): MainError | null => {
  if (input.skipped !== null || input.passed) return null
  if (input.error !== undefined) {
    return { ...none("refused"), rule: INTERPRETER_REFUSED, detail: firstLine(input.error) }
  }
  const charge = leading(input.charged)
  if (charge !== undefined) {
    return { ...none("rfc"), rule: charge.rule, rfc: citation(charge.detail), detail: charge.detail }
  }
  const fault = leading(input.harness ?? [])
  if (fault !== undefined) {
    return {
      ...none("harness-fault"),
      rule: fault.rule,
      rfc: citation(fault.detail),
      detail: `${fault.actor ?? "a scripted party"}: ${fault.detail}`
    }
  }
  const body = input.body?.[0]
  if (input.bodyGated === true && body !== undefined) return badBody(body)
  const verdict = input.verdict ?? undefined
  if (verdict === undefined) {
    return { ...none("failure"), rule: "no-verdict", detail: "the cell wrote no verdict" }
  }
  const failures = verdict.failures ?? []
  const death = failures.find(isDeath)
  if (death !== undefined) return died(death)
  const first = failures[0]
  if (first === undefined && body !== undefined) return badBody(body)
  // A clean verdict the reclassifier still refused: the confrontation records
  // are what refused it, so the detail names them.
  if (first === undefined) {
    const records = input.confrontation
    return {
      ...none("failure"),
      rule: "reclassified",
      detail: records === undefined
        ? `verdict ${verdict.status} with no failure named and no classification`
        : `confrontation: ${records.unknown} unknown, ${records.unlisted} unlisted`
    }
  }
  return {
    ...none("failure"),
    rule: first.failure,
    step: Bundle.failureStep(first) ?? null,
    detail: render(first)
  }
}
