/**
 * The report phase, once the job's collectors answered. It closes each lane's
 * run row with its case results (`record.ts`, each cell's log read off the
 * pack's log collector's line; a quick run's cells say none was fetched), the
 * refs of the pack's universe no case file ports among them where its
 * campaign reports them, then reads the run back from the store and writes
 * from it: one line per cell on the console (its outcome, lane, case, bundle;
 * the lines failing it indented under it, each with its side, and its waived
 * ones; the status of a cell its lines leave unverified; a cell replayed more
 * than once, each attempt with its bundle and the kept one marked; what a
 * quick run did not judge), the counts per lane, `results.json` and
 * `junit.xml` in the run's directory, and the JUnit file where the run asked
 * for one elsewhere (`junit.ts`); a quick run writes no JUnit file.
 *
 * A phase that did not run is an error of every lane's run row: one that
 * ended before the lanes (provision, compile) leaves them no cell, a job's
 * line that did not pass or a dirty batch end leaves them their cells.
 */
import { Verification } from "@sip/contracts"
import { Attachments } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as path from "node:path"
import { type JunitPack, junitXml } from "./junit.js"
import { labelOf, sideOf, typeOf, valuesOf } from "./line-text.js"
import type { CampaignPack } from "./pack.js"
import { QUICK_LOG } from "./quick.js"
import { close, type Record, type RunStore } from "./record.js"
import * as Results from "@sip/store/campaign-results"
import { type Outcome, PhaseFailed } from "./run.js"
import { notPorted, type Universe } from "./universe.js"

/** What a pack states of its run's report: its JUnit report, its universe, what a quick run's cells were not judged on. */
export interface ReportPack<R extends Results.CampaignRun = Results.CampaignRun> extends JunitPack<R> {
  readonly universe: Universe
  /** The lines the report states under each cell a quick run replayed. */
  readonly quickNotes: ReadonlyArray<string>
}

/** The pack's store as the report reads it: the run finished, then read back by its id. */
export interface ReportStore<R extends Results.CampaignRun> extends Pick<RunStore, "finish" | "writeCell"> {
  readonly read: (capture: string, options: { readonly ids: ReadonlyArray<number> }) => Effect.Effect<ReadonlyArray<R>, SqlError>
}

const MARK = { pass: "PASS ", failure: "FAIL ", error: "ERROR", skipped: "SKIP " } as const

/**
 * The console lines of a stored run; each cell of a quick one states the
 * pack's quick notes. `absent` names where a full run found no universe: its
 * count lines say so instead of counting none ported. Where the campaign
 * does not report coverage they say nothing of it.
 */
export const lines = (
  pack: Pick<ReportPack<never>, "failureType" | "log" | "universe" | "quickNotes">,
  run: Results.CampaignRun,
  quick = false,
  absent?: string,
  coverage = true
): ReadonlyArray<string> => {
  const out: Array<string> = []
  for (const lane of run.lanes) {
    for (const r of lane.results) {
      const outcome = Results.outcomeOf(r)
      if (outcome === "skipped" && r.skipped === Results.NOT_PORTED) continue
      const status = Results.statusOf(r)
      const type = outcome === "failure" ? ` [${typeOf(pack.failureType, r.verifications)}]` : ""
      const bundle = r.cell === null ? "" : `  ${path.join(lane.bundlePath, r.cell)}`
      out.push(`${MARK[outcome]} ${lane.lane} ${r.caseId}${type}${bundle}`)
      for (const [i, a] of (r.attempts ?? []).entries()) {
        const said = [
          MARK[a.outcome].trimEnd(),
          ...(a.outcome === "failure" ? [`${a.failed} failed check(s)`] : []),
          ...(a.kept ? ["kept"] : []),
          a.cell === null ? "no bundle" : path.join(lane.bundlePath, a.cell)
        ]
        out.push(`        attempt ${i + 1} of ${r.attempts!.length}: ${said.join("  ")}`)
      }
      for (const line of Verification.failing(r.verifications)) {
        const { expected, seen } = valuesOf(line)
        out.push(`        [${sideOf(line) ?? "untyped"}] ${labelOf(line)}: expected ${expected ?? "(no value)"}, seen ${seen ?? "(absent)"}`)
      }
      for (const line of r.verifications.filter((one) => one.waived === true)) out.push(`        waived: ${labelOf(line)}`)
      if (status === "error") out.push(`        ${(r.errorDetail ?? "").split("\n")[0]}`)
      else if (outcome === "error") out.push(`        ${status}`)
      if (outcome === "skipped" || (r.skipped !== null && Results.ranUnchecked(r.skipped))) {
        out.push(`        ${r.skipped ?? ""}`)
      }
      if (r.log !== null && "missing" in r.log) out.push(`        ${pack.log} missing: ${r.log.missing}`)
      if (quick && r.cell !== null) for (const note of pack.quickNotes) out.push(`        ${note}`)
    }
    if (lane.status === "error") out.push(`ERROR ${lane.lane} run: ${lane.detail ?? ""}`)
  }
  for (const lane of run.lanes) {
    const count = (o: Results.Outcome) => lane.results.filter((r) => Results.outcomeOf(r) === o).length
    const unported = lane.results.filter((r) => r.skipped === Results.NOT_PORTED).length
    const excluded = lane.results.filter(Results.isExcluded).length
    const skipped = count("skipped") - unported - excluded
    const counts = [
      `${count("pass")} passed`,
      `${count("failure")} failed`,
      `${count("error")} errored`,
      ...(skipped === 0 ? [] : [`${skipped} skipped`]),
      ...(excluded === 0 ? [] : [`${excluded} excluded`]),
      ...(!coverage ? [] : [absent === undefined ? `${unported} not ported` : `not ported unknown: ${pack.universe.absent(absent)}`])
    ]
    out.push(`${lane.lane}: ${counts.join(", ")}`)
  }
  return out
}

