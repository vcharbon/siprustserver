/**
 * EXTRACTION: a capture on this host to the artifacts a cut reads, and the
 * record that says which pcaps and which extractor they came from.
 *
 * A capture is an ordered SET of pcap files — one file, or the consecutive
 * files of one tap, which the extractor's `--contiguous` reads as one stream
 * and refuses when they overlap, leave a gap wider than {@link SET_MAX_GAP_MS}
 * or come out of time order. A call may start in one file and end in the
 * next, so the set is the unit of extraction, never the file.
 *
 * Only anonymised data is ever persisted. The extractor writes the raw flows
 * document to a scratch file under the store, the deployment's anonymiser
 * (`Extraction`) turns it into the capture's kept document, and the scratch
 * goes with the call — the pcaps themselves are never copied into the store,
 * and nothing is written beside them. The anonymisation runs under the seed
 * the deployment keeps for the capture name, so extracting the capture again
 * reproduces its documents byte for byte; the extractor's `index` then writes
 * the per-call index. An extraction that would anonymise a capture anew lands
 * under the capture's next generation name (`generation.ts`), never the old
 * one.
 *
 * An extraction that FAILED is recorded as a capture with no cases and the
 * reason, exactly as a refusal is: silence is never an outcome. It still
 * writes its ingest record, so the failure stands until a pcap or the
 * extractor changes rather than being re-attempted blind on every run.
 */
import { runner } from "@sip/toolchain"
import { IntoFile } from "@sip/toolchain"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import * as path from "node:path"
import { Extraction, Tools } from "../deployment/index.js"
import * as Timing from "../timing.js"
import type { Source } from "./discover.js"
import * as Generation from "./generation.js"
import * as Record from "./record.js"

/**
 * The widest silence between two consecutive files of one tap: a wider one
 * means a file is missing between them, and the set is refused rather than
 * cut across the hole.
 */
export const SET_MAX_GAP_MS = 60_000

/**
 * Where an extraction keeps its raw flows document while the anonymiser reads
 * it: under the store's own `.scratch`, on the store's disk. A temp dir may be
 * RAM, and a big capture's raw document is near a gigabyte.
 */
export const SCRATCH = ".scratch"

/**
 * The raw document's name in the scratch: never `*.flows.json`, so a sweep
 * walking the store for flows documents cannot read a raw one.
 */
export const rawFlowsName = (capture: string): string => `${capture}.raw-flows.tmp`

