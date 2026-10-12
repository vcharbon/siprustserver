/**
 * The COMPARE REPORT, `sip.compare/1`: two run sets compared as one JSON
 * object per call, jq-first. This package owns the format; the JSON Schema
 * published beside it (`schemas/compare-report.schema.json`) is generated from
 * {@link CompareReport} and checked against it by the package's tests.
 *
 *   { schema_version, headline, code, compared, unexplained,
 *     before, after:  { selector, runs, cells, lanes[], envs[], branches[],
 *                       components[{ name, shas[] }], laneHashes[],
 *                       sweeps[{ id, universe, selection, captures }] },
 *     counts:         { "<class>": { sameDocument, documentChanged } },
 *     onlyBefore, onlyAfter: { captures, count, calls[] },
 *     calls[]: { alias, capture, caseId, class, document, code, lane,
 *                before, after: { run, lane, env, branch, code[{ name, sha }],
 *                                 verdict, passed, repeats { runs, passed },
 *                                 skipped, failedStep,
 *                                 mainError, bless, accepted, knownBugs[] | null,
 *                                 ladder[] | null, links { study, text } },
 *                ref: ladder[] | null } }
 *
 * Only the calls BOTH sides ran are compared; `onlyBefore`/`onlyAfter` count
 * the captures the other side never ran and, within the captures both ran, the
 * calls one side alone holds (`capture/case`, the first few). `class` is one of
 * {@link MOVE_CLASSES}; `calls` holds the calls that moved, or every compared
 * call where the producer was asked for all. A ladder is the text renderer's
 * own lines; null where the bundle was reaped, the store holds no document, or
 * the call is past the producer's ladder budget. `knownBugs` is null where the
 * bundle is gone. `code`, `document` and `lane` say whether the two cells ran
 * the same code (tree fingerprints, component by component), the same case
 * document and the same lane behaviour; a verdict flip with all three the same
 * is `unexplained`: nothing but the harness explains it. A call takes the
 * first class of {@link MOVE_CLASSES} order that applies, `unstable` coming
 * last but `unchanged`: a call whose verdict, failure and bless class held but
 * whose repeats within one side (runs on the same known code, document and
 * lane) disagree. It counts as unexplained too.
 */
import * as Schema from "effect/Schema"
import { MainError } from "./main-error.js"

/** The version every document of this format carries; a change of shape is a new version. */
export const SCHEMA_VERSION = "sip.compare/1"

/** Whether two cells ran the same code; `unknown` wherever a fingerprint is missing. */
export const CodeReading = Schema.Literals(["same", "different", "unknown"])
export type CodeReading = typeof CodeReading.Type

/** Whether two cells ran on one lane behaviour; `unknown` where either lane hash is. */
export const LaneReading = Schema.Literals(["same", "different", "unknown"])
export type LaneReading = typeof LaneReading.Type

/** Whether two cells ran one case document. */
export const DocumentReading = Schema.Literals(["same", "changed", "unknown"])
export type DocumentReading = typeof DocumentReading.Type

/** What a cell's confrontation records needed; `unmeasured` where none was recorded. */
export const BlessClass = Schema.Literals(["clean", "known-bug", "unlisted", "unknown", "unmeasured"])
export type BlessClass = typeof BlessClass.Type

/** The classes a compared call takes, in the order a report lists them. */
export const MOVE_CLASSES = [
  "green→red",
  "red→green",
  "red→red-moved",
  "ran→skipped",
  "skipped→ran",
  "clean→known-bug",
  "known-bug→clean",
  "unstable",
  "unchanged"
] as const

export const MoveClass = Schema.Literals(MOVE_CLASSES)
export type MoveClass = typeof MoveClass.Type

/** A sweep a side's runs belong to, with the population it stated. */
export const SideSweep = Schema.Struct({
  id: Schema.Int,
  universe: Schema.String,
  /** The predicates the sweep was opened with; null for a sweep recorded before they were. */
  selection: Schema.NullOr(Schema.String),
  captures: Schema.Int
})
export interface SideSweep extends Schema.Schema.Type<typeof SideSweep> {}

/** The commits one component of a side's code was on, null where a run recorded none. */
export const ComponentShas = Schema.Struct({
  name: Schema.String,
  shas: Schema.Array(Schema.NullOr(Schema.String))
})
export interface ComponentShas extends Schema.Schema.Type<typeof ComponentShas> {}

