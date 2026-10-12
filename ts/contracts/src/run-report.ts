/**
 * The RUN REPORT, `sip.run-report/1`: one run set's calls, each as the
 * compare report's own cell (`compare-report.ts`), so a call reads the same in
 * a report of one run and in a comparison of two. The JSON Schema published
 * beside it (`schemas/run-report.schema.json`) is generated from
 * {@link RunReport} and checked against it by the package's tests.
 *
 *   { schema_version, selector, source, headline,
 *     runs[]: { id, capture, lane, setKind, status, env, branch, code[{ name, sha }],
 *               sweepId, jobId, startedAt, finishedAt, cases, failed, citedBy[] },
 *     total,
 *     side:   the compare report's side,
 *     counts: { calls, passed, failed, skipped },
 *     calls[]: { capture, caseId, cell } }
 *
 * `source` says what the calls were read off: `runs`, the store's run rows and
 * their results; `summaries`, the summary a citation kept of a run whose row
 * is gone — its cells hold verdicts, steps and main errors, never a bless
 * class, an accepted count or a ladder. `runs` lists the newest runs the set
 * holds, `total` how many it holds; `citedBy` names the records citing a run,
 * empty where none does or the source does not say.
 */
import * as Schema from "effect/Schema"
import { Cell, CellComponent, Side } from "./compare-report.js"

/** The version every document of this format carries; a change of shape is a new version. */
export const SCHEMA_VERSION = "sip.run-report/1"

/** What the calls were read off. */
export const Source = Schema.Literals(["runs", "summaries"])
export type Source = typeof Source.Type

/** One run of the set, as its row or its kept summary states it. */
export const Run = Schema.Struct({
  id: Schema.Int,
  capture: Schema.String,
  lane: Schema.String,
  setKind: Schema.String,
  status: Schema.String,
  env: Schema.NullOr(Schema.String),
  branch: Schema.NullOr(Schema.String),
  code: Schema.Array(CellComponent),
  sweepId: Schema.NullOr(Schema.Int),
  jobId: Schema.NullOr(Schema.Int),
  startedAt: Schema.String,
  finishedAt: Schema.NullOr(Schema.String),
  cases: Schema.Int,
  failed: Schema.Int,
  citedBy: Schema.Array(Schema.String)
})
export interface Run extends Schema.Schema.Type<typeof Run> {}

/** How the set's calls ended. */
export const Counts = Schema.Struct({
  calls: Schema.Int,
  passed: Schema.Int,
  failed: Schema.Int,
  skipped: Schema.Int
})
export interface Counts extends Schema.Schema.Type<typeof Counts> {}

/** One call of the set, at the set's latest run holding it. */
export const Call = Schema.Struct({
  capture: Schema.String,
  caseId: Schema.String,
  cell: Cell
})
export interface Call extends Schema.Schema.Type<typeof Call> {}

export const RunReport = Schema.Struct({
  schema_version: Schema.Literal(SCHEMA_VERSION),
  selector: Schema.String,
  source: Source,
  headline: Schema.String,
  runs: Schema.Array(Run),
  total: Schema.Int,
  side: Side,
  counts: Counts,
  calls: Schema.Array(Call)
})
export interface RunReport extends Schema.Schema.Type<typeof RunReport> {}

/** The published JSON Schema of {@link RunReport} (draft 2020-12). */
export const jsonSchema = (): Record<string, unknown> => {
  const document = Schema.toJsonSchemaDocument(RunReport)
  return {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    title: "Run report",
    ...document.schema,
    ...(Object.keys(document.definitions).length === 0 ? {} : { $defs: document.definitions })
  }
}

/** A document of this format, or a `SchemaError` saying where it is not one. */
export const decode = Schema.decodeUnknownSync(RunReport)
