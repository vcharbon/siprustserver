/**
 * The CUT phase: raw corpus artifacts to the cut-store rows a replay is planned
 * from.
 *
 * Enrich every capture, cut them all in ONE generator run into a SCRATCH
 * directory the cut owns, mirror what that produced into the store
 * (`cut-rows.ts`), and drop the scratch. What the cut leaves behind is the
 * rows, its own log under `_preps/prep<N>/`, and each capture's
 * `excluded.json` — which is the whole content of a `no-cases` row. Each batch
 * prints one `[timing]` line per step: `cut.enrich`, `cut.generate`,
 * `cut.mirror`.
 *
 * Everything the store holds is content-named and every prep number comes from
 * a sequence, so two cuts of one capture cannot rewrite each other's work and
 * the phase takes no lock.
 *
 * Nothing here knows a lane. What comes out is lane-free by construction,
 * which is what lets every lane replay it at once afterwards.
 *
 * A capture that breaks breaks ALONE: it is recorded with the reason, and the
 * captures cut beside it stand. A capture the batch cut never reached is
 * recorded as an error rather than left to read as a cut that produced nothing.
 */
import type { ExclusionRecord } from "@sip/pipeline/exclusion-record"
import { CaseIds, Cuts } from "@sip/store"
import * as Cause from "effect/Cause"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { Launcher, Tools } from "../deployment/index.js"
import * as Fingerprint from "../fingerprint.js"
import type { Options } from "../options.js"
import { prepDir, stemOf } from "../paths.js"
import * as Provenance from "../provenance.js"
import * as Timing from "../timing.js"
import { documentFile, egressOf, readDocument } from "./case-dir.js"
import * as Census from "./census.js"
import { type CaptureWork, type CutCase, type CutReading, decodeCaptureCut, NAMES_FILE } from "./cut-contract.js"
import * as CutRows from "./cut-rows.js"
import { enrichAll, flowsOf, workOf } from "./enrich.js"
import { EXCLUDED } from "./excluded.js"
import * as Generator from "./generator.js"

/**
 * What a cut that never started says on the captures it was given.
 *
 * A batch that recorded NO capture at all died before its first one, so the
 * reason is the BATCH's and the captures are only its bystanders. They still
 * carry an outcome — silence is never one here — but the cause is stated once,
 * at the phase, instead of the same stack tail copied onto a thousand rows.
 */
const DID_NOT_START = "the cut did not start"

/** One capture's place in a cut: the workdir its refusals are left in. */
export interface CaptureSlot {
  readonly capture: string
  /** `<sessions>/<stem>` — holds this capture's `run<id>` bundles and its refusals. */
  readonly sessionDir: string
}

export interface CutRequest {
  readonly slots: ReadonlyArray<CaptureSlot>
  /** Where the un-enriched flows documents are read from. */
  readonly corpus: string
  /** This cut's number, and the directory its work list and log go in. */
  readonly prep: number
  readonly prepDir: string
  /** Which slice of a multi-batch cut this is; it names the batch's work list. */
  readonly part: number
  /** How many captures are enriched at once. */
  readonly concurrency: number
  /** The memory box of the generator. */
  readonly memoryMax: string
  /** The census run the cut declares must-fail steps from (`census.ts`). */
  readonly census: string
}

/**
 * What the batch cut says about each capture it was given. A capture the cut
 * never reached has NO entry — and that is the point. `tail` is the dead cut's
 * own last words, for the readings that then have nothing else to explain them.
 */
interface CutReadings {
  readonly cut: ReadonlyMap<string, string | undefined>
  readonly tail: string
}