/** What a failed child said, as one line: room for a set refusal naming two paths. */
const lineOf = (stderr: string): string => stderr.slice(0, 600).replace(/["\n]/g, " ").trim()

/** The source under the generation its capture stands at in the store. */
export const standing = Effect.fn("Extract.standing")(function*(artifacts: string, source: Source) {
  const first = source.first ?? source.capture
  const at = yield* Generation.current(artifacts, first)
  return at.generation === 1 ? { ...source, capture: first } : { ...source, capture: at.capture, first }
})

/**
 * What one extraction came to: the capture name it landed under, the source
 * under that name, why it landed there, and why it failed where it did.
 */
export interface Extracted {
  readonly capture: string
  readonly source: Source
  readonly landed: string
  readonly broke: string | undefined
}

/** One extraction's clock: the whole of it, split between the read, the anonymisation and the index. */
export interface ExtractTiming {
  readonly capture: string
  readonly ms: number
  readonly steps: Readonly<Record<string, number>>
  readonly broke: string | undefined
}

/**
 * Whether a capture name is settled: a citation names one of its ids or a
 * real-clock lane ran it, so its bytes may never change under it. A name that
 * is not is extracted again in place.
 */
export type Settled = (capture: string) => boolean

/** Every name settled: what an extraction assumes where nobody says otherwise. */
export const EVERY_NAME_SETTLED: Settled = () => true

/** What an extraction reaches: the platform, the deployment's extractor and anonymiser. */
export type Requirements =
  | FileSystem.FileSystem
  | ChildProcessSpawner.ChildProcessSpawner
  | Tools.Service
  | Extraction.Service

/**
 * One capture extracted into the artifact store. The raw flows document lives
 * in a scratch file for the length of the anonymisation and is deleted with
 * it, whatever happens — only anonymised data persists. `extractor` is the
 * fingerprint the record keeps, `tool` the extractor binary, brought up to
 * date by the caller. `observe` hears how
 * long it took, step by step.
 */
export const extract = Effect.fn("Extract.extract")(function*(
  artifacts: string,
  source: Source,
  extractor: string,
  tool: string,
  observe?: (timing: ExtractTiming) => void,
  settled: Settled = EVERY_NAME_SETTLED
) {
  const watch = Timing.stopwatch()
  const done = yield* extractTimed(artifacts, source, extractor, tool, watch, settled)
  observe?.({ capture: done.capture, ms: watch.total(), steps: watch.steps(), broke: done.broke })
  return done
})

/**
 * A fresh scratch directory for one extraction, beside the store it extracts
 * into: a raw flows document is the capture before anonymisation, so it never
 * lands under the shared temp dir.
 */
export const scratchFor = (fs: FileSystem.FileSystem, artifacts: string) =>
  Effect.gen(function*() {
    const root = path.join(artifacts, SCRATCH)
    yield* fs.makeDirectory(root, { recursive: true })
    return yield* fs.makeTempDirectory({ directory: root, prefix: "ingest-" })
  })

/** Whether two files hold the same bytes. */
const sameBytes = (fs: FileSystem.FileSystem, a: string, b: string) =>
  Effect.gen(function*() {
    if (!(yield* fs.exists(a)) || !(yield* fs.exists(b))) return false
    return (yield* Record.sha256(a)) === (yield* Record.sha256(b))
  })

/**
 * Where one extraction lands: a capture the store never held, or whose last
 * extraction left no record, under its name; one whose last extraction failed
 * and left no anonymised bytes under its name, there, with the seed that run
 * kept or a fresh one; one that is not {@link Settled} — no runs, no cited ids
 * — there too, whatever its bytes come out as; one whose kept seed is still
 * kept, under its name when the anonymised bytes come out the same and under
 * the next generation when they do not; one the store holds without a kept
 * seed under the next generation, with a fresh seed. A new generation never
 * touches the old name's rows, artifacts or runs; its seed is kept under its
 * own name (the kept seed carried where only the bytes changed). A
 * re-extraction under a kept seed that fails leaves the artifacts it would
 * have replaced as they were.
 */
const extractTimed = (
  artifacts: string,
  given: Source,
  extractor: string,
  tool: string,
  watch: Timing.Stopwatch,
  settled: Settled
): Effect.Effect<Extracted, never, Requirements> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const extraction = yield* Extraction.Service
    const source = yield* standing(artifacts, given)
    const first = source.first ?? source.capture
    const at = yield* Generation.current(artifacts, first)
    const record = yield* Record.read(artifacts, source.capture)
    const kept = record?.seedFile !== undefined &&
      (yield* extraction.keptSeed(artifacts, source.capture)) !== undefined
    const scratch = yield* scratchFor(fs, artifacts)
    const raw = path.join(scratch, rawFlowsName(source.capture))
    const landing = (generation: Generation.Generation): Source =>
      generation.generation === 1
        ? { capture: first, pcaps: source.pcaps }
        : { capture: generation.capture, pcaps: source.pcaps, first }
    const dir = Record.captureDir(artifacts, source.capture)
    const document = yield* extraction.documentName(source.capture)
    const holdsBytes = yield* fs.exists(path.join(dir, document))
    const open = !settled(source.capture)
    const into = (to: Source, why: string, follows: string | undefined, done: Done = { _tag: "none" }) =>
      landInto(artifacts, to, extractor, tool, watch, raw, why, follows, done)
    const run = Effect.gen(function*() {
      if (record !== null && (yield* extraction.failureOf(dir, source.capture)) !== undefined && !holdsBytes) {
        return yield* into(source, "the last extraction failed: no anonymised bytes stand under this name", undefined)
      }
      if (record !== null && !kept && open) {
        const why = `${source.capture} keeps no seed and no cited id or real-lane run pins it: extracted again in place`
        return yield* into(source, why, undefined)
      }
      if (record !== null && !kept) {
        const next = landing(yield* Generation.after(artifacts, first, at.generation))
        const why = record.seedFile === undefined
          ? `${source.capture} keeps no seed: a fresh seed is a new capture name`
          : `the seed kept for ${source.capture} is gone: a fresh seed is a new capture name`
        return yield* into(next, why, source.capture)
      }
      if (record === null) return yield* into(source, "first extraction", undefined)
      // A kept seed: anonymise beside the store and compare before landing.
      const staged = path.join(scratch, "staged")
      yield* fs.makeDirectory(staged)
      // A failed re-extraction leaves the artifacts it would have replaced as they were.
      const keptLine = `${source.capture}: kept seed, its artifacts left as they were`
      const read = yield* readRaw(tool, source, raw)
      if (read !== undefined) return { capture: source.capture, source, landed: keptLine, broke: read }
      watch.lap("sipflow")
      const anonymised = yield* extraction.anonymise({ raw, dir: staged, artifacts, capture: source.capture }).pipe(
        Effect.as(undefined),
        Effect.catchTag("Corpus.AnonymiseFailed", (failed) => Effect.succeed(failed.line))
      )
      watch.lap("auto")
      if (anonymised !== undefined) {
        return { capture: source.capture, source, landed: keptLine, broke: anonymised }
      }
      if (yield* sameBytes(fs, path.join(staged, document), path.join(dir, document))) {
        return yield* into(source, "kept seed, same anonymised bytes", undefined, { _tag: "staged", dir: staged })
      }
      if (open) {
        const why = `the anonymised bytes of ${source.capture} changed under its kept seed; no cited id or ` +
          "real-lane run pins it: extracted again in place"
        return yield* into(source, why, undefined, { _tag: "staged", dir: staged })
      }
      const next = landing(yield* Generation.after(artifacts, first, at.generation))
      yield* extraction.carrySeed(artifacts, source.capture, next.capture)
      return yield* into(next, `the anonymised bytes of ${source.capture} changed under its kept seed`, source.capture, {
        _tag: "read"
      })
    })
    return yield* run.pipe(
      Effect.catchCause((cause) =>
        Effect.succeed(
          {
            capture: source.capture,
            source,
            landed: "",
            broke: `the extraction died: ${messageOf(Cause.squash(cause))}`
          } satisfies Extracted
        )
      ),
      Effect.ensuring(fs.remove(scratch, { recursive: true, force: true }).pipe(Effect.ignore))
    )
  }).pipe(
    Effect.catchCause((cause) =>
      Effect.succeed(
        {
          capture: given.capture,
          source: given,
          landed: "",
          broke: `the extraction died: ${messageOf(Cause.squash(cause))}`
        } satisfies Extracted
      )
    )
  )

