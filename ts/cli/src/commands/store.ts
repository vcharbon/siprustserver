/**
 * The run STORE, from the command line; with no verb it names every verb, one
 * line each:
 *
 *   migrate                      # apply the schema
 *   seed                         # import run bundles with no row yet
 *   rebuild                      # forget the session rows, then re-derive
 *   status [--lane L] [--limit N]  # what the queue would render
 *   sweeps [--limit N]           # the invocations, newest first
 *   jobs [--limit N]             # the jobs, newest first
 *   runs <selector>              # the runs a selector names, newest first
 *   compare <before> <after>     # what moved; a bare integer is a sweep here
 *   where <capture> [--prep N]   # every artifact of one capture
 *   case-doc <capture> <case> | <case id> [--prep N] [--print path|flow|ledger]
 *   delta --lane L [--against L]  # green there, red here
 *   rulehits [--limit N]         # count the rule hits of runs never measured
 *   calls-backfill [--lane L]    # measure what the per-call queue lacks
 *   pin <selector> [--ticket T] | unpin <selector> | pins  # runs kept by hand
 *   close-sweep <N>              # close a sweep whose process is gone
 *   close-prep <N>               # finish a prep whose cut is gone
 *
 * A deployment adds verbs of its own beside these (`command`'s `extra`).
 *
 * Seed and rebuild read nothing but the bundles: every fact the store holds
 * about a run is already in that run's own output, so a lost database costs a
 * command, not evidence. Seed is idempotent by bundle path — re-running it
 * imports only what arrived since.
 *
 * `calls-backfill` fills what the per-call queue reads and older rows lack:
 * for every run the queue reads (of the lane, or of every lane) whose failing
 * rows carry no main error and whose bundle survives, the error columns are
 * re-measured off the bundle — the verdict is never rewritten — and for every
 * capture whose standing cut did not break, the cut's `excluded.json` under
 * the session tree is read into its refusal rows, replacing what stood. A run
 * whose bundle is gone stays unmeasured: a measure nobody took is not a zero;
 * a cut with no record on disk keeps its rows.
 *
 * Import goes capture by capture in run order, because recency IS id order:
 * the latest run of a capture is its highest id. That is why an import never
 * keeps the id a directory name claims — `run11` importing before some other
 * capture's `run3` would make the older run look like the newer one.
 * `bundle_path` stays authoritative for finding the evidence.
 */
import { CellHost } from "@sip/corpus"
import * as Content from "@sip/corpus/content/content"
import { EXCLUDED, readRefusals } from "@sip/corpus/cut/excluded"
import * as PrepClose from "@sip/corpus/housekeeping/prep-close"
import * as SweepClose from "@sip/corpus/housekeeping/sweep-close"
import { listLine as jobLine } from "@sip/corpus/jobs/job-lines"
import { type Paths, prepDir, stemOf } from "@sip/corpus/paths"
import { countBundle } from "@sip/corpus/replay/rule-hits"
import * as CaseDoc from "@sip/corpus/report/case-doc"
import { read as readOverlay } from "@sip/corpus/triage-overlay"
import { captures as universeOf } from "@sip/corpus/universe"
import {
  Bundle,
  Cuts,
  Delta,
  Jobs,
  Measure,
  Model,
  Pins,
  Queue,
  Refusals,
  RuleHits,
  Runs,
  RunSets,
  StoreBinding,
  Sweeps
} from "@sip/store"
import * as Cause from "effect/Cause"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as Path from "effect/Path"
import { Argument, Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import { laneFlag, pathFlags } from "../flags.js"
import * as IdFlags from "../id-flags.js"
import { overridesOf, pathsOf } from "../options.js"
import { exit } from "../runner.js"
import * as Compare from "./compare.js"

const limitFlag = Flag.integer("limit").pipe(
  Flag.withDescription("Rows to show"),
  Flag.withDefault(20)
)

interface BundleOnDisk {
  readonly capture: string
  readonly dir: string
}

/** The entries of `dir`, none where it does not read. */
const entriesOf = (fs: FileSystem.FileSystem, dir: string) =>
  fs.readDirectory(dir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))