/** The cut of every enriched capture, in one generator run, into each one's own scratch. */
const cutAll = (
  work: ReadonlyArray<CaptureWork>,
  prepDir: string,
  part: number,
  memoryMax: string,
  census: string
) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const listFile = path.join(prepDir, `work-list-${part}.json`)
    yield* fs.writeFileString(listFile, `${JSON.stringify(work, null, 2)}\n`)
    const manifest = path.join(prepDir, `cut-manifest-${part}.ndjson`)
    // The census run is the one the fingerprint hashed, named here so the cut
    // and its fingerprint cannot read two different runs.
    const ended = yield* Generator.run({
      args: ["--work-list", listFile, "--manifest", manifest, "--census-report", census],
      memoryMax,
      log: path.join(prepDir, Generator.GENERATOR_LOG)
    })
    const cut = new Map<string, string | undefined>()
    const lines = yield* fs.readFileString(manifest).pipe(Effect.orElseSucceed(() => ""))
    for (const line of lines.split("\n")) {
      if (line.trim() === "") continue
      // A torn last line is a cut that died mid-write: that capture is
      // unreached, which is exactly what dropping the line records.
      try {
        const entry = decodeCaptureCut(JSON.parse(line) as unknown)
        cut.set(entry.capture, entry.cut ? undefined : entry.error ?? "the cut refused it")
      } catch {
        continue
      }
    }
    return { cut, tail: ended.said } satisfies CutReadings
  })

/**
 * The case ids one capture's cut produced, read for the endpoint their egress
 * leaves by. The directories are the scratch's, and live only until the rows
 * are written.
 */
const casesOf = (outDir: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const casesDir = path.join(outDir, "cases")
    const entries = yield* fs.readDirectory(casesDir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))
    const ids: Array<string> = []
    for (const entry of entries) {
      if (yield* fs.exists(path.join(casesDir, entry, documentFile(entry)))) ids.push(entry)
    }
    ids.sort()
    const cases: Array<CutCase> = []
    for (const id of ids) {
      const dir = path.join(casesDir, id)
      const egress = egressOf(yield* readDocument(path.join(dir, documentFile(id))))
      cases.push({ id, dir, ...(egress === undefined ? {} : { egressEndpoint: egress }) })
    }
    return cases as ReadonlyArray<CutCase>
  })

/**
 * What this cut refused, kept where a reader of the session tree finds it: a
 * `no-cases` row's reason is the whole content of that row, and the cases it
 * belongs to are in the store rather than on the tree. A cut that refused
 * nothing takes the last cut's answer away with it.
 */
const keepRefusals = (slot: CaptureSlot, outDir: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const target = path.join(slot.sessionDir, slot.capture, EXCLUDED)
    const written = path.join(outDir, EXCLUDED)
    if (!(yield* fs.exists(written))) return yield* fs.remove(target, { force: true })
    yield* fs.makeDirectory(path.dirname(target), { recursive: true })
    yield* fs.copyFile(written, target)
  })

/**
 * The source tier's refusal of the whole capture, kept where the generator's
 * would have been: the same file, one record, so the `no-cases` row and the
 * store's refusal rows read it the same way. Answers where it was written.
 */
const keepSourceRefusal = (slot: CaptureSlot, record: ExclusionRecord) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const target = path.join(slot.sessionDir, slot.capture, EXCLUDED)
    yield* fs.makeDirectory(path.dirname(target), { recursive: true })
    yield* fs.writeFileString(target, `${JSON.stringify([record], null, 2)}\n`)
    return target
  })

/** What one batch of the cut produced: every capture's reading, and its rows. */
export interface CutBatch {
  readonly readings: ReadonlyMap<string, CutReading>
  readonly mirrored: CutRows.Mirrored
  /** Why the batch never started, where it did not — see {@link DID_NOT_START}. */
  readonly stalled?: string
}

const messageOf = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause))

/**
 * Every capture of one batch cut and recorded, in the scratch its caller owns.
 * The map is the same readings the rows hold, so a caller can report without
 * reading the store back.
 */
