/**
 * The NAMED captures an invocation can take from a pcap root although the
 * artifact store does not hold them yet: a name the store lacks is looked up
 * as a file directly under each root (`Source.roots`), as `<root>/<name>` or
 * `<root>/<name>.pcap`, probed and never scanned (a root may be a large tree).
 * A file found is the capture `Discover.captureOf` names it: `x.pcap` is
 * capture `x`, so either spelling names it, and the invocation goes on under
 * the capture name. One the store does not hold yet is ingested here, in the
 * foreground, before the invocation's own refusals read the store
 * (`population.ts`); what the cut and the replay then do with it is theirs.
 *
 * A name no root holds is left as named, for the store's refusal to answer.
 * Two different files for one name refuse the invocation, naming both: which
 * one was meant is the operator's to say.
 */
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import type { Paths } from "../paths.js"
import type { Selection } from "../universe.js"
import { captureOf, type Source as Found } from "./discover.js"
import * as Source from "./source.js"

/** What adoption came to: the named list as the store now holds it, or why the invocation is refused. */
export type Answer =
  | {
    readonly _tag: "named"
    /** The selection's names, each the capture name a root's file gave it, duplicates dropped. */
    readonly positional: ReadonlyArray<string>
    /** The captures ingested here, or with `dryRun` the ones that would be. */
    readonly ingested: ReadonlyArray<string>
  }
  | { readonly _tag: "refused"; readonly why: string }

/** How one invocation adopts: the tag its lines carry, the extraction concurrency, whether it writes. */
export interface How {
  readonly tag: string
  readonly concurrency: number
  readonly dryRun: boolean
}

/** A named list read against the store and the roots: each name as a capture, the ones to ingest, what was renamed. */
export interface Resolved {
  readonly _tag: "resolved"
  readonly positional: ReadonlyArray<string>
  /** The captures the store lacks and a root holds, one file each. */
  readonly pending: ReadonlyArray<Found>
  /** Each name that stands for another capture name: the name, the capture, the file that says so. */
  readonly renamed: ReadonlyArray<{ readonly name: string; readonly capture: string; readonly file: string }>
}

const refused = (why: string) => ({ _tag: "refused", why }) as const

const answered = (positional: ReadonlyArray<string>, ingested: ReadonlyArray<string>): Answer => ({
  _tag: "named",
  positional: [...new Set(positional)],
  ingested
})

/** The distinct files under `roots` that `name` may stand for, by their real path. */
const filesFor = Effect.fn("Adopt.filesFor")(function*(name: string, roots: ReadonlyArray<string>) {
  const fs = yield* FileSystem.FileSystem
  const found = new Map<string, string>()
  if (name === "" || name !== path.basename(name)) return [] as ReadonlyArray<string>
  for (const root of roots) {
    for (const candidate of [name, `${name}.pcap`]) {
      const file = path.join(root, candidate)
      const info = yield* fs.stat(file).pipe(Effect.option)
      if (info._tag === "None" || info.value.type !== "File") continue
      if (candidate !== name && captureOf(file) !== name) continue
      const real = yield* fs.realPath(file).pipe(Effect.orElseSucceed(() => file))
      if (!found.has(real)) found.set(real, file)
    }
  }
  return [...found.values()] as ReadonlyArray<string>
})

/**
 * The selection's names read against the artifact store at `paths.corpus`
 * and the pcap roots: a held name stays; one the store lacks takes the
 * capture name of the one file a root holds for it, and is pending where the
 * store lacks that capture too; one no root holds stays as named. Two files
 * for one name refuse.
 */
export const resolve = Effect.fn("Adopt.resolve")(function*(paths: Paths, positional: ReadonlyArray<string>) {
  const fs = yield* FileSystem.FileSystem
  const held = new Set(yield* fs.readDirectory(paths.corpus).pipe(Effect.orElseSucceed(() => [] as Array<string>)))
  const roots = yield* Source.roots(paths)
  const named: Array<string> = []
  const pending = new Map<string, Found>()
  const renamed: Array<Resolved["renamed"][number]> = []
  for (const name of positional) {
    if (held.has(name)) {
      named.push(name)
      continue
    }
    const files = yield* filesFor(name, roots)
    if (files.length > 1) return refused(`${JSON.stringify(name)} names ${files.length} pcaps: ${files.join(", ")}`)
    const file = files[0]
    if (file === undefined) {
      named.push(name)
      continue
    }
    const capture = captureOf(file)
    named.push(capture)
    if (capture !== name) renamed.push({ name, capture, file })
    if (!held.has(capture) && !pending.has(capture)) pending.set(capture, { capture, pcaps: [file] })
  }
  return { _tag: "resolved", positional: named, pending: [...pending.values()], renamed } satisfies Resolved
})

/** What an ingest of the pending captures came to, as the answer and the lines that report it. */
export const settled = (
  positional: ReadonlyArray<string>,
  ingested: Pick<Source.Ingested, "sources" | "outcomes" | "renamed">
): { readonly answer: Answer; readonly lines: ReadonlyArray<string> } => {
  const failed: Array<string> = []
  const lines: Array<string> = []
  for (const source of ingested.sources) {
    const outcome = ingested.outcomes.get(source.capture) ?? { _tag: "current" as const }
    if (Source.standsFailed(outcome)) failed.push(source.capture)
    lines.push(Source.line(source.capture, outcome))
  }
  if (failed.length > 0) {
    return { answer: refused(`${failed.join(", ")} did not ingest (the line above says why): there is nothing to cut`), lines }
  }
  return {
    answer: answered(
      positional.map((name) => ingested.renamed.get(name) ?? name),
      ingested.sources.map((source) => source.capture)
    ),
    lines
  }
}

/** How pending captures are ingested: `Source.ingest`, or a test's stand-in. */
export type Ingester<E, R> = (
  sources: ReadonlyArray<Found>,
  artifacts: string,
  concurrency: number
) => Effect.Effect<Pick<Source.Ingested, "sources" | "outcomes" | "renamed">, E, R>

/**
 * The selection's named captures the store lacks, taken from the pcap roots
 * ({@link resolve}): each one found is said, ingested by `ingest` (unless
 * `how.dryRun`, which says what it would ingest and writes nothing), and its
 * outcome line printed ({@link settled}). A derived universe names nothing
 * and is answered as it is.
 */
export const fromRootsBy = <E, R>(paths: Paths, selection: Selection, how: How, ingest: Ingester<E, R>) =>
  Effect.gen(function*() {
    if (selection.universe !== "named") return answered(selection.positional, [])
    const read = yield* resolve(paths, selection.positional)
    if (read._tag === "refused") return read
    for (const one of read.renamed) yield* Console.log(`[${how.tag}] ${one.name}: the capture ${one.capture} (${one.file})`)
    if (read.pending.length === 0) return answered(read.positional, [])
    for (const one of read.pending) {
      yield* Console.log(
        `[${how.tag}] ${one.capture}: never ingested, found at ${one.pcaps[0]} — ` +
          (how.dryRun ? "would ingest and cut it" : "ingesting it first, then cutting it")
      )
    }
    if (how.dryRun) return answered(read.positional, read.pending.map((one) => one.capture))
    const ingested = yield* ingest(read.pending, paths.corpus, Math.max(1, how.concurrency))
    const { answer, lines } = settled(read.positional, ingested)
    for (const line of lines) yield* Console.log(line)
    return answer
  })

/** {@link fromRootsBy} with the ingest every command runs. */
export const fromRoots = (paths: Paths, selection: Selection, how: How) =>
  fromRootsBy(paths, selection, how, Source.ingest)
