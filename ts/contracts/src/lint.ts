/**
 * The pivot lint report (`pivot-schema lint --json`), mirroring
 * `pivot_schema::lint`'s `Report` / `Diagnostic` / `Severity`.
 *
 * Hand-mirrored: those three types derive `Serialize` alone upstream — no
 * `JsonSchema`, so `pivot-schema schema` publishes no contract to check this
 * against, and the struct declarations are the only source of truth.
 *
 * Every diagnostic is copy for whoever has to fix the document: a stable rule
 * id, a location keyed by the human-facing id (`flow[id:s7]`), what is wrong, and
 * what to change.
 */
import * as Schema from "effect/Schema"
import { STRICT } from "./strict.js"

/** How much a finding costs. */
export const Severity = Schema.Literals(["error", "warning"])
export type Severity = typeof Severity.Type

/** One finding. */
export const Diagnostic = Schema.Struct({
  /** Stable rule id, `group/rule`. */
  rule: Schema.String,
  severity: Severity,
  /** Where, keyed by the human-facing id. */
  path: Schema.String,
  message: Schema.String,
  hint: Schema.String
})
export interface Diagnostic extends Schema.Schema.Type<typeof Diagnostic> {}

/** Everything lint found in one document. */
export const Report = Schema.Struct({
  diagnostics: Schema.Array(Diagnostic)
})
export interface Report extends Schema.Schema.Type<typeof Report> {}

export const decodeLintReport = Schema.decodeUnknownEffect(Report, STRICT)
export const decodeLintReportSync = Schema.decodeUnknownSync(Report, STRICT)

/** Whether anything blocks a replay. */
export const reportHasErrors = (report: Report): boolean =>
  report.diagnostics.some((d) => d.severity === "error")

/** The rule ids present, order-independent — what a test asserts on. */
export const reportRules = (report: Report): Array<string> =>
  [...new Set(report.diagnostics.map((d) => d.rule))].sort()
