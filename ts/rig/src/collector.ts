/**
 * The collector manifest, `collectors/<name>/collector.toml` (`collector.v1`,
 * ADR-0060 appendix §5): what a collector reads the lab with, at which scope,
 * on the lanes that have the parts it needs, and the executables it runs.
 */
import * as Schema from "effect/Schema"
import { Name, Params, parseWith, RelativePath, Seconds, Text } from "./manifest-file.js"

/** The schema version a collector manifest states. */
export const SCHEMA = "collector.v1"

/** The manifest's file name in its directory. */
export const FILE = "collector.toml"

/** The scope a collector runs once per: the job, a batch, a cell. */
export const SCOPES = ["job", "batch", "cell"] as const
export type Scope = (typeof SCOPES)[number]

/** When a collector runs on a lane that wires it, unless a level or the lane says otherwise. */
export const DEFAULTS = ["always", "on-failure", "on-demand"] as const
export type Default = (typeof DEFAULTS)[number]

const Timeouts = Schema.Struct({
  open_s: Schema.optionalKey(Seconds),
  collect_s: Schema.optionalKey(Seconds),
  close_s: Schema.optionalKey(Seconds)
})

const Run = Schema.Struct({
  open: Schema.optionalKey(RelativePath),
  collect: RelativePath,
  close: Schema.optionalKey(RelativePath)
})

export const Collector = Schema.Struct({
  schema: Schema.Literal(SCHEMA),
  /** Its directory's name. */
  name: Name,
  scope: Schema.Literals(SCOPES),
  /** `before`: a pre-check, answered before any call of its scope; `after` (default): it reads what the scope did. */
  at: Schema.optionalKey(Schema.Literals(["before", "after"])),
  /** The lane parts it needs: it is wired on every lane that has them all. */
  needs: Schema.optionalKey(Schema.Array(Name)),
  /** Default `always`. */
  default: Schema.optionalKey(Schema.Literals(DEFAULTS)),
  /** No level and no lane switches it off. */
  required: Schema.optionalKey(Schema.Boolean),
  /** Collectors of its scope whose keys and records it reads; one not wired on the lane orders nothing. */
  after: Schema.optionalKey(Schema.Array(Name)),
  /** The verifications it, or its judge, answers. */
  answers: Schema.optionalKey(Schema.Array(Name)),
  /** Its lines never decide: each it answers, and each `not checkable` the base answers for it, is informative. */
  informative: Schema.optionalKey(Schema.Boolean),
  /** It answers its scope's own lines alone, never a cell's: each `not checkable` the base answers for it is its scope's alone. */
  scope_only: Schema.optionalKey(Schema.Boolean),
  /** The judge of the path that decides over its records. */
  judge: Schema.optionalKey(Name),
  /** Box-wide locks held from `open` to `close`; none on a cell-scope collector. */
  locks: Schema.optionalKey(Schema.Array(Name)),
  /** Cell scope: wall-clock seconds after the cell ended before it runs. */
  delay_s: Schema.optionalKey(Seconds),
  /** Cell scope: the cell's own locks are kept until it answered. */
  holds_cell_locks: Schema.optionalKey(Schema.Boolean),
  /** What a lane file may override, each by its name. */
  params: Schema.optionalKey(Params),
  /** The record field the base joins its records to cells on; absent, it assigns them itself. */
  join: Schema.optionalKey(Schema.Struct({ key: Text })),
  timeouts: Schema.optionalKey(Timeouts),
  run: Run
})
export interface Collector extends Schema.Schema.Type<typeof Collector> {}

/** Each timeout of a collector, in seconds. */
export interface Bounds {
  readonly open_s: number
  readonly collect_s: number
  readonly close_s: number
}

/** The timeouts a collector takes where its manifest states none. */
export const DEFAULT_BOUNDS: Bounds = { open_s: 30, collect_s: 300, close_s: 30 }

/** The timeouts of `collector`: its manifest's, each default where it states none. */
export const boundsOf = (collector: Collector): Bounds => ({ ...DEFAULT_BOUNDS, ...collector.timeouts })

/**
 * The verifications a failed `open` or `collect` of `collector` answers `not
 * checkable`: those it names, or its own name where it names none, so its
 * cells never read as verified by a collector that did not run.
 */
export const answeredBy = (collector: Collector): ReadonlyArray<string> =>
  collector.answers === undefined || collector.answers.length === 0 ? [collector.name] : collector.answers

/** Whether `collector`'s lines never decide a verdict (`informative = true`). */
export const isInformative = (collector: Collector): boolean => collector.informative === true

/** Whether `collector` answers its scope's own lines alone, never a cell's (`scope_only = true`). */
export const isScopeOnly = (collector: Collector): boolean => collector.scope_only === true

/** The executables a collector names, by the step that runs each. */
export const executablesOf = (collector: Collector): ReadonlyArray<readonly [string, string]> =>
  (["open", "collect", "close"] as const).flatMap((step) => {
    const file = collector.run[step]
    return file === undefined ? [] : [[step, file] as const]
  })

/** Whether no level and no lane may switch it off. */
export const isRequired = (collector: Collector): boolean => collector.required === true

/** The rules a shape cannot state. */
const meaningOf = (collector: Collector): string | undefined => {
  if (collector.scope !== "cell" && collector.delay_s !== undefined) return "delay_s: only a cell-scope collector is delayed"
  if (collector.scope !== "cell" && collector.holds_cell_locks !== undefined) {
    return "holds_cell_locks: only a cell-scope collector holds a cell's locks"
  }
  if (collector.scope === "cell" && (collector.locks ?? []).length > 0) {
    return "locks: a cell-scope collector takes no box-wide lock, which it would hold over the lane's next cells"
  }
  if (collector.at === "before" && collector.scope === "cell") return "at: a pre-check is of job or batch scope"
  if (collector.at === "before" && collector.run.open !== undefined) return "run.open: a pre-check has no open"
  if (collector.at === "before" && collector.judge !== undefined) return "judge: a pre-check's lines are its own, no judge decides them"
  if (collector.at === "before" && collector.join !== undefined) return "join: a pre-check runs over no cell, it joins nothing"
  if (collector.at === "before" && collector.default === "on-failure") {
    return "default: a pre-check runs before any cell failed, never on-failure"
  }
  if (collector.required === true && collector.default !== undefined && collector.default !== "always") {
    return `default: a required collector runs always, not ${collector.default}`
  }
  if ((collector.after ?? []).includes(collector.name)) return "after: a collector does not run after itself"
  if (collector.informative === true && collector.judge !== undefined) {
    return "judge: an informative collector's lines never decide, no judge decides over its records"
  }
  if (collector.scope_only === true && collector.scope === "cell") return "scope_only: a cell-scope collector's lines are its cell's"
  if (collector.scope_only === true && collector.judge !== undefined) {
    return "judge: a scope-only collector answers no cell, and a judge answers its cells"
  }
  return undefined
}

/** The collector manifest `file` holds as `text`, or why it is none. */
export const parse = parseWith(Collector, meaningOf)
