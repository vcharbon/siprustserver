/**
 * The rig path (ADR-0060 item 2, appendix §2): the pack, then the workspace,
 * each holding `collectors/`, `judges/`, `operations/` and `parts/`. Loading it
 * reads and validates every manifest before anything runs, and refuses, naming
 * the file:
 *
 * - a manifest that does not decode, states a key its schema does not name, or
 *   whose `name` is not its directory's;
 * - an executable or module it names that is missing, or an executable that
 *   is not executable;
 * - a name defined twice along the path (naming both files), a collector's
 *   judge no judge defines, a part schema that is no JSON object, and a path
 *   `pack.toml` names that its tree does not hold;
 * - a collector named like a source the base answers lines as, or like a
 *   judge another collector names (naming both files), a lock named by
 *   collectors of two scopes (naming both files), and a cycle of `after`
 *   among one scope's collectors (naming each file).
 */
import { Verification } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import * as path from "node:path"
import * as Collector from "./collector.js"
import * as CollectorOrder from "./collector-order.js"
import * as Judge from "./judge.js"
import { type Refusal, refuse } from "./manifest-file.js"
import * as Operation from "./operation.js"
import * as Pack from "./pack.js"
import * as Part from "./part.js"

/** Who owns a root of the path. */
export type Level = "pack" | "workspace"

/** A manifest found on the path. */
export interface Defined<M> {
  readonly name: string
  /** The manifest file. */
  readonly file: string
  /** Its directory: the working directory of what it runs. */
  readonly dir: string
  readonly level: Level
  readonly manifest: M
}

/** A part schema, `parts/<kind>.schema.json`, and what the base reads of it (`part.ts`). */
export interface PartSchema {
  readonly kind: string
  readonly file: string
  readonly level: Level
  readonly schema: Readonly<Record<string, unknown>>
  readonly reads: Part.Kind
}

/** Every plug-in the path defines, by name. */
export interface RigPath {
  readonly pack: Defined<Pack.Pack> | undefined
  readonly collectors: ReadonlyMap<string, Defined<Collector.Collector>>
  readonly judges: ReadonlyMap<string, Defined<Judge.Judge>>
  readonly operations: ReadonlyMap<string, Defined<Operation.Operation>>
  readonly parts: ReadonlyMap<string, PartSchema>
}

/** The roots of a path: the pack's directory, then the workspace's; either may be absent, and a workspace that is the pack's directory is the pack alone. */
export interface Roots {
  readonly pack?: string | undefined
  readonly workspace?: string | undefined
}

/** The directory of each plug-in kind under a root. */
export const DIRS = { collectors: "collectors", judges: "judges", operations: "operations", parts: "parts" } as const

/** The suffix of a part schema's file name. */
export const PART_SUFFIX = ".schema.json"

const PART_KIND = /^[a-z0-9][a-z0-9-]*$/

type Kind<M> = {
  readonly dir: string
  readonly file: string
  readonly what: string
  readonly parse: (file: string, text: string) => Result.Result<M, Refusal>
  readonly executables: (manifest: M) => ReadonlyArray<readonly [string, string]>
  readonly modules: (manifest: M) => ReadonlyArray<readonly [string, string]>
}

const COLLECTORS: Kind<Collector.Collector> = {
  dir: DIRS.collectors,
  file: Collector.FILE,
  what: "collector",
  parse: Collector.parse,
  executables: (manifest) => Collector.executablesOf(manifest).map(([step, file]) => [`run.${step}`, file] as const),
  modules: () => []
}

const JUDGES: Kind<Judge.Judge> = {
  dir: DIRS.judges,
  file: Judge.FILE,
  what: "judge",
  parse: Judge.parse,
  executables: (manifest) => (manifest.run === undefined ? [] : [["run", manifest.run] as const]),
  modules: (manifest) => (manifest.module === undefined ? [] : [["module", manifest.module] as const])
}

const OPERATIONS: Kind<Operation.Operation> = {
  dir: DIRS.operations,
  file: Operation.FILE,
  what: "operation",
  parse: Operation.parse,
  executables: (manifest) => Operation.executablesOf(manifest).map(([step, file]) => [`run.${step}`, file] as const),
  modules: () => []
}

/** What stands at `file`: a file, whether any execute bit is set, a directory, or nothing. */
const standing = (file: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const info = yield* fs.stat(file).pipe(Effect.option)
    if (info._tag === "None") return "missing" as const
    if (info.value.type === "Directory") return "directory" as const
    if (info.value.type !== "File") return "other" as const
    return (info.value.mode & 0o111) === 0 ? ("file" as const) : ("executable" as const)
  })

/** The entries of `dir` a plug-in may be, dotfiles aside, sorted; none where it does not exist. */
const entriesOf = (dir: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const entries = yield* fs.readDirectory(dir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))
    return entries.filter((entry) => !entry.startsWith(".")).sort()
  })

