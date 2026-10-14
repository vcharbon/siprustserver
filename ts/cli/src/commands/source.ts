/**
 * `source` — SOURCES from the command line:
 *
 *   source add <path>... [--paths-from FILE|-] [--set <file>]... [--cut] [--concurrency N] [--memory-max SIZE]
 *   source add --job <id> …          (what the launched ingest job runs)
 *   source list [--stale] [-q NAME] [--limit N] [--offset N]
 *
 * `add` takes paths on the host — a file, a directory, or with `--set` the
 * consecutive files of ONE capture — under the roots the host names
 * (`@sip/corpus` `source.ts`), resolves them against the operator's own
 * directory, and launches an `ingest` job over them (`job-start.ts`): the
 * campaign's discover, extract and population phases, idempotent on the
 * pcap's fingerprint, then with `--cut` the cut over whatever those captures
 * left stale. "Add" and "add + cut" are the two shapes; a replay after them is
 * a request. A path outside the roots, or one naming nothing, is refused
 * before any row exists. A selection of thousands of files is a list file
 * (`--paths-from`), never a shell glob: a package runner hands its script the
 * whole line as one argument, and past the kernel's 128 KiB per argument the
 * spawn dies (E2BIG) before this runs. A directory is the other way. The job
 * row records the inputs as named and the list by its snapshot
 * (`Source.readPathList`), which the launched job reads.
 *
 * `list` is the source list: per capture its ingest (current, stale, or the
 * failure), its standing prep with case and refusal counts and longest call,
 * and its standing on every lane — one statement, paged and counted.
 */
import { phase as cutPhase } from "@sip/corpus/cut/cut"
import { cutStaleness } from "@sip/corpus/cut/cut-staleness"
import * as Fingerprint from "@sip/corpus/fingerprint"
import * as Discover from "@sip/corpus/ingest/discover"
import * as Source from "@sip/corpus/ingest/source"
import * as Job from "@sip/corpus/jobs/job"
import * as JobStart from "@sip/corpus/jobs/job-start"
import { DEFAULT_BATCH, DEFAULT_CONCURRENCY, DEFAULT_MEMORY_MAX } from "@sip/corpus/knobs"
import * as Locks from "@sip/corpus/locks"
import type { Options } from "@sip/corpus/options"
import type { Paths } from "@sip/corpus/paths"
import { standing } from "@sip/corpus/universe"
import { Jobs, Sources } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import { Argument, Command, Flag } from "effect/unstable/cli"
import * as path from "node:path"
import type { CliFacts } from "../facts.js"
import { pathFlags } from "../flags.js"
import * as IdFlags from "../id-flags.js"
import { overridesOf, pathsOf } from "../options.js"
import * as Refusals from "../refusals.js"
import { exit, operatorCwd, stdinText } from "../runner.js"

const usage = (facts: CliFacts): string =>
  `usage: ${facts.invocation("source")} add <path>... [--set <file>]... [--cut] | ` +
  "list [--stale] [-q NAME] [--limit N] [--offset N]"

export const addFlags = (facts: CliFacts) => ({
  paths: Argument.string("path").pipe(
    Argument.withDescription("A pcap, or a directory of pcaps, one capture each"),
    Argument.variadic()
  ),
  set: Flag.string("set").pipe(
    Flag.withDescription("A file of ONE capture split across consecutive files (or a directory of them); repeatable"),
    Flag.atLeast(0)
  ),
  pathsFrom: Flag.string("paths-from").pipe(
    Flag.withDescription(
      "A file of pcap paths, one per line, relative to the file (- reads them from stdin): a selection too long " +
        "for a shell glob, which a package runner refuses past 128 KiB of paths (E2BIG)"
    ),
    Flag.optional
  ),
  cut: Flag.boolean("cut").pipe(
    Flag.withDescription("Cut what the ingest left stale once it has landed"),
    Flag.withDefault(false)
  ),
  concurrency: Flag.integer("concurrency").pipe(
    Flag.withDescription("Extractions at once, and captures enriched at once by the cut"),
    Flag.withDefault(DEFAULT_CONCURRENCY)
  ),
  memoryMax: Flag.string("memory-max").pipe(
    Flag.withDescription("The memory scope the cut runs under"),
    Flag.withDefault(DEFAULT_MEMORY_MAX)
  ),
  batch: Flag.integer("batch").pipe(Flag.withDescription("Captures per cut batch"), Flag.withDefault(DEFAULT_BATCH)),
  job: IdFlags.flag("job", "job").pipe(
    Flag.withDescription(`The job row this process was launched for (${facts.invocation("source")} add)`),
    Flag.optional
  )
})

type AddInput = Command.Command.Config.Infer<ReturnType<typeof addFlags>>

/** The inputs as the operator stated them, resolved against the shell's directory. */
const absolute = (inputs: ReadonlyArray<string>, cwd: string): ReadonlyArray<string> =>
  inputs.map((input) => path.resolve(cwd, input))

