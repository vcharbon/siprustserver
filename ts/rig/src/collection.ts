/**
 * Wiring (ADR-0060 item 10, appendix §10): the workspace file's collection
 * levels (`[collection]`), each setting collectors to `always`, `on-failure`,
 * `on-demand` or `never` for one job, and a lane file's `[collectors]`, which
 * excludes collectors and overrides their parameters.
 *
 * {@link check} runs before a job opens and refuses, naming the file, a level
 * or a lane that names a collector the path does not define, a level that sets
 * a `required` collector or a lane that excludes one, and a lane override of a
 * key the collector's manifest does not offer. A lane may tune a required
 * collector's parameters: that does not switch it off.
 */
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as Collector from "./collector.js"
import { decoderOf, Name, Params, type Refusal, refuse } from "./manifest-file.js"
import type { RigPath } from "./rig-path.js"

/** What a level sets a collector to. */
export const SETTINGS = ["always", "on-failure", "on-demand", "never"] as const
export type Setting = (typeof SETTINGS)[number]

/** The level every job takes unless the workspace's `default` or the job names another: each collector's own default. */
export const STANDARD = "standard"

/** The key of `[collection]` naming the level a job takes when it names none. */
export const DEFAULT_KEY = "default"

/** The key of a lane's `[collectors]` listing the collectors the lane never runs. */
export const EXCLUDE_KEY = "exclude"

/** A collection level: the collectors it sets, by setting. */
export const Level = Schema.Struct({
  always: Schema.optionalKey(Schema.Array(Name)),
  "on-failure": Schema.optionalKey(Schema.Array(Name)),
  "on-demand": Schema.optionalKey(Schema.Array(Name)),
  never: Schema.optionalKey(Schema.Array(Name))
})
export interface Level extends Schema.Schema.Type<typeof Level> {}

/** The workspace file's `[collection]`: `default`, the level's name, beside one table per level. */
export const Collection = Schema.Record(Schema.String, Schema.Union([Name, Level]))
export type Collection = typeof Collection.Type

/** A lane file's `[collectors]`: `exclude`, a list of names, beside one table of overrides per collector. */
export const LaneCollectors = Schema.Record(Schema.String, Schema.Union([Schema.Array(Name), Params]))
export type LaneCollectors = typeof LaneCollectors.Type

/**
 * A `[collection]` table as read, or why it is none (the dotted key under the
 * table first). A host file decodes its table here, by this package's own
 * schema runtime, rather than nesting {@link Collection} in a schema of its own.
 */
export const collectionOf: (raw: unknown) => Result.Result<Collection, string> = decoderOf(Collection)

/** A lane file's `[collectors]` table as read, or why it is none, as {@link collectionOf}. */
export const laneCollectorsOf: (raw: unknown) => Result.Result<LaneCollectors, string> = decoderOf(LaneCollectors)

/** The levels a `[collection]` defines and the one a job takes when it names none. */
export interface Levels {
  readonly default: string
  readonly levels: ReadonlyMap<string, Level>
}

/** The levels `collection` states, or why it states none (the dotted key first). */
export const levelsOf = (collection: Collection | undefined): Result.Result<Levels, string> => {
  const levels = new Map<string, Level>()
  let chosen = STANDARD
  for (const [key, value] of Object.entries(collection ?? {})) {
    if (key === DEFAULT_KEY) {
      if (typeof value !== "string") return Result.fail(`collection.${DEFAULT_KEY}: the name of a level`)
      chosen = value
      continue
    }
    if (typeof value === "string") return Result.fail(`collection.${key}: a level is a table`)
    if (!Schema.is(Name)(key)) return Result.fail(`collection.${key}: a level's name is lower-case letters, digits and dashes`)
    levels.set(key, value)
  }
  if (chosen !== STANDARD && !levels.has(chosen)) {
    return Result.fail(`collection.${DEFAULT_KEY}: names ${chosen}, which is neither ${STANDARD} nor a level of the file`)
  }
  return Result.succeed({ default: chosen, levels })
}

/** The collectors a lane excludes and those whose parameters it overrides, or why its table states neither. */
export const laneOf = (
  collectors: LaneCollectors | undefined
): Result.Result<{ readonly exclude: ReadonlyArray<string>; readonly overrides: ReadonlyMap<string, Readonly<Record<string, unknown>>> }, string> => {
  const overrides = new Map<string, Readonly<Record<string, unknown>>>()
  let exclude: ReadonlyArray<string> = []
  for (const [key, value] of Object.entries(collectors ?? {})) {
    if (key === EXCLUDE_KEY) {
      if (!isNames(value)) return Result.fail(`collectors.${EXCLUDE_KEY}: a list of collector names`)
      exclude = value
      continue
    }
    if (isNames(value)) return Result.fail(`collectors.${key}: a collector's overrides are a table`)
    overrides.set(key, value)
  }
  return Result.succeed({ exclude, overrides })
}