const messageOf = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause))

/** The raw flows document of a capture, into `raw`; why it could not be read, where it could not. */
const readRaw = (tool: string, source: Source, raw: string) =>
  Effect.gen(function*() {
    const ended = yield* IntoFile.exec(
      tool,
      [...source.pcaps, "--contiguous", String(SET_MAX_GAP_MS), "--json"],
      raw,
      { stderrHead: 600 }
    )
    return ended.broke === undefined && ended.exit === 0 ? undefined : `sipflow failed: ${lineOf(ended.stderr)}`
  })

/** How much of an extraction an earlier step did: nothing, the raw read, or a whole staged anonymisation. */
type Done = { readonly _tag: "none" } | { readonly _tag: "read" } | { readonly _tag: "staged"; readonly dir: string }

/**
 * One extraction under `source`'s name: the raw document read (unless `done`
 * says it is), anonymised into the capture's directory (or the files a staged
 * anonymisation left moved there), indexed, and recorded with its generation
 * and the name it follows.
 */
const landInto = (
  artifacts: string,
  source: Source,
  extractor: string,
  tool: string,
  watch: Timing.Stopwatch,
  raw: string,
  why: string,
  follows: string | undefined,
  done: Done
) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const extraction = yield* Extraction.Service
    const dir = Record.captureDir(artifacts, source.capture)
    yield* fs.makeDirectory(dir, { recursive: true })
    // The old record goes first: an extraction that dies short of writing its own
    // must read as never extracted, not as the artifacts it was replacing.
    yield* fs.remove(path.join(dir, Record.INGEST), { force: true })
    const landed = follows === undefined ? `${source.capture}: ${why}` : `${follows} → ${source.capture}: ${why}`
    const record = Effect.gen(function*() {
      const generation = yield* Generation.current(artifacts, source.first ?? source.capture)
      const seedFile = yield* extraction.keptSeed(artifacts, source.capture)
      yield* Record.write(
        artifacts,
        yield* Record.recordOf(source, extractor, { seedFile, generation: generation.generation, follows })
      )
    })
    const fail = (broke: string) =>
      Effect.gen(function*() {
        yield* extraction.recordFailure(dir, source.capture, broke)
        yield* record
        return { capture: source.capture, source, landed, broke } satisfies Extracted
      })
    if (done._tag === "none") {
      const read = yield* readRaw(tool, source, raw)
      if (read !== undefined) return yield* fail(read)
      watch.lap("sipflow")
    }
    if (done._tag === "staged") {
      for (const file of yield* fs.readDirectory(done.dir)) {
        yield* fs.rename(path.join(done.dir, file), path.join(dir, file))
      }
    } else {
      const anonymised = yield* extraction.anonymise({ raw, dir, artifacts, capture: source.capture }).pipe(
        Effect.as(undefined),
        Effect.catchTag("Corpus.AnonymiseFailed", (failed) => Effect.succeed(failed.line))
      )
      if (anonymised !== undefined) return yield* fail(anonymised)
      watch.lap("auto")
    }
    const run = yield* runner
    const document = yield* extraction.documentName(source.capture)
    const indexed = yield* run.all(tool, ["index", path.join(dir, document)]).pipe(
      Effect.map((output) => (output.exitCode === 0 ? undefined : lineOf(output.stderr))),
      Effect.catch(() => Effect.succeed(""))
    )
    if (indexed !== undefined) return yield* fail(`sipflow index failed: ${indexed}`)
    watch.lap("index")
    yield* record
    watch.lap("record")
    return { capture: source.capture, source, landed, broke: undefined } satisfies Extracted
  })

/**
 * Every named capture extracted, the extractor brought up to date first.
 * Concurrent because an extraction is a short child and a corpus is thousands
 * of them; each capture stands alone, so one that breaks is a line and a count
 * rather than the end of the phase. `observe` hears each extraction's timing;
 * `settled` says which names a citation or a real-clock run pins.
 */
export const phase = Effect.fn("Extract.phase")(function*(
  sources: ReadonlyArray<Source>,
  artifacts: string,
  extractor: string,
  concurrency: number,
  observe?: (timing: ExtractTiming) => void,
  settled: Settled = EVERY_NAME_SETTLED
) {
  if (sources.length === 0) return [] as ReadonlyArray<Extracted>
  const tool = yield* Effect.flatMap(Tools.Service, (tools) => tools.ready("extractor")).pipe(Effect.orDie)
  const fs = yield* FileSystem.FileSystem
  yield* fs.makeDirectory(artifacts, { recursive: true }).pipe(Effect.orDie)
  return yield* Effect.forEach(sources, (source) => extract(artifacts, source, extractor, tool, observe, settled), {
    concurrency
  })
})
