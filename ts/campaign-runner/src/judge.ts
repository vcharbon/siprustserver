/**
 * What one cell came to on a lane, from the replay tool's exit code and the
 * bundle it wrote: `error` when no call was judged (refused, environment,
 * panicked: exit 2–4), `failure` when the verdict failed (a check, or a
 * gate it charges, the cleanup among them) or the RFC audit gated, else
 * `pass`. The message names each failed check on a line of its own, read off
 * `verdict.json` and `rfc.json`; an error's is the tool's own summary. A
 * failure's checks are typed by the pack's {@link Typer} off the bundle and
 * the compiled case; the lines the lane answered in process off
 * `verifications.json` where the cell wrote it, and the audit's line off
 * `rfc.json` (`check-lines.ts` `auditLineOf`). A hosted-lane cell
 * whose batch's end did not run ({@link unjudgedAtEnd}) is an error whatever
 * its verdict says. An error that wrote no bundle (a cell not run) states
 * none, so no collector reads one. What a pack's collectors answer of a cell is joined by
 * the batch (`replay.ts`). A cell claiming
 * of a collector no part of the lane answers is skipped where it would pass,
 * naming the claims not requested (`claims.ts`).
 */
import { answeredOf, VERIFICATIONS_FILE } from "@sip/store/cell-lines"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { type Cell, type CellResult, errored, type Lane, skippedOf } from "./cell.js"
import * as CheckLines from "./check-lines.js"
import { notRequestedOf } from "./claims.js"
import type { Check } from "@sip/store/campaign-results"

const Verdict = Schema.Struct({
  status: Schema.String,
  failed_step: Schema.optionalKey(Schema.NullOr(Schema.String)),
  failures: Schema.optionalKey(Schema.Array(Schema.Record(Schema.String, Schema.Unknown)))
})

const Finding = Schema.Struct({ rule: Schema.String, detail: Schema.String, gating: Schema.Boolean })

/** What a failed cell's typer reads: the verdict's failures, the RFC audit's findings, the cell's bundle and the cell. */
export interface Failed {
  /** `verdict.json`'s `failures`. */
  readonly failures: ReadonlyArray<Readonly<Record<string, unknown>>>
  /** `rfc.json`'s `findings`. */
  readonly rfc: ReadonlyArray<{ readonly rule: string; readonly detail: string; readonly gating: boolean }>
  /** The cell's bundle directory. */
  readonly bundle: string
  readonly cell: Cell
}

/**
 * The pack's typer of a failed cell's checks: one check per failure of the
 * verdict and per gating RFC finding (and any the pack adds), each anchored
 * and typed to a side off what the bundle and the compiled case hold.
 */
export type Typer = (failed: Failed) => Effect.Effect<ReadonlyArray<Check>, never, FileSystem.FileSystem>

const Rfc = Schema.Struct({
  status: Schema.String,
  reason: Schema.optionalKey(Schema.String),
  findings: Schema.optionalKey(Schema.Array(Finding))
})

/** One failure of `verdict.json` on one line: its kind, then its fields. */
export const failureLine = (failure: Readonly<Record<string, unknown>>): string => {
  const { failure: kind, ...rest } = failure
  if (kind === "check-failed") {
    return `${rest.site}: ${rest.field} ${rest.op} ${JSON.stringify(rest.expected)}, observed ${JSON.stringify(rest.observed)}`
  }
  const fields = Object.entries(rest).map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v)}`)
  return `${String(kind)}: ${fields.join(" ")}`
}

/** The JSON at `file` decoded by `schema`, none where it does not read or decode. */
export const readJson = <S extends Schema.Top>(file: string, schema: S) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const text = yield* fs.readFileString(file)
    return yield* Schema.decodeUnknownEffect(Schema.fromJsonString(schema))(text)
  }).pipe(Effect.option)

/** The JSON at `file`, or `fallback` where it does not read. */
export const readAny = (file: string, fallback: unknown) =>
  readJson(file, Schema.Unknown).pipe(Effect.map((read) => (read._tag === "Some" ? read.value : fallback)))

/** The cell's result on `lane` from the tool's `exit` code, its `summary` line and its `bundle`, a failure's checks typed by `typer`. */
export const judge = <C extends Cell>(typer: Typer, lane: Lane, cell: C, exit: number, summary: string, bundle: string) =>
  Effect.gen(function*() {
    const base = { ...errored(lane, cell, ""), bundle }
    if (exit >= 2) {
      const wrote = yield* Effect.flatMap(FileSystem.FileSystem, (fs) => fs.exists(bundle)).pipe(Effect.orElseSucceed(() => true))
      const own = wrote ? base : errored(lane, cell, "")
      return { ...own, message: summary.trim() } satisfies CellResult<C>
    }
    const verdict = yield* readJson(path.join(bundle, "verdict.json"), Verdict)
    const rfc = yield* readJson(path.join(bundle, "rfc.json"), Rfc)
    if (verdict._tag === "None") {
      return { ...base, message: `no verdict.json in ${bundle}: ${summary.trim()}` } satisfies CellResult<C>
    }
    const failures = verdict.value.failures ?? []
    const findings = rfc._tag === "Some" ? rfc.value.findings ?? [] : []
    const lines = failures.map(failureLine)
    for (const f of findings) if (f.gating) lines.push(`rfc ${f.rule}: ${f.detail}`)
    const failed = verdict.value.status === "failed" || lines.length > 0 || exit === 1
    if (failed && lines.length === 0) lines.push(summary.trim())
    const checks = failed ? [...yield* typer({ failures, rfc: findings, bundle, cell })] : []
    const answered = answeredOf(yield* readAny(path.join(bundle, VERIFICATIONS_FILE), null))
    const audit = CheckLines.auditLineOf(
      rfc._tag === "Some"
        ? { status: rfc.value.status, reason: rfc.value.reason, gating: findings.filter((f) => f.gating).length }
        : undefined
    )
    const result = {
      ...base,
      ...(answered === null ? {} : { answered }),
      ...(audit === undefined ? {} : { audit }),
      verdict: verdict.value.status,
      failedStep: verdict.value.failed_step ?? null,
      rfcGating: rfc._tag === "Some" ? findings.filter((f) => f.gating).length : null,
      checks,
      status: failed ? "failure" : "pass",
      message: lines.join("\n")
    } satisfies CellResult<C>
    const notRequested = failed ? undefined : notRequestedOf(lane, cell)
    return notRequested === undefined ? result : skippedOf(result, notRequested)
  })

/**
 * `result` of a cell whose batch never wrote its end line (its bound, its
 * death): the gates and checks the batch's end runs never judged its calls,
 * so a judged cell is an error naming why; one already an error keeps its own.
 */
export const unjudgedAtEnd = <C extends Cell>(result: CellResult<C>, why: string): CellResult<C> =>
  result.status === "error"
    ? result
    : { ...result, status: "error", message: `not judged: the batch's end did not run: ${why}`, checks: [] }