/** The workspace file and its `[collection]`, as a job reads them. */
export interface WorkspaceWiring {
  readonly file: string
  readonly collection: Collection | undefined
}

/** One lane file of the job and its `[collectors]`. */
export interface LaneWiring {
  readonly file: string
  readonly collectors: LaneCollectors | undefined
}

/** What a job wires: the workspace's levels, the level it asked for, and its lanes. */
export interface Wiring {
  readonly workspace?: WorkspaceWiring | undefined
  /** The level the job names; absent, the workspace's default. */
  readonly level?: string | undefined
  readonly lanes: ReadonlyArray<LaneWiring>
}

/** The keys a lane may override on `collector`: its parameters, and its delay at cell scope. */
export const overridableOf = (collector: Collector.Collector): ReadonlyArray<string> => [
  ...Object.keys(collector.params ?? {}),
  ...(collector.scope === "cell" ? ["delay_s"] : [])
]

const isNames = (value: ReadonlyArray<string> | Readonly<Record<string, unknown>>): value is ReadonlyArray<string> =>
  Array.isArray(value)

const checkLevel = (rig: RigPath, file: string, name: string, level: Level): Refusal | undefined => {
  const seen = new Map<string, Setting>()
  for (const setting of SETTINGS) {
    for (const collector of level[setting] ?? []) {
      const at = `collection.${name}.${setting}`
      const defined = rig.collectors.get(collector)
      if (defined === undefined) return refuse([file], `${at}: names ${collector}, which no collector of the path defines`)
      if (Collector.isRequired(defined.manifest)) {
        return refuse([file, defined.file], `${at}: names ${collector}, a required collector no level sets`)
      }
      const earlier = seen.get(collector)
      if (earlier !== undefined) return refuse([file], `${at}: names ${collector}, which collection.${name}.${earlier} names too`)
      seen.set(collector, setting)
    }
  }
  return undefined
}

const checkWorkspace = (rig: RigPath, workspace: WorkspaceWiring | undefined, asked: string | undefined): Refusal | undefined => {
  const levels = levelsOf(workspace?.collection)
  const file = workspace?.file
  if (Result.isFailure(levels)) return refuse(file === undefined ? [] : [file], levels.failure)
  for (const [name, level] of levels.success.levels) {
    const refusal = checkLevel(rig, file ?? "", name, level)
    if (refusal !== undefined) return refusal
  }
  if (asked !== undefined && asked !== STANDARD && !levels.success.levels.has(asked)) {
    return refuse(file === undefined ? [] : [file], `the job asks for the collection level ${asked}, which is neither ${STANDARD} nor a level of the file`)
  }
  return undefined
}

const checkLane = (rig: RigPath, lane: LaneWiring): Refusal | undefined => {
  const read = laneOf(lane.collectors)
  if (Result.isFailure(read)) return refuse([lane.file], read.failure)
  for (const name of read.success.exclude) {
    const defined = rig.collectors.get(name)
    const at = `collectors.${EXCLUDE_KEY}`
    if (defined === undefined) return refuse([lane.file], `${at}: names ${name}, which no collector of the path defines`)
    if (Collector.isRequired(defined.manifest)) {
      return refuse([lane.file, defined.file], `${at}: names ${name}, a required collector no lane excludes`)
    }
  }
  for (const [name, overrides] of read.success.overrides) {
    const defined = rig.collectors.get(name)
    if (defined === undefined) return refuse([lane.file], `collectors.${name}: names a collector the path does not define`)
    if (read.success.exclude.includes(name)) return refuse([lane.file], `collectors.${name}: the lane excludes it`)
    const offered = overridableOf(defined.manifest)
    const unknown = Object.keys(overrides).find((key) => !offered.includes(key))
    if (unknown !== undefined) {
      return refuse([lane.file, defined.file], `collectors.${name}.${unknown}: not a parameter ${name} offers`)
    }
  }
  return undefined
}

/** Whether the job's wiring holds over `rig`, or the refusal of the first file at fault. */
export const check = (rig: RigPath, wiring: Wiring): Result.Result<void, Refusal> => {
  const refusal = checkWorkspace(rig, wiring.workspace, wiring.level) ??
    wiring.lanes.map((lane) => checkLane(rig, lane)).find((found) => found !== undefined)
  return refusal === undefined ? Result.void : Result.fail(refusal)
}