const cutInto = Effect.fn("Cut.cutInto")(function*(request: CutRequest, scratch: string) {
  const cutAt = new Date().toISOString()
  const code = yield* Provenance.current
  const cutFingerprint = yield* Fingerprint.cut
  const work = new Map(request.slots.map((slot) => [slot.capture, workOf(scratch, slot.capture)] as const))
  const readings = new Map<string, CutReading>()
  const record = (
    slot: CaptureSlot,
    rest: Pick<CutReading, "cases"> & { broke?: string; excluded?: string }
  ) =>
    Effect.gen(function*() {
      const at = workOf(scratch, slot.capture)
      readings.set(slot.capture, {
        capture: slot.capture,
        prep: request.prep,
        cutAt,
        flows: yield* flowsOf(at),
        provenance: code,
        cutFingerprint,
        excluded: path.join(at.outDir, EXCLUDED),
        names: path.join(at.outDir, NAMES_FILE),
        ...rest
      })
    })
  const failed = (slot: CaptureSlot, broke: string) =>
    record(slot, { broke: `[prep${request.prep}] ${broke}`, cases: [] })

  for (const [capture, why] of yield* seedRefusals([...work.keys()])) {
    yield* failed(request.slots.find((slot) => slot.capture === capture)!, why)
    work.delete(capture)
  }

  const batch = { prep: request.prep, part: request.part }
  const enrichClock = Timing.stopwatch()
  const enrichFailures = yield* enrichAll([...work.values()], request.corpus, request.concurrency)
  Timing.emit({ phase: "cut.enrich", ...batch, captures: work.size, ms: enrichClock.total() })
  for (const slot of request.slots) {
    const why = enrichFailures.get(slot.capture)
    if (why === undefined) continue
    if (why.kind === "broke") {
      yield* failed(slot, why.why)
      continue
    }
    // A source the ingest could not read is refused whole, as a case is: the
    // record beside the cases it has none of, and a reading with no cases.
    yield* Console.error(`[cut] ${why.record.line}`)
    yield* record(slot, { cases: [], excluded: yield* keepSourceRefusal(slot, why.record) })
  }
  const enriched = request.slots.filter((slot) => work.has(slot.capture) && !enrichFailures.has(slot.capture))

  // Every call the store already names keeps its name: the frozen ids travel
  // with each capture's work entry, and the cut names a call by them first.
  const frozen = yield* Effect.flatMap(CaseIds.Service, (ids) => ids.of(enriched.map((slot) => slot.capture)))
  const generateClock = Timing.stopwatch()
  const reading = enriched.length === 0
    ? { cut: new Map<string, string | undefined>(), tail: "" }
    : yield* cutAll(
      enriched.map((slot) => ({ ...work.get(slot.capture)!, frozen: frozen.get(slot.capture) ?? [] })),
      request.prepDir,
      request.part,
      request.memoryMax,
      request.census
    )
  Timing.emit({ phase: "cut.generate", ...batch, captures: enriched.length, ms: generateClock.total() })

  const stalled = enriched.length > 0 && reading.cut.size === 0
    ? `[prep${request.prep}] ${DID_NOT_START} over ${enriched.length} capture(s) — ${reading.tail}`
    : undefined
  for (const slot of enriched) {
    if (!reading.cut.has(slot.capture)) {
      yield* failed(
        slot,
        stalled === undefined ? `the batch cut never reached this capture — ${reading.tail}` : DID_NOT_START
      )
      continue
    }
    const refusal = reading.cut.get(slot.capture)
    if (refusal !== undefined) {
      yield* failed(slot, `the cut failed: ${refusal}`)
      continue
    }
    // One malformed case file is that capture's problem: the cut behind it
    // still lands.
    const outDir = work.get(slot.capture)!.outDir
    const read = yield* Effect.gen(function*() {
      yield* keepRefusals(slot, outDir)
      return yield* casesOf(outDir)
    }).pipe(
      Effect.map((cases) => ({ cases })),
      Effect.catchCause((cause) => Effect.succeed({ broke: messageOf(Cause.squash(cause)) }))
    )
    if ("broke" in read) yield* failed(slot, `reading the cut's cases failed: ${read.broke}`)
    else yield* record(slot, { cases: read.cases })
  }

  const mirrorClock = Timing.stopwatch()
  const mirrored = yield* CutRows.mirror(request.prep, readings)
  Timing.emit({
    phase: "cut.mirror",
    ...batch,
    captures: readings.size,
    ms: mirrorClock.total(),
    maxRssKb: Timing.selfPeakKb()
  })
  return {
    readings: readings as ReadonlyMap<string, CutReading>,
    mirrored,
    ...(stalled === undefined ? {} : { stalled })
  } satisfies CutBatch
})