export const Side = Schema.Struct({
  selector: Schema.String,
  runs: Schema.Int,
  cells: Schema.Int,
  lanes: Schema.Array(Schema.String),
  envs: Schema.Array(Schema.NullOr(Schema.String)),
  branches: Schema.Array(Schema.NullOr(Schema.String)),
  components: Schema.Array(ComponentShas),
  laneHashes: Schema.Array(Schema.NullOr(Schema.String)),
  sweeps: Schema.Array(SideSweep)
})
export interface Side extends Schema.Schema.Type<typeof Side> {}

/** Where a viewer shows one call of one run, and its text export. */
export const Links = Schema.Struct({
  study: Schema.String,
  text: Schema.String
})
export interface Links extends Schema.Schema.Type<typeof Links> {}

/** A known-bug rule a cell's confrontation cited: the rule, the ticket it waits on, and how many records. */
export const KnownBug = Schema.Struct({
  rule: Schema.String,
  ticket: Schema.NullOr(Schema.String),
  records: Schema.Int
})
export interface KnownBug extends Schema.Schema.Type<typeof KnownBug> {}

/** One component's commit as the cell's run recorded it. */
export const CellComponent = Schema.Struct({
  name: Schema.String,
  sha: Schema.NullOr(Schema.String)
})
export interface CellComponent extends Schema.Schema.Type<typeof CellComponent> {}

/** A side's runs of one call on the cell's code, document and lane, the cell's own included, and how many passed. */
export const Repeats = Schema.Struct({
  runs: Schema.Int,
  passed: Schema.Int
})
export interface Repeats extends Schema.Schema.Type<typeof Repeats> {}

export const Cell = Schema.Struct({
  run: Schema.Int,
  lane: Schema.String,
  env: Schema.NullOr(Schema.String),
  branch: Schema.NullOr(Schema.String),
  code: Schema.Array(CellComponent),
  verdict: Schema.String,
  passed: Schema.Boolean,
  repeats: Repeats,
  skipped: Schema.NullOr(Schema.String),
  failedStep: Schema.NullOr(Schema.String),
  mainError: Schema.NullOr(MainError),
  bless: BlessClass,
  accepted: Schema.NullOr(Schema.Int),
  knownBugs: Schema.NullOr(Schema.Array(KnownBug)),
  ladder: Schema.NullOr(Schema.Array(Schema.String)),
  links: Links
})
export interface Cell extends Schema.Schema.Type<typeof Cell> {}

export const Call = Schema.Struct({
  /** The id the call is cited by. */
  alias: Schema.String,
  capture: Schema.String,
  caseId: Schema.String,
  class: MoveClass,
  document: DocumentReading,
  code: CodeReading,
  lane: LaneReading,
  before: Cell,
  after: Cell,
  ref: Schema.NullOr(Schema.Array(Schema.String))
})
export interface Call extends Schema.Schema.Type<typeof Call> {}

/** A class's count, split by whether the document moved under it. */
export const ClassCount = Schema.Struct({
  sameDocument: Schema.Int,
  documentChanged: Schema.Int
})
export interface ClassCount extends Schema.Schema.Type<typeof ClassCount> {}

/** What one side alone holds: captures the other never ran, and calls within the captures both ran. */
export const Only = Schema.Struct({
  captures: Schema.Int,
  count: Schema.Int,
  calls: Schema.Array(Schema.String)
})
export interface Only extends Schema.Schema.Type<typeof Only> {}

export const CompareReport = Schema.Struct({
  schema_version: Schema.Literal(SCHEMA_VERSION),
  headline: Schema.String,
  code: CodeReading,
  compared: Schema.Int,
  unexplained: Schema.Int,
  before: Side,
  after: Side,
  counts: Schema.Record(MoveClass, ClassCount),
  onlyBefore: Only,
  onlyAfter: Only,
  calls: Schema.Array(Call)
})
export interface CompareReport extends Schema.Schema.Type<typeof CompareReport> {}

/** The published JSON Schema of {@link CompareReport} (draft 2020-12). */
export const jsonSchema = (): Record<string, unknown> => {
  const document = Schema.toJsonSchemaDocument(CompareReport)
  return {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    title: "Compare report",
    ...document.schema,
    ...(Object.keys(document.definitions).length === 0 ? {} : { $defs: document.definitions })
  }
}

/** A document of this format, or a `SchemaError` saying where it is not one. */
export const decode = Schema.decodeUnknownSync(CompareReport)
