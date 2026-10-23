/**
 * A SOURCE added by path on the host: the pcaps an operator names, discovered,
 * extracted where the artifact store has nothing current for them, and
 * recorded in the population with the extractor that took them and the
 * failure where one refused — the ingest, as one function a campaign and an
 * `ingest` job both run.
 *
 * A path is taken only under a ROOT: the artifact store's {@link UPLOADS} or
 * one the deployment names (`Roots.pcapRoots`). A path elsewhere is refused
 * with the roots, never read: what the store ingests must be findable again by
 * the name the population keeps.
 */
import { Population, Sources } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as crypto from "node:crypto"
import * as path from "node:path"
import { Roots } from "../deployment/index.js"
import * as Fingerprint from "../fingerprint.js"
import type { Paths } from "../paths.js"
import type { Source } from "./discover.js"
import * as Extract from "./extract.js"
import * as Record from "./record.js"
import * as Settled from "./settled.js"

/** Where an upload saves a capture, under the artifact store. */
export const UPLOADS = "_uploads"

/**
 * The paths a list file names: one per line, trimmed, blank lines and `#`
 * comments skipped. A selection of thousands of pcaps travels as this file: a
 * shell glob of them is one argument a spawn is handed, past the kernel's
 * per-argument limit (E2BIG).
 */
export const pathsFromText = (text: string): ReadonlyArray<string> =>
  text.split(/\r?\n/).map((line) => line.trim()).filter((line) => line !== "" && !line.startsWith("#"))

/** A path list read: the file it is kept in, absolute, and its paths, absolute. */
export interface PathList {
  readonly file: string
  readonly paths: ReadonlyArray<string>
}

/**
 * The list `--paths-from` names — a file, its relative entries read against
 * its own directory, or `-`, stdin (`stdin` its text), read against `cwd` —
 * kept, absolute, as a content-named copy under `keepIn`: a launched job reads
 * that snapshot, so the list it ingests is the one the line was refused or
 * accepted on, whatever becomes of the file. A list that does not read is the
 * refusal's reason.
 */
export const readPathList = Effect.fn("Source.readPathList")(function*(
  from: string,
  cwd: string,
  keepIn: string,
  stdin: Effect.Effect<string, unknown> = Effect.fail("no stdin")
) {
  const fs = yield* FileSystem.FileSystem
  const source = from === "-" ? undefined : path.resolve(cwd, from)
  const text = yield* (source === undefined ? stdin : fs.readFileString(source)).pipe(
    Effect.catch((error) =>
      Effect.succeed({
        unreadable: `${source === undefined ? "stdin" : `--paths-from ${source}`} does not read: ${
          error instanceof Error ? error.message : String(error)
        }`
      })
    )
  )
  if (typeof text !== "string") return text.unreadable
  const base = source === undefined ? cwd : path.dirname(source)
  const paths = pathsFromText(text).map((entry) => path.resolve(base, entry))
  const body = `${paths.join("\n")}\n`
  const file = path.join(keepIn, `paths-${crypto.createHash("sha256").update(body).digest("hex").slice(0, 12)}.txt`)
  yield* fs.makeDirectory(keepIn, { recursive: true })
  yield* fs.writeFileString(file, body)
  return { file, paths } satisfies PathList
})

/** The roots a source path may lie under, absolute: the store's uploads, then the deployment's own. */
export const roots = Effect.fn("Source.roots")(function*(paths: Paths) {
  const named = yield* Effect.flatMap(Roots.Service, (roots) => roots.pcapRoots)
  return [path.join(paths.corpus, UPLOADS), ...named] as ReadonlyArray<string>
})

const under = (file: string, root: string): boolean => {
  const relative = path.relative(root, file)
  return relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative))
}

/**
 * Why these inputs may not be read, or `null`: each resolved against `cwd`
 * (the operator's directory) must lie under a root, as stated or once both
 * are followed through their links.
 */
export const refusal = Effect.fn("Source.refusal")(function*(
  inputs: ReadonlyArray<string>,
  cwd: string,
  paths: Paths
) {
  const fs = yield* FileSystem.FileSystem
  const realOrSelf = (one: string) => fs.realPath(one).pipe(Effect.orElseSucceed(() => one))
  const allowed = yield* roots(paths)
  const realRoots = yield* Effect.forEach(allowed, realOrSelf)
  const outside: Array<string> = []
  for (const input of inputs) {
    const absolute = path.resolve(cwd, input)
    const real = yield* realOrSelf(absolute)
    if (!allowed.some((root, at) => under(absolute, root) || under(real, realRoots[at]!))) outside.push(input)
  }
  if (outside.length === 0) return null
  return `${outside.map((one) => JSON.stringify(one)).join(", ")} ${
    outside.length === 1 ? "lies" : "lie"
  } outside the roots a source is read from (${allowed.join(", ")})`
})

/** What one capture's ingest came to; a current one may stand refused by the extraction it keeps. */
export type Outcome =
  | { readonly _tag: "current"; readonly failed?: string }
  | { readonly _tag: "ingested"; readonly landed: string }
  | { readonly _tag: "failed"; readonly why: string; readonly landed: string }