/** Why the file a manifest names at `key` does not hold, or `undefined`. */
const checkNamed = (dir: string, key: string, named: string, wanted: "executable" | "file") =>
  Effect.gen(function*() {
    const at = yield* standing(path.resolve(dir, named))
    if (at === "missing") return `${key}: ${named} does not exist`
    if (at === "directory" || at === "other") return `${key}: ${named} is not a file`
    if (wanted === "executable" && at !== "executable") return `${key}: ${named} is not executable`
    return undefined
  })

const loadKind = <M extends { readonly name: string }>(kind: Kind<M>, root: string, level: Level) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const base = path.join(root, kind.dir)
    const found: Array<Defined<M>> = []
    for (const entry of yield* entriesOf(base)) {
      const dir = path.join(base, entry)
      if ((yield* standing(dir)) !== "directory") {
        return yield* Effect.fail(refuse([dir], `not a ${kind.what} directory: ${kind.dir}/ holds one directory per ${kind.what}`))
      }
      const file = path.join(dir, kind.file)
      const text = yield* fs.readFileString(file).pipe(Effect.option)
      if (text._tag === "None") return yield* Effect.fail(refuse([dir], `holds no ${kind.file}`))
      const manifest = kind.parse(file, text.value)
      if (Result.isFailure(manifest)) return yield* Effect.fail(manifest.failure)
      if (manifest.success.name !== entry) {
        return yield* Effect.fail(refuse([file], `name: ${manifest.success.name} stands in the directory ${entry}`))
      }
      const named = [
        ...kind.executables(manifest.success).map(([key, at]) => checkNamed(dir, key, at, "executable")),
        ...kind.modules(manifest.success).map(([key, at]) => checkNamed(dir, key, at, "file"))
      ]
      for (const check of named) {
        const why = yield* check
        if (why !== undefined) return yield* Effect.fail(refuse([file], why))
      }
      found.push({ name: entry, file, dir, level, manifest: manifest.success })
    }
    return found
  })

const loadParts = (root: string, level: Level) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const base = path.join(root, DIRS.parts)
    const found: Array<PartSchema> = []
    for (const entry of yield* entriesOf(base)) {
      const file = path.join(base, entry)
      const kind = entry.endsWith(PART_SUFFIX) ? entry.slice(0, -PART_SUFFIX.length) : undefined
      if (kind === undefined || !PART_KIND.test(kind) || (yield* standing(file)) === "directory") {
        return yield* Effect.fail(refuse([file], `not a part schema: ${DIRS.parts}/ holds <kind>${PART_SUFFIX} files`))
      }
      const text = yield* fs.readFileString(file).pipe(Effect.orElseSucceed(() => ""))
      const schema = yield* Effect.try(() => JSON.parse(text) as unknown).pipe(Effect.orElseSucceed(() => undefined))
      if (typeof schema !== "object" || schema === null || Array.isArray(schema)) {
        return yield* Effect.fail(refuse([file], "a part schema is one JSON Schema object"))
      }
      const reads = Part.kindOf(file, schema as Record<string, unknown>)
      if (Result.isFailure(reads)) return yield* Effect.fail(reads.failure)
      found.push({ kind, file, level, schema: schema as Record<string, unknown>, reads: reads.success })
    }
    return found
  })

/** The entries of every root by name, refusing a name a second root defines again. */
const byName = <T extends { readonly file: string }>(
  levels: ReadonlyArray<ReadonlyArray<T>>,
  nameOf: (item: T) => string,
  what: string
): Result.Result<ReadonlyMap<string, T>, Refusal> => {
  const out = new Map<string, T>()
  for (const item of levels.flat()) {
    const first = out.get(nameOf(item))
    if (first !== undefined) return Result.fail(refuse([first.file, item.file], `${what} ${nameOf(item)} is defined twice`))
    out.set(nameOf(item), item)
  }
  return Result.succeed(out)
}

const loadPack = (root: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const file = path.join(root, Pack.FILE)
    const text = yield* fs.readFileString(file).pipe(Effect.option)
    if (text._tag === "None") return yield* Effect.fail(refuse([root], `a pack holds ${Pack.FILE} at its root`))
    const manifest = Pack.parse(file, text.value)
    if (Result.isFailure(manifest)) return yield* Effect.fail(manifest.failure)
    for (const named of Pack.pathsOf(manifest.success)) {
      const at = yield* standing(path.resolve(root, named.path))
      const holds = at === "missing"
        ? false
        : named.is === "either"
        ? true
        : named.is === "directory"
        ? at === "directory"
        : at !== "directory"
      if (!holds) {
        return yield* Effect.fail(refuse([file], `${named.key}: the pack holds no ${named.is === "either" ? "path" : named.is} ${named.path}`))
      }
    }
    const defined: Defined<Pack.Pack> = { name: manifest.success.name, file, dir: root, level: "pack", manifest: manifest.success }
    return defined
  })

