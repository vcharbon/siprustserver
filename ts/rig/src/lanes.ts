/**
 * The lanes of one root (ADR-0060 appendix §11): every `lanes/<id>.toml` of
 * it, each with the parts it names read from `targets/<kind>/<name>.toml` of
 * the same root and validated against the rig path's `parts/<kind>.schema.json`,
 * the operations it binds resolved on the path, and its capabilities.
 *
 * Loading refuses, naming the file: a lane file that does not hold
 * (`lane.ts`), an id that is not a name, a part kind the path has no schema
 * for, a part file that is not there, a part file its schema refuses (the
 * dotted field named), and a binding to an operation the path does not define
 * or whose parts the lane lacks. {@link sharedOf} refuses a job whose lanes
 * name different files of a kind its schema declares shared.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import type * as Collection from "./collection.js"
import * as Lane from "./lane.js"
import { Name, type Refusal, refuse } from "./manifest-file.js"
import type * as Operation from "./operation.js"
import * as Part from "./part.js"
import * as RigPath from "./rig-path.js"

/** The capabilities the base derives from a lane's clock and backend mode. */
export const BASE_CAPABILITIES = {
  /** The rig serves the backend the system calls (`served`, or a simulated lane), so recorded decisions replay. */
  scriptedRouting: "scripted-routing",
  /** The rig sits on the system's backend path (`served`, `proxied`, or a simulated lane), so it sees every exchange. */
  routingVisible: "routing-visible",
  /** The system runs on the wall clock. */
  realClock: "real-clock",
  /** The system runs on a paused clock, so a long timer takes no wall-clock time. */
  pausedClock: "paused-clock"
} as const

/** The prefix of the capability a binding in `[operations]` gives: `operation:<name>`. */
export const OPERATION_PREFIX = "operation:"

/** A lane as loaded. */
export interface Loaded {
  readonly id: string
  readonly file: string
  readonly lane: Lane.LaneFile
  /** Per kind, the part the lane names. */
  readonly parts: ReadonlyMap<string, Part.Part>
  /** Per name a case asks for, the operation bound to it. */
  readonly operations: ReadonlyMap<string, RigPath.Defined<Operation.Operation>>
  readonly capabilities: ReadonlySet<string>
}

/**
 * The capabilities of a lane: those of its clock and backend mode
 * ({@link BASE_CAPABILITIES}), those its parts' schemas give (`part.ts`), and
 * `operation:<name>` for each name bound in `[operations]`.
 */
export const capabilitiesOf = (
  lane: Lane.LaneFile,
  parts: ReadonlyArray<readonly [Part.Kind, Part.Part]>,
  bound: ReadonlyArray<string>
): ReadonlySet<string> => {
  const derived = new Set<string>()
  const mode = lane.backend?.mode
  if (lane.clock === "simulated") {
    derived.add(BASE_CAPABILITIES.scriptedRouting).add(BASE_CAPABILITIES.routingVisible).add(BASE_CAPABILITIES.pausedClock)
  } else {
    derived.add(BASE_CAPABILITIES.realClock)
    if (mode === "served") derived.add(BASE_CAPABILITIES.scriptedRouting)
    if (mode === "served" || mode === "proxied") derived.add(BASE_CAPABILITIES.routingVisible)
  }
  for (const [kind, part] of parts) for (const capability of Part.capabilitiesOf(kind, part.value)) derived.add(capability)
  for (const name of bound) derived.add(`${OPERATION_PREFIX}${name}`)
  return derived
}

/** A lane's `[collectors]`, as the job's wiring check reads it (`Collection.check`). */
export const wiringOf = (lane: Loaded): Collection.LaneWiring => ({ file: lane.file, collectors: lane.lane.collectors })

const readText = (file: string) =>
  Effect.flatMap(FileSystem.FileSystem, (fs) => fs.readFileString(file)).pipe(
    Effect.mapError((error) => refuse([file], `unreadable: ${error.message}`))
  )

/** The lane ids under `dir`, sorted; none where the directory is absent. */
const idsOf = (dir: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    if (!(yield* fs.exists(dir).pipe(Effect.orElseSucceed(() => false)))) return []
    const names = yield* fs.readDirectory(dir).pipe(Effect.mapError((error) => refuse([dir], `unreadable: ${error.message}`)))
    return names.filter((name) => name.endsWith(".toml")).map((name) => name.slice(0, -".toml".length)).sort()
  })