/**
 * One batch, cut in a scratch directory that goes whatever happens: what the cut
 * produced lives in the store from the moment `mirror` returns, and a capture
 * whose rows the store refused stands at whatever cut it stood at before.
 */
export const run = Effect.fn("Cut.run")(function*(request: CutRequest) {
  const fs = yield* FileSystem.FileSystem
  const scratch = path.join(request.prepDir, `work-${request.part}`)
  const drop = fs.remove(scratch, { recursive: true, force: true }).pipe(Effect.ignore)
  yield* fs.makeDirectory(request.prepDir, { recursive: true })
  yield* drop
  return yield* Effect.ensuring(cutInto(request, scratch), drop)
})

/** What a cut invocation produced: its number, every capture's reading, and its rows. */
export interface Cut {
  readonly prep: number
  readonly readings: ReadonlyMap<string, CutReading>
  readonly mirrored: CutRows.Mirrored
}

/** One capture's reading as the per-capture line states it: cut, broke, or refused whole. */
export const readingLine = (reading: CutReading): string =>
  reading.broke !== undefined
    ? `[cut] prep${reading.prep} ${reading.capture}: broke — ${reading.broke}`
    : reading.cases.length === 0
    ? `[cut] prep${reading.prep} ${reading.capture}: refused-all`
    : `[cut] prep${reading.prep} ${reading.capture}: cut (${reading.cases.length} case(s))`

/**
 * The captures of `captures` a cut refuses, each with its line: those read
 * before their ids were frozen and not seeded since. A name the cut made up
 * for one of their calls would otherwise take that call for good.
 */
export const seedRefusals = Effect.fn("Cut.seedRefusals")(function*(captures: ReadonlyArray<string>) {
  const unseeded = yield* Effect.flatMap(CaseIds.Service, (ids) => ids.unseeded(captures))
  const seed = (yield* Launcher.Service).seedIds
  return new Map(unseeded.map((capture) => [
    capture,
    `${capture}: its earlier readings' ids are not frozen — run ${seed} first`
  ] as const))
})

/** A cut `--strict-census` stopped before it opened a prep: the census run does not cover every capture. */
export class CensusRefused extends Schema.TaggedError<CensusRefused>()("Corpus.CensusRefused", {
  reason: Schema.String
}) {
  override get message(): string {
    return this.reason
  }
}

/** The census run a cut reads. Every command line that cuts refuses first (`Census.refusal`). */
const censusRun = (options: Options) =>
  Effect.flatMap(Census.latest(options.paths.census), (census) =>
    census === undefined
      ? Effect.die(new Error(`no census run at ${options.paths.census}`))
      : Effect.succeed(census))

/**
 * The census gate of one whole cut, before its prep opens and before anything
 * is enriched: the coverage line, printed once, and the run's path. Under
 * `--strict-census` a capture the run did not sweep refuses the cut, and an
 * unreadable run refuses it whatever the flag.
 */
export const censusGate = Effect.fn("Cut.censusGate")(function*(captures: ReadonlyArray<string>, options: Options) {
  const census = yield* censusRun(options)
  const swept = yield* Census.sweptCaptures(census)
  if ("unreadable" in swept) return yield* new CensusRefused({ reason: swept.unreadable })
  const coverage = Census.captureCoverage(captures, swept)
  yield* Console.log(`[cut] ${Census.coverageLine(coverage)} (census ${path.basename(census)})`)
  if (options.strictCensus === true && coverage.uncovered.length > 0) {
    return yield* new CensusRefused({
      reason: `--strict-census: the census run ${census} does not cover ${coverage.uncovered.length} ` +
        `capture(s); sweep them, or cut without --strict-census`
    })
  }
  return census
})