/** The workspace root of `roots` the path reads: none where it is the pack's own directory, which the path reads once, as the pack. */
const workspaceOf = (roots: Roots): string | undefined =>
  roots.workspace === undefined || (roots.pack !== undefined && path.resolve(roots.workspace) === path.resolve(roots.pack))
    ? undefined
    : roots.workspace

/** The path the roots make, or the first refusal of a manifest on it. */
export const load = (roots: Roots): Effect.Effect<RigPath, Refusal, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const pack = roots.pack === undefined ? undefined : yield* loadPack(roots.pack)
    const workspace = workspaceOf(roots)
    const levels: ReadonlyArray<readonly [string, Level]> = [
      ...(roots.pack === undefined ? [] : [[roots.pack, "pack"] as const]),
      ...(workspace === undefined ? [] : [[workspace, "workspace"] as const])
    ]
    const collectors = yield* Effect.forEach(levels, ([root, level]) => loadKind(COLLECTORS, root, level))
    const judges = yield* Effect.forEach(levels, ([root, level]) => loadKind(JUDGES, root, level))
    const operations = yield* Effect.forEach(levels, ([root, level]) => loadKind(OPERATIONS, root, level))
    const parts = yield* Effect.forEach(levels, ([root, level]) => loadParts(root, level))
    const path: RigPath = {
      pack,
      collectors: yield* Effect.fromResult(byName(collectors, (item) => item.name, "collector")),
      judges: yield* Effect.fromResult(byName(judges, (item) => item.name, "judge")),
      operations: yield* Effect.fromResult(byName(operations, (item) => item.name, "operation")),
      parts: yield* Effect.fromResult(byName(parts, (item) => item.kind, "part schema"))
    }
    for (const collector of path.collectors.values()) {
      const judge = collector.manifest.judge
      if (judge !== undefined && !path.judges.has(judge)) {
        return yield* Effect.fail(refuse([collector.file], `judge: names ${judge}, which no judge of the path defines`))
      }
    }
    const refused = sourceNamed(path) ?? judgeNamed(path) ?? lockOfNestedScopes(path) ?? afterCycle(path)
    if (refused !== undefined) return yield* Effect.fail(refused)
    return path
  })

/**
 * The first collector named like a judge another collector names, refused
 * naming both manifests: the lines of a collector naming a judge carry the
 * judge's name as their source, so the namesake's lines would stand for its.
 */
const judgeNamed = (path: RigPath): Refusal | undefined => {
  for (const collector of path.collectors.values()) {
    const judge = collector.manifest.judge
    const namesake = judge === undefined || judge === collector.name ? undefined : path.collectors.get(judge)
    if (namesake !== undefined) {
      return refuse([namesake.file, collector.file], `name: ${judge} is the judge ${collector.name} names, whose lines carry its name`)
    }
  }
  return undefined
}

/**
 * The first cycle the `after` of one scope's collectors make, refused naming
 * its manifests: none of its collectors could run after the others.
 */
const afterCycle = (path: RigPath): Refusal | undefined => {
  for (const scope of Collector.SCOPES) {
    const ofScope = [...path.collectors.values()].filter((collector) => collector.manifest.scope === scope)
    const cycle = CollectorOrder.cycleOf(ofScope)
    if (cycle !== undefined) {
      return refuse(
        [...new Set(cycle)].map((name) => path.collectors.get(name)!.file),
        `after: ${cycle.join(" runs after ")}: a cycle, so none of them runs first`
      )
    }
  }
  return undefined
}

/**
 * The first collector or judge named like a source the base answers lines as
 * (`Verification.BASE_SOURCES`), refused: its answers would stand for the
 * base's.
 */
const sourceNamed = (path: RigPath): Refusal | undefined => {
  for (const defined of [...path.collectors.values(), ...path.judges.values()]) {
    if (Verification.baseSources.includes(defined.name)) {
      return refuse([defined.file], `name: ${defined.name} is a source the base answers its own lines as`)
    }
  }
  return undefined
}

/**
 * The first lock collectors of two scopes name, refused naming both
 * manifests: scopes nest (a job's batches, a batch's cells), so the outer
 * scope's collector would hold the lock while the inner one's waits for it.
 */
const lockOfNestedScopes = (path: RigPath): Refusal | undefined => {
  const firstNaming = new Map<string, Defined<Collector.Collector>>()
  for (const collector of path.collectors.values()) {
    for (const lock of collector.manifest.locks ?? []) {
      const first = firstNaming.get(lock)
      if (first === undefined) firstNaming.set(lock, collector)
      else if (first.manifest.scope !== collector.manifest.scope) {
        return refuse(
          [first.file, collector.file],
          `locks: ${lock} is named by the ${first.manifest.scope} collector ${first.name} and the ${collector.manifest.scope} collector ${collector.name}, ` +
            `whose scopes nest: the outer one would hold it while the inner one waits`
        )
      }
    }
  }
  return undefined
}