/** The bindings of `lane` resolved on `rig`, or why one does not hold. */
const operationsOf = (rig: RigPath.RigPath, file: string, lane: Lane.LaneFile, parts: ReadonlyMap<string, Part.Part>) => {
  const bound = new Map<string, RigPath.Defined<Operation.Operation>>()
  for (const [name, operation] of Object.entries(lane.operations ?? {})) {
    const defined = rig.operations.get(operation)
    if (defined === undefined) return Result.fail(refuse([file], `operations.${name}: names ${operation}, which no operation of the path defines`))
    const lacks = (defined.manifest.needs ?? []).filter((kind) => !parts.has(kind))
    if (lacks.length > 0) {
      return Result.fail(refuse([file, defined.file], `operations.${name}: ${operation} needs the part ${lacks.join(", ")}, which the lane names none of`))
    }
    bound.set(name, defined)
  }
  return Result.succeed(bound)
}

/** Every lane of `root`, by `order`, then id; the first file at fault refuses them all. */
export const load = (rig: RigPath.RigPath, root: string): Effect.Effect<ReadonlyArray<Loaded>, Refusal, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const dir = path.join(root, Lane.LANES_DIR)
    const read = new Map<string, Part.Part>()
    const partOf = (lane: string, kind: string, name: string) =>
      Effect.gen(function*() {
        const schema = rig.parts.get(kind)
        if (schema === undefined) return yield* Effect.fail(refuse([lane], `parts.${kind}: no part schema ${kind}${RigPath.PART_SUFFIX} on the path`))
        const file = Part.fileOf(root, kind, name)
        const known = read.get(file)
        if (known !== undefined) return [schema.reads, known] as const
        if (!(yield* fs.exists(file).pipe(Effect.orElseSucceed(() => false)))) {
          return yield* Effect.fail(refuse([lane], `parts.${kind}: no part file ${file}`))
        }
        const value = Part.parse(schema.reads, file, yield* readText(file))
        if (Result.isFailure(value)) return yield* Effect.fail(value.failure)
        const part: Part.Part = { kind, name, file, value: value.success }
        read.set(file, part)
        return [schema.reads, part] as const
      })
    const loaded: Array<Loaded> = []
    for (const id of yield* idsOf(dir)) {
      const file = path.join(dir, `${id}.toml`)
      if (!Schema.is(Name)(id)) return yield* Effect.fail(refuse([file], "a lane id is lower-case letters, digits and dashes"))
      const lane = Lane.parse(file, yield* readText(file))
      if (Result.isFailure(lane)) return yield* Effect.fail(lane.failure)
      const named = yield* Effect.forEach(Object.entries(lane.success.parts ?? {}), ([kind, name]) => partOf(file, kind, name))
      const parts = new Map(named.map(([, part]) => [part.kind, part] as const))
      const operations = operationsOf(rig, file, lane.success, parts)
      if (Result.isFailure(operations)) return yield* Effect.fail(operations.failure)
      loaded.push({
        id,
        file,
        lane: lane.success,
        parts,
        operations: operations.success,
        capabilities: capabilitiesOf(lane.success, named, [...operations.success.keys()])
      })
    }
    const orderOf = (lane: Loaded) => lane.lane.order ?? Number.MAX_SAFE_INTEGER
    return loaded.sort((a, b) => orderOf(a) - orderOf(b) || a.id.localeCompare(b.id))
  })

/**
 * The parts of the shared kinds the lanes of one job name: one file per kind
 * declared `shared` (`part.ts`), or none on every lane. Refuses, naming the
 * lane files, where two lanes differ.
 */
export const sharedOf = (
  rig: RigPath.RigPath,
  lanes: ReadonlyArray<Loaded>
): Result.Result<ReadonlyMap<string, Part.Part | undefined>, Refusal> => {
  const shared = new Map<string, Part.Part | undefined>()
  for (const [kind, schema] of rig.parts) {
    if (!schema.reads.shared) continue
    const named = (lane: Loaded) => lane.parts.get(kind)?.name ?? "none"
    if (new Set(lanes.map(named)).size > 1) {
      return Result.fail(
        refuse(
          lanes.map((lane) => lane.file),
          `the lanes of one job name one ${kind} part: ${lanes.map((lane) => `${lane.id} names ${named(lane)}`).join(", ")}`
        )
      )
    }
    shared.set(kind, lanes[0]?.parts.get(kind))
  }
  return Result.succeed(shared)
}