/**
 * The whole cut phase: the extractor brought up to date, then every capture
 * cut and recorded, one batch at a time, each capture reported as its batch
 * lands.
 *
 * The prep number is the store-assigned one, so two cuts can never pick the
 * same one and the `_preps/prep<N>` log directory carries the id the rows
 * carry. A caller that opened its own prep hands it in and finishes it
 * itself; the one that does not gets a prep opened and finished here, under
 * `jobId` where the cut runs as a job.
 *
 * The census gate ({@link censusGate}) runs here before that prep opens; a
 * caller that opens its own prep runs the gate itself first, once for its
 * whole cut.
 */
export const phase = Effect.fn("Cut.phase")(function*(
  captures: ReadonlyArray<string>,
  options: Options,
  standing?: number,
  jobId?: number
) {
  const cuts = yield* Cuts.Service
  const census = standing === undefined ? yield* censusGate(captures, options) : yield* censusRun(options)
  const prep = standing ?? (yield* cuts.open(yield* Provenance.current, yield* Fingerprint.cut, jobId))
  const logs = prepDir(options.paths.sessions, prep)
  yield* Console.log(
    `[cut] prep${prep}: ${captures.length} capture(s) in ` +
      `${Math.ceil(captures.length / options.batchSize)} batch(es) of ${options.batchSize} -> ${logs}` +
      ` (census ${path.basename(census)})`
  )
  yield* Effect.flatMap(Tools.Service, (tools) => tools.ready("extractor")).pipe(Effect.orDie)
  const readings = new Map<string, CutReading>()
  const stalls: Array<string> = []
  let mirrored = CutRows.NOTHING_MIRRORED
  for (let at = 0; at < captures.length; at += options.batchSize) {
    const part = yield* run({
      slots: captures.slice(at, at + options.batchSize).map((capture) => ({
        capture,
        sessionDir: path.join(options.paths.sessions, stemOf(capture))
      })),
      corpus: options.paths.corpus,
      prep,
      prepDir: logs,
      part: at / options.batchSize + 1,
      concurrency: options.concurrency,
      memoryMax: options.memoryMax,
      census
    })
    for (const [capture, reading] of part.readings) {
      readings.set(capture, reading)
      yield* Console.log(readingLine(reading))
    }
    if (part.stalled !== undefined) stalls.push(part.stalled)
    mirrored = CutRows.plus(mirrored, part.mirrored)
  }
  const broke = [...readings.values()].filter((reading) => reading.broke !== undefined)
  const empty = [...readings.values()].filter((r) => r.broke === undefined && r.cases.length === 0)
  yield* Console.log(
    `[cut] prep${prep} done: cut=${readings.size - broke.length - empty.length} ` +
      `no-cases=${empty.length} errors=${broke.length} ` +
      `rows=${mirrored.recorded} rows-failed=${mirrored.failed}`
  )
  // The free correctness check: a re-cut of unchanged captures must AGREE with
  // the documents the replays already stored, so almost every case is known.
  yield* Console.log(
    `[cut] prep${prep} cases=${mirrored.cases} known=${mirrored.cases - mirrored.fresh} new=${mirrored.fresh}`
  )
  for (const stall of stalls) yield* Console.error(`   ! ${stall}`)
  for (const reading of broke) {
    if (reading.broke?.endsWith(DID_NOT_START) === true) continue
    yield* Console.error(`   ! ${reading.capture}  ${reading.broke}`)
  }
  if (standing === undefined) yield* cuts.finish(prep)
  return { prep, readings, mirrored } satisfies Cut
})