/** The capture a bundle belongs to: its own case ids say so; the session dir is the fallback. */
const captureOf = Effect.fn("Store.captureOf")(function*(
  sessionDir: string,
  stem: string,
  caseIds: ReadonlyArray<string>
) {
  for (const id of caseIds) {
    const cut = /^(.*)-case\d+$/.exec(id)
    if (cut !== null) return cut[1]!
  }
  const fs = yield* FileSystem.FileSystem
  const listed = (yield* entriesOf(fs, sessionDir)).find((entry) =>
    entry.startsWith(stem) && entry !== stem && !/^run\d+$/.test(entry)
  )
  return listed ?? `${stem}.pcap.gz`
})

/** Every run bundle under the session tree, capture by capture in run order. */
const listBundles = Effect.fn("Store.listBundles")(function*(sessions: string) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const out: Array<BundleOnDisk> = []
  for (const stem of [...(yield* entriesOf(fs, sessions))].sort()) {
    const sessionDir = path.join(sessions, stem)
    const info = yield* fs.stat(sessionDir).pipe(Effect.option)
    if (Option.isNone(info) || info.value.type !== "Directory") continue
    const runs = (yield* entriesOf(fs, sessionDir))
      .map((entry) => ({ entry, n: /^run(\d+)$/.exec(entry)?.[1] }))
      .filter((r): r is { entry: string; n: string } => r.n !== undefined)
      .sort((a, b) => Number(a.n) - Number(b.n))
    for (const { entry } of runs) out.push({ capture: stem, dir: path.join(sessionDir, entry) })
  }
  return out as ReadonlyArray<BundleOnDisk>
})

/** A bundle's own timestamps: its `campaign.json` states when it started, the tree when it ended. */
const timestamps = Effect.fn("Store.timestamps")(function*(dir: string) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const rollup = yield* fs.readFileString(path.join(dir, "campaign.json")).pipe(
    Effect.map((text) => JSON.parse(text) as { ts?: unknown }),
    Effect.orElseSucceed(() => ({}) as { ts?: unknown })
  )
  const info = yield* fs.stat(dir).pipe(Effect.option)
  const mtime = Option.isSome(info) ? Option.getOrUndefined(info.value.mtime) : undefined
  return {
    started: typeof rollup.ts === "string" ? rollup.ts : undefined,
    finished: mtime?.toISOString()
  }
})

const ingest = Effect.fn("Store.ingest")(function*(paths: Paths, lane: string) {
  const runs = yield* Runs.Service
  const host = yield* CellHost.Service
  const path = yield* Path.Path
  const known = new Set((yield* runs.allBundles).map((row) => row.bundlePath))
  const bundles = yield* listBundles(paths.sessions)
  let imported = 0
  let skipped = 0
  for (const bundle of bundles) {
    if (known.has(bundle.dir)) {
      skipped += 1
      continue
    }
    const reading = yield* Bundle.read(bundle.dir, { charges: host.charges })
    const capture = yield* captureOf(
      path.dirname(bundle.dir),
      bundle.capture,
      reading.cases.map((entry) => entry.caseId)
    )
    const when = yield* timestamps(bundle.dir)
    yield* runs.importRun({
      capture,
      lane: reading.lane ?? lane,
      mode: reading.mode ?? "record",
      status: reading.status,
      bundlePath: bundle.dir,
      ...(when.started === undefined ? {} : { startedAt: when.started }),
      ...(when.finished === undefined ? {} : { finishedAt: when.finished }),
      cases: reading.cases
    })
    imported += 1
    if (imported % 100 === 0) yield* Console.log(`[store] imported ${imported}…`)
  }
  yield* Console.log(
    `[store] ${bundles.length} bundle(s) on disk: imported=${imported} already-recorded=${skipped}`
  )
  return imported
})

/** Where the cut leaves what it refused of one capture. */
const excludedOf = (path: Path.Path, paths: Paths, capture: string): string =>
  path.join(paths.sessions, stemOf(capture), capture, EXCLUDED)

/**
 * The distinct reason tokens the cut refused a capture under, in file order —
 * the whole content of a `no-cases` row, so the row prints them the way a
 * `skipped` row prints its lane blocks. Empty where the cut left no record.
 */
const refusedUnder = Effect.fn("Store.refusedUnder")(function*(paths: Paths, capture: string) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const records = yield* fs.readFileString(excludedOf(path, paths, capture)).pipe(
    Effect.map((text) => JSON.parse(text) as unknown),
    Effect.orElseSucceed(() => undefined)
  )
  if (!Array.isArray(records)) return [] as ReadonlyArray<string>
  const reasons = records.flatMap((entry: unknown) => {
    const reason = typeof entry === "object" && entry !== null
      ? (entry as Record<string, unknown>)["reason"]
      : undefined
    return typeof reason === "string" ? [reason] : []
  })
  return [...new Set(reasons)] as ReadonlyArray<string>
})

