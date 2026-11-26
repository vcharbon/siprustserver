/**
 * The NAMED captures an invocation can take from a pcap root although the
 * artifact store does not hold them yet: a name the store lacks is looked up
 * as a capture file directly under each root (`Source.roots`), as
 * `<root>/<name>` or `<root>/<name>.pcap`, probed and never scanned (a root
 * may be a large tree). A file found is the capture `Discover.captureOf`
 * names it: `x.pcap` is capture `x`, so either spelling names it, and the
 * invocation goes on under the capture name.
 *
 * Two steps, so an invocation refuses before it writes: {@link plan} reads
 * the names and writes nothing; the invocation's own refusals then run with
 * the plan's pending captures counted as held (`population.ts`); {@link land}
 * ingests them in the foreground, saying so. What the cut and the replay then
 * do with them is theirs.
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
import { captureOf, isRingMember, type Source as Found } from "./discover.js"
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

/** How one invocation lands its plan: the tag its lines carry, the extraction concurrency, whether it writes. */
export interface How {
  readonly tag: string
  readonly concurrency: number
  readonly dryRun: boolean
}

/** A named list read against the store and the roots: each name as a capture, the ones to ingest, what was renamed. */
export interface Plan {
  readonly _tag: "plan"
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

/** The captures a plan would ingest. */
export const pendingOf = (plan: Plan): ReadonlyArray<string> => plan.pending.map((one) => one.capture)

/** The distinct capture files under `roots` that `name` may stand for, by their real path. */
const filesFor = Effect.fn("Adopt.filesFor")(function*(name: string, roots: ReadonlyArray<string>) {
  const fs = yield* FileSystem.FileSystem
  const found = new Map<string, string>()
  if (name === "" || name !== path.basename(name)) return [] as ReadonlyArray<string>
  for (const root of roots) {
    for (const candidate of [name, `${name}.pcap`]) {
      if (!isRingMember(candidate)) continue
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
 * and the pcap roots, writing nothing: a held name stays; one the store lacks
 * takes the capture name of the one capture file a root holds for it, and is
 * pending where the store lacks that capture too; one no root holds stays as
 * named. Two files for one name refuse. A derived universe plans nothing.
 */
export const resolve = Effect.fn("Adopt.resolve")(function*(paths: Paths, selection: Selection) {
  if (selection.universe !== "named") {
    return { _tag: "plan", positional: selection.positional, pending: [], renamed: [] } satisfies Plan
  }
  const fs = yield* FileSystem.FileSystem
  const held = new Set(yield* fs.readDirectory(paths.corpus).pipe(Effect.orElseSucceed(() => [] as Array<string>)))
  const roots = yield* Source.roots(paths)
  const named: Array<string> = []
  const pending = new Map<string, Found>()
  const renamed: Array<Plan["renamed"][number]> = []
  for (const name of selection.positional) {
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
  return { _tag: "plan", positional: [...new Set(named)], pending: [...pending.values()], renamed } satisfies Plan
})

/** {@link resolve}, each renamed name said under `tag`. */
export const plan = Effect.fn("Adopt.plan")(function*(paths: Paths, selection: Selection, tag: string) {
  const read = yield* resolve(paths, selection)
  if (read._tag === "plan") {
    for (const one of read.renamed) yield* Console.log(`[${tag}] ${one.name}: the capture ${one.capture} (${one.file})`)
  }
  return read
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
 * `plan`'s pending captures, each said and ingested by `ingest` (unless
 * `how.dryRun`, which says what it would ingest and writes nothing), its
 * outcome line printed ({@link settled}).
 */
export const landBy = <E, R>(paths: Paths, plan: Plan, how: How, ingest: Ingester<E, R>) =>
  Effect.gen(function*() {
    if (plan.pending.length === 0) return answered(plan.positional, [])
    for (const one of plan.pending) {
      yield* Console.log(
        `[${how.tag}] ${one.capture}: never ingested, found at ${one.pcaps[0]} — ` +
          (how.dryRun ? "would ingest and cut it" : "ingesting it first, then cutting it")
      )
    }
    if (how.dryRun) return answered(plan.positional, pendingOf(plan))
    const ingested = yield* ingest(plan.pending, paths.corpus, Math.max(1, how.concurrency))
    const { answer, lines } = settled(plan.positional, ingested)
    for (const line of lines) yield* Console.log(line)
    return answer
  })

/** {@link landBy} with the ingest every command runs. */
export const land = (paths: Paths, plan: Plan, how: How) => landBy(paths, plan, how, Source.ingest)