export interface Ingested {
  /** The sources under the capture names they stand at, a new generation's where one landed. */
  readonly sources: ReadonlyArray<Source>
  readonly outcomes: ReadonlyMap<string, Outcome>
  /** How many population rows were new. */
  readonly added: number
  /** The captures extracted by this call, refused ones included: what a cut after it must take. */
  readonly extracted: ReadonlySet<string>
  /** Every capture a new generation replaced in this call: old name → new name. */
  readonly renamed: ReadonlyMap<string, string>
}

/** One capture's outcome as a report line. */
export const line = (capture: string, outcome: Outcome): string => {
  switch (outcome._tag) {
    case "current":
      return `   =  ${capture}  current${
        outcome.failed === undefined ? "" : ` (its extraction refused it: ${outcome.failed})`
      }`
    case "ingested":
      return `   +  ${capture}  ingested — ${outcome.landed}`
    case "failed":
      return `   !  ${capture}  ${outcome.why}${outcome.landed === "" ? "" : ` — ${outcome.landed}`}`
  }
}

/**
 * Why each source must be extracted again, keyed by capture; absent where it
 * is current. ONE judgement, in two readings of the same facts: the
 * population row first (`Sources.ingestState` — a row never ingested, or
 * ingested under another extractor than this tree's, is stale whatever the
 * artifact store holds), then the artifact store's own record and files
 * (`Record.staleness`), which the store cannot see.
 */
export const staleness = Effect.fn("Source.staleness")(function*(
  sources: ReadonlyArray<Source>,
  artifacts: string,
  extractor: string
) {
  const population = yield* Population.Service
  const rows = new Map((yield* population.list).map((row) => [row.capture, row]))
  const stale = new Map<string, string>()
  for (const source of sources) {
    const row = rows.get(source.capture)
    const why = row === undefined
      ? "not in the population"
      : Sources.ingestState(row, extractor) === "stale"
      ? row.ingestedAt === null
        ? "the population has no ingest of it"
        : "the population's extractor is not this tree's"
      : yield* Record.staleness(artifacts, source, extractor)
    if (why !== undefined) stale.set(source.capture, why)
  }
  return stale as ReadonlyMap<string, string>
})

/**
 * Every named source extracted where `staleness` says so (a current one is
 * reported and not read), then every one recorded in the population — the
 * extractor hash and the failure with it: for a capture found current those
 * are read back off its ingest record, so the row and the artifact store agree
 * without a second extraction. A capture whose extraction FAILED still joins
 * and still carries its ingest record: re-reading the same pcap would only
 * produce the same refusal.
 */
export const ingest = Effect.fn("Source.ingest")(function*(
  sources: ReadonlyArray<Source>,
  artifacts: string,
  concurrency: number
) {
  const population = yield* Population.Service
  const extractor = yield* Fingerprint.extractor
  const outcomes = new Map<string, Outcome>()
  // Every capture under the generation it stands at.
  const standing = yield* Effect.forEach(sources, (source) => Extract.standing(artifacts, source))
  const why = yield* staleness(standing, artifacts, extractor)
  const stale = standing.filter((source) => {
    if (!why.has(source.capture)) outcomes.set(source.capture, { _tag: "current" })
    return why.has(source.capture)
  })
  const settled = stale.length === 0 ? new Set<string>() : yield* Settled.captures()
  const extracted = yield* Extract.phase(stale, artifacts, extractor, concurrency, undefined, (capture) =>
    settled.has(capture)
  )
  const renamed = new Map<string, string>()
  const landedBy = new Map<string, Source>()
  extracted.forEach((one, i) => {
    const from = stale[i]!.capture
    if (one.capture !== from) renamed.set(from, one.capture)
    landedBy.set(from, one.source)
    outcomes.set(
      one.capture,
      one.broke === undefined
        ? { _tag: "ingested", landed: one.landed }
        : { _tag: "failed", why: one.broke, landed: one.landed }
    )
  })
  const landed = standing.map((source) => landedBy.get(source.capture) ?? source)
  const rows = yield* Effect.forEach(landed, (source) =>
    Effect.gen(function*() {
      const outcome = outcomes.get(source.capture)
      if (outcome === undefined || outcome._tag === "current") {
        // Current: the record names this extractor and its instant; the failure is whatever it left.
        const failed = yield* Record.failureOf(artifacts, source.capture)
        const at = yield* Record.ingestedAt(artifacts, source.capture)
        outcomes.set(
          source.capture,
          failed === undefined ? { _tag: "current" } : { _tag: "current", failed: failed.error }
        )
        return {
          capture: source.capture,
          sourcePath: source.pcaps.join("\n"),
          ingested: false,
          ...(at === null ? {} : { ingestedAt: at }),
          extractorHash: extractor,
          ingestFailed: failed === undefined ? null : failed.error
        }
      }
      return {
        capture: source.capture,
        sourcePath: source.pcaps.join("\n"),
        ingested: true,
        extractorHash: extractor,
        ingestFailed: outcome._tag === "failed" ? outcome.why : null
      }
    }))
  const added = yield* population.add(rows)
  return {
    sources: landed,
    outcomes: outcomes as ReadonlyMap<string, Outcome>,
    added,
    extracted: new Set(extracted.map((one) => one.capture)) as ReadonlySet<string>,
    renamed: renamed as ReadonlyMap<string, string>
  } satisfies Ingested
})