const refusedLine = (reasons: ReadonlyArray<string>): string =>
  reasons.length === 0 ? "  refused: (no record)" : `  refused: ${reasons.join(", ")}`

const status = Effect.fn("Store.status")(function*(paths: Paths, lane: string, limit: number) {
  const runs = yield* Runs.Service
  const standings = yield* runs.standings(lane)
  const { registry, tickets } = yield* readOverlay(paths.registry)
  const captures = yield* universeOf(standings.map((one) => one.capture))
  const view = Queue.overlay({ standings, registry, tickets, captures })
  yield* Console.log(
    `[store] lane=${lane} recorded=${standings.length} universe=${captures.length} ` +
      `never-run=${view.neverRun.length}`
  )
  for (const key of Queue.QUEUE_ORDER) yield* Console.log(`   ${key.padEnd(22)} ${view.totals[key] ?? 0}`)
  yield* Console.log(`   ${"off-queue records".padEnd(22)} ${view.offQueue.length}`)
  for (const row of view.rows.slice(0, limit)) {
    const refused = row.status !== "no-cases" ? "" : refusedLine(yield* refusedUnder(paths, row.capture))
    yield* Console.log(
      `${row.capture}  ${row.status}  run${row.runId} ${row.failed}/${row.cases.length} failed` +
        (row.rfcGating === 0 ? "" : `  rfc: ${row.rfcGating} gating`) +
        (row.skipReasons.length === 0 ? "" : `  skipped: ${row.skipReasons.join(", ")}`) +
        refused +
        (row.lastOkRunId === undefined ? "  (never green)" : `  last-ok run${row.lastOkRunId}`)
    )
  }
})

/** A materialisation, or the one-line reason the store could not answer it. */
const orSaid = <R>(effect: Effect.Effect<string, unknown, R>) =>
  effect.pipe(
    Effect.catchCause((cause) =>
      Effect.succeed(`unavailable — ${Cause.pretty(cause).split("\n")[0]}`)
    )
  )

const said = (label: string, where: string) => Console.log(`  ${label.padEnd(10)} ${where}`)

/**
 * Every artifact of ONE capture, by absolute path: what it was cut from, what
 * the store holds of that cut, and what each lane's standing run left on disk.
 *
 * The cases and the flows document are MATERIALISED on the way past, because a
 * path into a lazily-filled cache is only an answer once the file is there.
 */
const where = Effect.fn("Store.where")(function*(
  facts: CliFacts,
  paths: Paths,
  capture: string,
  prep: number | undefined
) {
  const cuts = yield* Cuts.Service
  const runs = yield* Runs.Service
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  yield* Console.log(capture)
  if (stemOf(capture) === capture) {
    yield* Console.log("  ! a bare stem names no capture — pass the pcap file name, extension and all")
  }
  yield* said("corpus", path.join(paths.corpus, capture))

  const standing = yield* cuts.standing(capture, prep)
  if (standing === null) {
    yield* said(
      "cut",
      (prep === undefined ? "none" : `none as of prep${prep}`) +
        ` — \`${facts.invocation("cut")} ${capture}\` records one`
    )
  } else {
    yield* said(
      "cut",
      `prep${standing.prepId}  ${standing.cutAt}  ` +
        (standing.broke !== null ? `BROKE: ${standing.broke}` : `${standing.cases.length} case(s)`)
    )
    yield* said("cut log", prepDir(paths.sessions, standing.prepId))
    if (standing.flowsHash !== null) {
      yield* said("flows", yield* orSaid(Content.materialiseFlows(paths.sessions, standing.flowsHash)))
    }
    for (const entry of standing.cases) {
      yield* said("case", yield* orSaid(Content.materialise(paths.sessions, entry.caseHash)))
    }
  }

  const excluded = excludedOf(path, paths, capture)
  if (yield* fs.exists(excluded).pipe(Effect.orElseSucceed(() => false))) yield* said("excluded", excluded)

  for (const lane of yield* runs.lanes) {
    const run = yield* runs.standing(capture, lane.lane)
    if (run === null) continue
    yield* said(
      `run${run.runId}`,
      `${run.bundlePath}  ${lane.lane}  ${run.status}  ` +
        `${run.failed}/${run.cases} failed` +
        (run.rfcGating === 0 ? "" : `  rfc: ${run.rfcGating} gating`) +
        (run.lastOkRunId === null ? "  (never green)" : `  last-ok run${run.lastOkRunId}`)
    )
  }
})