/** What the inputs come to, or the line that refuses them. */
const discovered = Effect.fn("SourceCli.discovered")(function*(
  paths: ReadonlyArray<string>,
  set: ReadonlyArray<string>,
  cwd: string
) {
  const found = Discover.join(yield* Discover.discover(paths, cwd), yield* Discover.set(set, cwd))
  if (found.missing.length > 0) return found.missing.join("; ")
  if (found.sources.length === 0) return "the paths named no pcap"
  return found
})

/** The cut's options for an ingest job that cuts: the store's roots, the batch shape, nothing selected. */
const cutOptions = (facts: CliFacts, paths: Paths, input: AddInput, captures: ReadonlyArray<string>): Options => ({
  paths,
  lane: facts.defaultLane,
  selection: {
    universe: "named",
    positional: captures,
    onlyNew: false,
    maxSpanSeconds: undefined,
    greenOnLane: undefined,
    sampleSize: undefined,
    limit: undefined,
    cases: []
  },
  batchSize: Math.max(1, input.batch),
  concurrency: Math.max(1, input.concurrency),
  startsPerSecond: 0,
  memoryMax: input.memoryMax,
  patience: { waitMs: Locks.DEFAULT_WAIT_MS },
  dryRun: false,
  prep: undefined,
  cleanup: false,
  forceRecut: false
})

/**
 * The ingest as the job (`--job`): every source extracted where stale and
 * recorded, one line each; then, with `--cut`, the captures the cut rule
 * finds stale — the ones just extracted first among them — cut under a prep
 * of this job's own. The row closes with the exit code: 1 where a capture's
 * ingest or cut broke, 0 otherwise.
 */
const runAsJob = Effect.fn("Source.runAsJob")(function*(
  facts: CliFacts,
  input: AddInput,
  job: number,
  found: Discover.Discovered,
  paths: Paths
) {
  const jobs = yield* Jobs.Service
  for (const said of found.collisions) yield* Console.error(`   ! ${said}`)
  yield* jobs.planned(job, found.sources.length)
  yield* standing
  const ingested = yield* Source.ingest(found.sources, paths.corpus, Math.max(1, input.concurrency))
  let broke = 0
  for (const source of ingested.sources) {
    const outcome = ingested.outcomes.get(source.capture) ?? { _tag: "current" as const }
    if (outcome._tag === "failed") broke += 1
    yield* Console.log(Source.line(source.capture, outcome))
  }
  yield* Console.log(
    `[source] ingest: ${ingested.extracted.size - broke} extracted, ${broke} refused, ` +
      `${found.sources.length - ingested.extracted.size} already current; +${ingested.added} new in the population`
  )
  if (!input.cut) return broke > 0 ? 1 : 0
  const captures = ingested.sources.map((source) => source.capture)
  const stale = yield* cutStaleness(captures, paths.corpus, ingested.extracted, false)
  // What the job decided with nothing to cut — standing current, or refused at
  // ingest — lands now: its progress reads N/N once the stale ones are cut.
  yield* jobs.settle(job, found.sources.length - stale.length)
  if (stale.length === 0) {
    yield* Console.log("[source] cut: every capture stands cut on this code")
    return broke > 0 ? 1 : 0
  }
  for (const one of stale) yield* Console.log(`[source] cut  ${one.capture}  ${one.why}`)
  const options = cutOptions(facts, paths, input, captures)
  const cut = yield* cutPhase(stale.map((one) => one.capture), options, undefined, job)
  const cutBroke = [...cut.readings.values()].some((reading) => reading.broke !== undefined)
  return broke > 0 || cutBroke || cut.mirrored.failed > 0 ? 1 : 0
})

const listFlags = {
  stale: Flag.boolean("stale").pipe(
    Flag.withDescription("Only the captures that need ingest or need cut"),
    Flag.withDefault(false)
  ),
  q: Flag.string("query").pipe(Flag.withAlias("q"), Flag.withDescription("A capture-name substring"), Flag.optional),
  limit: Flag.integer("limit").pipe(Flag.withDescription("Rows per page"), Flag.withDefault(50)),
  offset: Flag.integer("offset").pipe(Flag.withDescription("Rows to skip"), Flag.withDefault(0))
}

const instant = (value: string | null): string => (value === null ? "-" : value.replace(/\.\d{3}Z$/, "Z"))

/** One capture's standing on one lane: the status, the run, what failed. */
const standingOf = (one: Sources.LaneStanding): string =>
  `${one.lane}=${one.status}(run${one.runId}${one.failed > 0 ? `, ${one.failed} failed` : ""})`

/** One source as the list prints it. */
export const sourceLine = (row: Sources.Row): string =>
  [
    row.capture.padEnd(44),
    instant(row.ingestedAt).padEnd(20),
    (row.ingest === "failed" ? `failed: ${row.ingestFailed}` : row.ingest).padEnd(8),
    (row.prepId === null ? "-" : `prep${row.prepId}`).padEnd(8),
    (row.cut === "broke" ? `broke: ${row.cutBroke}` : row.cut).padEnd(8),
    String(row.cases).padStart(6),
    String(row.refusals).padStart(6),
    String(row.cases + row.refusals).padStart(6),
    (row.spanSeconds === null ? "-" : row.spanSeconds.toFixed(1)).padStart(8),
    row.standings.map(standingOf).join(" ")
  ].join("  ")