/** Each of `bundles`' log file (the `collector`'s, as its attachments list it); a bundle whose list does not read names none. */
export const logFilesOf = (collector: string, bundles: ReadonlyArray<string>) =>
  Effect.map(
    Effect.forEach(bundles, (bundle) =>
      Attachments.listed(bundle).pipe(
        Effect.map((listed) => listed.attachments.filter((one) => one.name === collector).at(-1)),
        Effect.map((found) => (found === undefined ? [] : [[bundle, path.resolve(bundle, found.file)] as const]))
      )),
    (pairs) => new Map(pairs.flat()) as ReadonlyMap<string, string>
  )

export interface Reporting {
  readonly root: string
  readonly junit: string | undefined
  readonly site: string
  readonly record: Record
  /** A run of less than every case: no ref is recorded not ported. */
  readonly partial: boolean
  /** Whether the campaign's full runs report the refs no case covers. */
  readonly coverage: boolean
  /** The refs the run's case files state: the ones it ports. */
  readonly ported: ReadonlyArray<string>
  /** A quick run (`quick.ts`). */
  readonly quick: boolean
}

/** What each lane's run row is closed with as an error; undefined where it ran to its end. */
export const laneErrors = (outcome: Outcome, lane: string): string | undefined => {
  const mine = outcome.errors.filter((e) => !e.phase.startsWith("replay ") || e.phase === `replay ${lane}`)
  return mine.length === 0 ? undefined : mine.map((e) => `${e.phase}: ${e.message}`).join("; ")
}

/** The report of `outcome` into `store` and the run's directory, under `campaign` and its report `pack`. */
export const report = <R extends Results.CampaignRun>(
  store: ReportStore<R>,
  campaign: Pick<CampaignPack, "own" | "logCollector" | "verb">,
  pack: ReportPack<R>,
  outcome: Outcome,
  reporting: Reporting
) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const counted = outcome.results.length > 0 && !reporting.partial && reporting.coverage
    const where = yield* pack.universe.where
    const refs = counted ? yield* pack.universe.refs(where) : []
    const unported = refs === undefined ? [] : notPorted(refs, reporting.ported)
    if (refs === undefined) yield* Console.log(`not ported: unknown, ${pack.universe.absent(where)} (${pack.universe.looked})`)
    for (const row of reporting.record.lanes) {
      yield* close(store, campaign, {
        record: reporting.record,
        row,
        results: outcome.results.filter((r) => r.lane === row.lane),
        notPorted: unported,
        error: laneErrors(outcome, row.lane),
        unlogged: reporting.quick ? QUICK_LOG : undefined
      })
    }
    yield* store.finish(reporting.record.id)
    const [run] = yield* store.read(reporting.record.capture, { ids: [reporting.record.id] })
    if (run === undefined) return yield* Effect.fail(new Error(`${campaign.verb} ${reporting.record.id} reads back as nothing`))
    for (const line of lines(pack, run, reporting.quick, refs === undefined ? where : undefined, reporting.coverage)) {
      yield* Console.log(line)
    }
    yield* fs.makeDirectory(reporting.root, { recursive: true })
    yield* fs.writeFileString(path.join(reporting.root, "results.json"), `${JSON.stringify(run, null, 2)}\n`)
    const logs = yield* logFilesOf(
      campaign.logCollector,
      outcome.results.flatMap((result) => (result.bundle === undefined ? [] : [result.bundle]))
    )
    const xml = junitXml(pack, run, { verb: campaign.verb, site: reporting.site, logs })
    if (!reporting.quick) yield* fs.writeFileString(path.join(reporting.root, "junit.xml"), xml)
    if (reporting.junit !== undefined && !reporting.quick) {
      yield* fs.makeDirectory(path.dirname(reporting.junit), { recursive: true })
      yield* fs.writeFileString(reporting.junit, xml)
      yield* Console.log(`junit: ${reporting.junit}`)
    }
    yield* Console.log(`summary: ${reporting.site}/campaign/${reporting.record.capture}?run=${run.id}`)
    yield* Console.log(`run: ${reporting.root}`)
  }).pipe(
    Effect.catch((e) =>
      Effect.andThen(
        Console.error(`${campaign.verb}: the report was not written: ${String(e)}`),
        Effect.fail(new PhaseFailed({ phase: "report", reason: String(e) }))
      )
    )
  )