/**
 * The per-rule counts of every run whose bundle is STILL ON DISK and was never
 * measured — the history the record cannot have, since it starts the day it
 * lands and retention keeps few bundles.
 *
 * A run whose bundle is gone is skipped and stays unmeasured: marking it read
 * would claim a zero nobody took.
 */
const backfillRuleHits = Effect.fn("Store.backfillRuleHits")(function*(limit: number) {
  const hits = yield* RuleHits.Service
  const fs = yield* FileSystem.FileSystem
  const pending = yield* hits.unmeasured(limit)
  let measured = 0
  let gone = 0
  let cells = 0
  for (const run of pending) {
    if (!(yield* fs.exists(run.bundlePath).pipe(Effect.orElseSucceed(() => false)))) {
      gone += 1
      continue
    }
    const counted = yield* countBundle(run.bundlePath, run.lane)
    if (counted.cells === 0) {
      gone += 1
      continue
    }
    yield* hits.record(run.id, counted.hits)
    measured += 1
    cells += counted.cells
  }
  yield* Console.log(
    `[store] rule hits: ${measured} run(s) measured over ${cells} cell(s); ` +
      `${gone} of ${pending.length} unmeasured run(s) have no readable bundle left`
  )
})

/**
 * The per-call measures the queue reads and the rows recorded before them
 * lack: main errors off the surviving bundles, refusals off the session tree's
 * `excluded.json`. A cut whose file holds no record is left as it is — it may
 * simply have refused nothing — and is asked again next time.
 */
const backfillCalls = Effect.fn("Store.backfillCalls")(function*(paths: Paths, lane: string | undefined) {
  const measure = yield* Measure.Service
  const refusals = yield* Refusals.Service
  const host = yield* CellHost.Service
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path

  const pending = yield* measure.unmeasured(lane)
  let measured = 0
  let gone = 0
  let rows = 0
  for (const run of pending) {
    if (!(yield* fs.exists(path.join(run.bundlePath, "campaign.json")).pipe(Effect.orElseSucceed(() => false)))) {
      gone += 1
      continue
    }
    const reading = yield* Bundle.read(run.bundlePath, { charges: host.charges })
    if (reading.cases.length === 0) {
      gone += 1
      continue
    }
    rows += yield* measure.remeasure(run.id, reading.cases)
    measured += 1
    if (measured % 200 === 0) yield* Console.log(`[store] measured ${measured}…`)
  }
  yield* Console.log(
    `[store] main errors: ${measured} run(s) re-measured over ${rows} case row(s); ` +
      `${gone} of ${pending.length} unmeasured run(s) have no readable bundle left`
  )

  const standing = yield* refusals.standing
  let recorded = 0
  let firstTime = 0
  let refused = 0
  for (const cut of standing) {
    const records = yield* readRefusals(excludedOf(path, paths, cut.capture))
    if (records.length === 0) continue
    refused += yield* refusals.record(cut.prepId, cut.capture, records)
    recorded += 1
    if (cut.rows === 0) firstTime += 1
  }
  yield* Console.log(
    `[store] refusals: ${recorded} capture(s) recorded (${firstTime} for the first time), ` +
      `${refused} refused call(s); ${standing.length - recorded} of ${standing.length} standing cut(s) ` +
      "have no record on disk"
  )
})

/** One sweep as a line: what it swept, how, and the tree it swept on. */
const sweepLine = (row: Sweeps.Row): string =>
  `sweep ${String(row.id).padStart(4)}  ${row.startedAt}  ${row.selection ?? `--${row.universe}`}  ` +
  `${String(row.runs).padStart(5)} run(s)  x${row.concurrency}  ` +
  `${row.finishedAt === null ? "UNFINISHED" : "done"}  ` +
  `tree ${Model.codeLine(row.code, "tree")}`

/** One run as a listing line: who made it, on which code, and what it came to. */
const runLine = (row: RunSets.RunRow): string =>
  `run${row.id}  ${row.startedAt}  ${row.capture}  ${row.lane}  ${row.setKind}  ${row.status}  ` +
  `${row.failed}/${row.cases} failed  env ${row.env ?? "unknown"}@${row.branch ?? "unknown"}  ` +
  `code ${Model.codeLine(row.code, "sha")}` +
  (row.sweepId === null ? "" : `  sweep ${row.sweepId}`) +
  (row.jobId === null ? "" : `  job ${row.jobId}`) +
  (row.cleanup === null ? "" : `  DIRTY cleanup: ${row.cleanup}`)