const HEADER = [
  "capture".padEnd(44),
  "ingested at".padEnd(20),
  "ingest".padEnd(8),
  "prep".padEnd(8),
  "cut".padEnd(8),
  " cases",
  "refusd",
  " calls",
  "  span s",
  "standing per lane"
].join("  ")

const list = Command.make("list", listFlags, (input) =>
  Effect.gen(function*() {
    const sources = yield* Sources.Service
    const extractor = yield* Fingerprint.extractor
    const cut = yield* Fingerprint.cut
    const page = yield* sources.list({
      stale: input.stale,
      ...(Option.isSome(input.q) ? { q: input.q.value } : {}),
      limit: Math.max(1, input.limit),
      offset: Math.max(0, input.offset),
      extractorHash: extractor,
      cutFingerprint: cut
    })
    yield* Console.log(HEADER)
    for (const row of page.rows) yield* Console.log(sourceLine(row))
    yield* Console.log(
      `[source] ${page.rows.length} of ${page.total} capture(s)` +
        (input.offset > 0 ? ` from ${input.offset}` : "") +
        (input.stale ? " needing ingest or cut" : "") +
        ` — extractor ${Fingerprint.short(extractor)}, cut ${Fingerprint.short(cut)}`
    )
  })).pipe(Command.withDescription("The source list: ingest, standing cut, counts, span and standing per lane"))

export const command = (facts: CliFacts) => {
  /** The root: the path flags every subcommand reads, and a usage line where none was named. */
  const root = Command.make("source", {}, () =>
    Effect.gen(function*() {
      yield* Console.error(usage(facts))
      yield* exit(2)
    })).pipe(
      Command.withDescription("Sources: add pcaps by path, list what the population holds"),
      Command.withSharedFlags(pathFlags)
    )

  const rootPaths = Effect.flatMap(root, (input) => pathsOf(overridesOf(input)))

  const add = Command.make("add", addFlags(facts), (input) =>
    Effect.gen(function*() {
      const paths = yield* rootPaths
      const cwd = operatorCwd()
      const stdin = stdinText.pipe(Effect.provideService(FileSystem.FileSystem, yield* FileSystem.FileSystem))
      const listed = Option.isNone(input.pathsFrom)
        ? undefined
        : yield* Source.readPathList(input.pathsFrom.value, cwd, Job.jobsDir(paths.sessions), stdin)
      if (typeof listed === "string") {
        yield* Console.error(`[source] refused: ${listed}`)
        return yield* exit(2)
      }
      const named = [...input.paths, ...(listed?.paths ?? [])]
      if (named.length === 0 && input.set.length === 0) {
        yield* Console.error(`[source] refused: no path named\n${usage(facts)}`)
        return yield* exit(2)
      }
      const outside = yield* Source.refusal([...named, ...input.set], cwd, paths)
      if (outside !== null) {
        yield* Console.error(`[source] refused: ${outside}`)
        return yield* exit(2)
      }
      const found = yield* discovered(named, input.set, cwd)
      if (typeof found === "string") {
        yield* Console.error(`[source] refused: ${found}`)
        return yield* exit(2)
      }
      const jobs = yield* Jobs.Service
      if (Option.isSome(input.job)) {
        const job = input.job.value
        if ((yield* jobs.show(job)) === null) {
          yield* Console.error(`[source] refused: there is no job ${job}`)
          return yield* exit(2)
        }
        const code = yield* runAsJob(facts, input, job, found, paths).pipe(
          Effect.tapCause(() => jobs.close(job, { status: "failed", exit: 1 }).pipe(Effect.ignore))
        )
        yield* jobs.close(job, { status: code === 0 ? "done" : "failed", exit: code })
        return yield* exit(code)
      }
      if (input.cut) {
        const refused = yield* Refusals.cut(facts, paths)
        if (refused !== null) {
          yield* Console.error(`[source] refused: ${refused}`)
          return yield* exit(2)
        }
      }
      for (const said of found.collisions) yield* Console.error(`   ! ${said}`)
      const started = yield* JobStart.ingest(paths.sessions, {
        paths: absolute(input.paths, cwd),
        ...(listed === undefined ? {} : { pathsFrom: listed.file }),
        set: absolute(input.set, cwd),
        captures: found.sources.map((source) => source.capture),
        cut: input.cut,
        concurrency: Math.max(1, input.concurrency),
        memoryMax: input.memoryMax
      }).pipe(Effect.catchIf(JobStart.isNotStarted, (refused) => Effect.succeed(refused)))
      if (started instanceof JobStart.NotStarted) {
        yield* Console.error(`[source] refused: ${started.message}`)
        return yield* exit(2)
      }
      const what = `${found.sources.length} source(s)${input.cut ? " + cut" : ""}`
      for (const line of JobStart.startedLines("ingest", what, started, facts.invocation("job"))) {
        yield* Console.log(line)
      }
    })).pipe(Command.withDescription("Add pcaps by path on the host: an ingest job, and with --cut the cut after it"))

  return root.pipe(Command.withSubcommands([add, list]))
}