/** The runs a pin line names: one selector, every run it admits. */
const runsNamed = Effect.fn("Store.runsNamed")(function*(selector: string) {
  const parsed = yield* StoreBinding.parse(selector)
  if ("refused" in parsed) return parsed.refused
  const sets = yield* RunSets.Service
  const refused = yield* sets.refusal(parsed)
  if (refused !== null) return refused
  return (yield* sets.runs(parsed, Number.MAX_SAFE_INTEGER)).map((row) => row.id)
})

/** The root of a store command named `name`: the path flags every verb reads. */
const rootOf = <Name extends string>(facts: CliFacts, name: Name, verbLines: () => ReadonlyArray<string>) =>
  Command.make(name, {}, () =>
    Effect.gen(function*() {
      yield* Console.error(
        [`usage: ${name} <verb> [flags]   (${name} <verb> --help for its flags)`, "", ...verbLines(), "", facts.frontDoors]
          .join("\n")
      )
      yield* exit(2)
    })).pipe(
      Command.withDescription(`The run store, from the command line\n\n${facts.frontDoors}`),
      Command.withSharedFlags(pathFlags)
    )

/**
 * The store's verbs, each reading the roots the root command was given
 * through `rootPaths`, in the order the help lists them.
 */
export const verbs = <E, R>(facts: CliFacts, rootPaths: Effect.Effect<Paths, E, R>) => {
  const migrate = Command.make("migrate", {}, () =>
    Effect.gen(function*() {
      const binding = yield* StoreBinding.Service
      const applied = yield* binding.migrate
      yield* Console.log(
        applied.length === 0
          ? "[store] schema already current"
          : `[store] applied ${applied.map(([id, name]) => `${id}_${name}`).join(", ")}`
      )
    })).pipe(Command.withDescription("Apply the schema"))

  const seed = Command.make("seed", { lane: laneFlag(facts) }, (input) =>
    Effect.gen(function*() {
      yield* ingest(yield* rootPaths, input.lane)
    })).pipe(Command.withDescription("Import run bundles with no row yet"))

  const rebuild = Command.make("rebuild", { lane: laneFlag(facts) }, (input) =>
    Effect.gen(function*() {
      const at = yield* rootPaths
      const runs = yield* Runs.Service
      const dropped = yield* runs.forget(at.sessions)
      yield* Console.log(`[store] forgot ${dropped} run row(s) under ${at.sessions}`)
      yield* ingest(at, input.lane)
    })).pipe(Command.withDescription("Forget the session rows, then re-derive"))

  const statusCommand = Command.make("status", { lane: laneFlag(facts), limit: limitFlag }, (input) =>
    Effect.gen(function*() {
      yield* status(yield* rootPaths, input.lane, input.limit)
    })).pipe(Command.withDescription("What the queue would render"))

  const sweepsCommand = Command.make("sweeps", { limit: limitFlag }, (input) =>
    Effect.gen(function*() {
      const sweeps = yield* Sweeps.Service
      for (const row of yield* sweeps.list(input.limit)) yield* Console.log(sweepLine(row))
    })).pipe(Command.withDescription("The invocations, newest first"))

  const jobsCommand = Command.make("jobs", { limit: limitFlag }, (input) =>
    Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      for (const row of yield* jobs.list(input.limit)) yield* Console.log(jobLine(row))
    })).pipe(Command.withDescription("The jobs, newest first"))

  const runsCommand = Command.make(
    "runs",
    {
      selector: Argument.string("selector").pipe(Argument.withDescription(`Which runs: ${facts.selectorForms}`)),
      limit: limitFlag
    },
    (input) =>
      Effect.gen(function*() {
        const parsed = yield* StoreBinding.parse(input.selector)
        if ("refused" in parsed) {
          yield* Console.error(`[store] refused: ${parsed.refused}`)
          return yield* exit(2)
        }
        const sets = yield* RunSets.Service
        const refused = yield* sets.refusal(parsed)
        if (refused !== null) {
          yield* Console.error(`[store] refused: ${refused}`)
          return yield* exit(2)
        }
        for (const row of yield* sets.runs(parsed, input.limit)) yield* Console.log(runLine(row))
      })
  ).pipe(Command.withDescription("The runs a selector names, newest first"))

  const compareCommand = Command.make("compare", Compare.flags(facts), Compare.handler(facts, true)).pipe(
    Command.withDescription(
      `What moved between two run sets (\`${facts.invocation("compare")}\`); a bare integer is a sweep here`
    )
  )

  const whereCommand = Command.make(
    "where",
    {
      capture: Argument.string("capture").pipe(
        Argument.withDescription("A pcap file name, extension and all")
      ),
      prep: IdFlags.flag("prep", "prep").pipe(
        Flag.withDescription("The cut to read the capture as of"),
        Flag.optional
      )
    },
    (input) =>
      Effect.gen(function*() {
        yield* where(facts, yield* rootPaths, input.capture, Option.getOrUndefined(input.prep))
      })
  ).pipe(Command.withDescription("Every artifact of one capture"))

  const caseDocCommand = Command.make(
    "case-doc",
    {
      first: Argument.string("capture-or-id").pipe(
        Argument.withDescription("The capture, then its case; or a case id alone, in any cited form")
      ),
      second: Argument.string("case").pipe(
        Argument.withDescription("The case of the capture: caseN, a full case id"),
        Argument.optional
      ),
      prep: IdFlags.flag("prep", "prep").pipe(
        Flag.withDescription("The prep to read the case as of; the newest naming it where absent"),
        Flag.optional
      ),
      print: Flag.choice("print", CaseDoc.PRINTS).pipe(
        Flag.withDescription(
          "path: the case directory, document and side rows; flow: the reference ladder; ledger: the side rows"
        ),
        Flag.withDefault("path")
      )
    },
    (input) =>
      Effect.gen(function*() {
        const at = yield* rootPaths
        const prep = Option.getOrUndefined(input.prep)
        const asked: CaseDoc.Input = Option.match(input.second, {
          onNone: () => ({ id: input.first, print: input.print }),
          onSome: (id) => ({ capture: input.first, id, print: input.print })
        })
        const read = yield* CaseDoc.read(at.sessions, prep === undefined ? asked : { ...asked, prep })
        if (read._tag === "refused") {
          yield* Console.error(`[store] refused: ${read.why}`)
          return yield* exit(1)
        }
        for (const line of read.lines) yield* Console.log(line)
      })
  ).pipe(Command.withDescription("One case's document by any cited id: its path, reference ladder or side rows"))

  const deltaCommand = Command.make(
    "delta",
    {
      lane: laneFlag(facts),
      against: Flag.string("against").pipe(
        Flag.withDescription("The lane the cells are green on"),
        Flag.withDefault(facts.defaultLane)
      ),
      limit: limitFlag
    },
    (input) =>
      Effect.gen(function*() {
        if (input.against === input.lane) {
          yield* Console.error(`[store] refused: --lane and --against are both ${input.lane}`)
          return yield* exit(2)
        }
        const delta = yield* Delta.Service
        const population = yield* delta.population(input.lane, input.against)
        const rows = yield* delta.rows(input.lane, input.against)
        const unasked = yield* delta.unasked(input.lane, input.against)
        yield* Console.log(Delta.render(population, rows, unasked, input.limit))
      })
  ).pipe(Command.withDescription("Green there, red here"))

  const rulehits = Command.make(
    "rulehits",
    { limit: Flag.integer("limit").pipe(Flag.withDescription("Runs to measure"), Flag.optional) },
    (input) => backfillRuleHits(Option.getOrElse(input.limit, () => Number.MAX_SAFE_INTEGER))
  ).pipe(Command.withDescription("Count the rule hits of runs never measured"))

  const callsBackfill = Command.make(
    "calls-backfill",
    {
      lane: Flag.string("lane").pipe(
        Flag.withDescription("The lane whose standing runs to measure; every lane where absent"),
        Flag.optional
      )
    },
    (input) =>
      Effect.gen(function*() {
        yield* backfillCalls(yield* rootPaths, Option.getOrUndefined(input.lane))
      })
  ).pipe(Command.withDescription("Measure what the per-call queue lacks: main errors and refusals"))

  const selectorArgument = Argument.string("selector").pipe(
    Argument.withDescription(`The runs: a run id, or ${facts.selectorForms}`)
  )

  const pinCommand = Command.make(
    "pin",
    {
      selector: selectorArgument,
      ticket: Flag.string("ticket").pipe(Flag.withDescription("The ticket that keeps them"), Flag.optional)
    },
    (input) =>
      Effect.gen(function*() {
        const ids = yield* runsNamed(input.selector)
        if (typeof ids === "string") {
          yield* Console.error(`[store] refused: ${ids}`)
          return yield* exit(2)
        }
        const pinned = yield* Effect.flatMap(Pins.Service, (pins) => pins.pin(ids, Option.getOrNull(input.ticket)))
        yield* Console.log(`[store] pinned ${pinned.length} run(s) of ${ids.length} named; the others were pinned already`)
      })
  ).pipe(Command.withDescription("Keep runs and their bundles through retention and the wipe"))

  const unpinCommand = Command.make("unpin", { selector: selectorArgument }, (input) =>
    Effect.gen(function*() {
      const ids = yield* runsNamed(input.selector)
      if (typeof ids === "string") {
        yield* Console.error(`[store] refused: ${ids}`)
        return yield* exit(2)
      }
      const unpinned = yield* Effect.flatMap(Pins.Service, (pins) => pins.unpin(ids))
      yield* Console.log(`[store] unpinned ${unpinned.length} run(s)`)
    })).pipe(Command.withDescription("Let retention take runs a pin kept"))

  const pinsCommand = Command.make("pins", {}, () =>
    Effect.gen(function*() {
      for (const pin of yield* Effect.flatMap(Pins.Service, (pins) => pins.all)) {
        yield* Console.log(`run${pin.runId}  pinned ${pin.pinnedAt}${pin.ticket === null ? "" : `  by ${pin.ticket}`}`)
      }
    })).pipe(Command.withDescription("The pinned runs, newest first"))

  const closeSweepCommand = Command.make(
    "close-sweep",
    { sweep: IdFlags.argument("sweep", "sweep").pipe(Argument.withDescription("The sweep whose process is gone")) },
    (input) =>
      Effect.gen(function*() {
        const closed = yield* SweepClose.close(input.sweep)
        if (closed._tag === "refused") {
          yield* Console.error(`[store] refused: ${closed.why}`)
          return yield* exit(1)
        }
        yield* Console.log(
          `[store] sweep ${input.sweep} closed: ${closed.runs.length} open run(s) closed as abandoned` +
            (closed.job === null ? "" : `, job ${closed.job} closed`)
        )
      })
  ).pipe(Command.withDescription("Close a sweep whose process is gone, and the runs it left open"))

  const closePrepCommand = Command.make(
    "close-prep",
    { prep: IdFlags.argument("prep", "prep").pipe(Argument.withDescription("The prep whose cut is gone")) },
    (input) =>
      Effect.gen(function*() {
        const closed = yield* PrepClose.close(input.prep)
        if (closed._tag === "refused") {
          yield* Console.error(`[store] refused: ${closed.why}`)
          return yield* exit(1)
        }
        yield* Console.log(`[store] prep ${input.prep} closed`)
      })
  ).pipe(Command.withDescription("Finish a prep whose cut is gone"))

  return [
    migrate,
    seed,
    rebuild,
    statusCommand,
    sweepsCommand,
    jobsCommand,
    runsCommand,
    compareCommand,
    whereCommand,
    caseDocCommand,
    deltaCommand,
    rulehits,
    callsBackfill,
    pinCommand,
    unpinCommand,
    pinsCommand,
    closeSweepCommand,
    closePrepCommand
  ] as const
}

/**
 * The store command named `name`: its verbs, then the deployment's `extra`
 * ones, each handed the roots the root command was given.
 */
export const command = <Name extends string, Extra extends ReadonlyArray<Command.Command<any, any, any, any, any>>>(
  facts: CliFacts,
  name: Name,
  extra: (rootPaths: Effect.Effect<Paths, any, any>) => Extra = () => [] as unknown as Extra
) => {
  let lines: ReadonlyArray<string> = []
  const root = rootOf(facts, name, () => lines)
  const rootPaths = Effect.flatMap(root, (shared) => pathsOf(overridesOf(shared)))
  const all = [...verbs(facts, rootPaths), ...extra(rootPaths)] as const
  const width = Math.max(...all.map((verb) => verb.name.length))
  lines = all.map((verb) => `  ${verb.name.padEnd(width)}  ${(verb.description ?? "").split("\n")[0]}`)
  return root.pipe(Command.withSubcommands(all))
}
