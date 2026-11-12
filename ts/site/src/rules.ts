/**
 * The RULES catalog: every rule this deployment runs, joined to what the corpus
 * came to under it.
 *
 * The join is keyed on the ROSTER and never on the counts, so a rule that fires
 * nowhere is a row reading zero rather than an absence. That is the whole point
 * of the view: `refer-replaces-out-of-scope` refused nothing in ten thousand
 * documents and `source-actor-ack-not-captured` refused about one, and a table
 * that lists only what fired shows neither.
 *
 * Three readings, from where each is DURABLE. Refusals come off the session
 * tree's `excluded.json`, which the cut writes and retention never touches, so
 * it is the standing cut entire. Tolerance and bless counts come off
 * `run_rule_hits`, because a bundle survives only while it is a (capture,
 * lane)'s latest or last-green one and a reading taken from disk answers for
 * the survivors alone. Declarations come off the standing cut's own documents.
 *
 * Every count carries the population it was taken over. A rule reading zero on
 * a population nothing measured is not a finding, and the two must not be told
 * apart by eye.
 */
import { Declared, RuleHits, Runs } from "@sip/store"
import * as Context from "effect/Context"
import { Effect, Layer } from "effect"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import { ViewerConfig } from "./config.ts"
import { SitePack } from "./site-pack.ts"

/** How a rule's count stands: it fired, it can match nothing as written, nothing reached it, or the roster lacks it. */
export type Standing = "measured" | "declared-zero" | "unexercised" | "unrostered"

/** The cheap falsifier's report, as `<sessions>/_rules/off-tier.json` holds it. */
const OFF_TIER_REPORT = "_rules/off-tier.json"

/** How one lane's delta list classes a tolerance rule. */
export interface LaneClass {
  readonly lane: string
  readonly class: string
  readonly ticket: string
}

/** One count of one rule, under the subdivision it was counted in. */
export interface Bucket {
  readonly bucket: string
  readonly hits: number
  readonly cells: number
}

/** What the cheap falsifier came to on one rule: a firing here is a defect. */
export interface OffTier {
  readonly fired: number
  readonly silent: number
  readonly unavailable: number
  readonly evidence: ReadonlyArray<string>
}

/** One rule, as it declares itself and as the corpus answered it. */
export interface RuleRow {
  readonly family: string
  readonly id: string
  readonly stage: string
  readonly tier: string
  readonly subject: string
  readonly disposition: string
  /** Why the rule can match nothing; its zero is then the rule's own claim. */
  readonly inert?: string
  /** Why no replay can contradict it. A claim about falsification, not firing. */
  readonly unfalsifiable?: string
  readonly lanes: ReadonlyArray<LaneClass>
  /** measured | declared-zero | unexercised | unrostered. */
  readonly standing: string
  /** Records, refusals or cases the rule named — the family's own unit. */
  readonly hits: number
  /** Cells or captures it landed on: `hits` says how loud, this how wide. */
  readonly cells: number
  readonly buckets: ReadonlyArray<Bucket>
  /** A handful of cells or captures by name. */
  readonly samples: ReadonlyArray<string>
  readonly offTier?: OffTier
}

/** What the counts were taken over. Every number here bounds a zero. */
export interface Population {
  /** Captures whose `excluded.json` the refusal counts were read from. */
  readonly refusalCaptures: number
  readonly refusalRecords: number
  /** Standing runs the tolerance and bless counts cover. */
  readonly standingRuns: number
  /** Those of them whose per-rule counts were recorded. */
  readonly measuredRuns: number
  readonly cutCaptures: number
  readonly cutCases: number
}

/** The last cheap-falsifier pass, where one left a report. */
export interface OffTierPass {
  readonly at: string
  readonly captures: number
  readonly evaluated: number
  readonly unreadable: number
  readonly findings: number
}

export interface RulesView {
  readonly available: boolean
  /** Why a reading is thin or missing, when it is. */
  readonly detail: string
  readonly lane?: string
  readonly lanes: ReadonlyArray<{ readonly lane: string; readonly runs: number; readonly captures: number }>
  readonly population: Population
  readonly offTierPass?: OffTierPass
  readonly rows: ReadonlyArray<RuleRow>
}

export interface Interface {
  readonly view: (lane?: string) => Effect.Effect<RulesView>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/Rules") {}

/** One `excluded.json` record, read for the two fields a count needs. */
const reasonOf = (entry: unknown): string | undefined => {
  const reason = typeof entry === "object" && entry !== null
    ? (entry as Record<string, unknown>)["reason"]
    : undefined
  return typeof reason === "string" ? reason : undefined
}

/** How many refusals one rule made, and over how many captures. */
interface Refusals {
  readonly byRule: ReadonlyMap<string, { hits: number; captures: Set<string>; samples: Array<string> }>
  readonly captures: number
  readonly records: number
}

const SAMPLES = 5

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const path = yield* Path
    const config = yield* ViewerConfig.Service
    const pack = yield* SitePack.Service
    const runs = yield* Runs.Service
    const ruleHits = yield* RuleHits.Service
    const declared = yield* Declared.Service

    const readJson = (file: string) =>
      fs.readFileString(file).pipe(
        Effect.flatMap((text) =>
          Effect.try({ try: () => JSON.parse(text) as unknown, catch: () => null })
        ),
        Effect.orElseSucceed(() => null)
      )

    const listDir = (dir: string) =>
      fs.readDirectory(dir).pipe(Effect.orElseSucceed(() => [] as Array<string>))

    /**
     * Every refusal the STANDING CUT recorded: `<sessions>/<stem>/<capture>/
     * excluded.json`, which the cut rewrites per capture and retention never
     * removes. A capture with no such file refused nothing, which is a reading
     * and not a gap.
     */
    const refusalsOf = Effect.fn("Rules.refusalsOf")(function*(sessions: string) {
      const byRule = new Map<
        string,
        { hits: number; captures: Set<string>; samples: Array<string> }
      >()
      let captures = 0
      let records = 0
      for (const stem of yield* listDir(sessions)) {
        if (stem.startsWith("_")) continue
        for (const entry of yield* listDir(path.join(sessions, stem))) {
          if (!entry.includes(".pcap")) continue
          captures += 1
          const document = yield* readJson(path.join(sessions, stem, entry, "excluded.json"))
          if (!Array.isArray(document)) continue
          for (const record of document) {
            const reason = reasonOf(record)
            if (reason === undefined) continue
            records += 1
            const at = byRule.get(reason) ?? { hits: 0, captures: new Set<string>(), samples: [] }
            at.hits += 1
            at.captures.add(entry)
            if (at.samples.length < SAMPLES && !at.samples.includes(entry)) at.samples.push(entry)
            byRule.set(reason, at)
          }
        }
      }
      return { byRule, captures, records } satisfies Refusals
    })

    /** The last cheap-falsifier pass, per rule, where one left a report. */
    const offTierOf = Effect.fn("Rules.offTierOf")(function*(sessions: string) {
      const document = yield* readJson(path.join(sessions, OFF_TIER_REPORT))
      if (typeof document !== "object" || document === null) return undefined
      const held = document as Record<string, unknown>
      const rows = Array.isArray(held["rows"]) ? held["rows"] as ReadonlyArray<Record<string, unknown>> : []
      const byRule = new Map<string, OffTier>()
      for (const row of rows) {
        if (typeof row["rule"] !== "string") continue
        byRule.set(row["rule"], {
          fired: Number(row["fired"] ?? 0),
          silent: Number(row["silent"] ?? 0),
          unavailable: Number(row["unavailable"] ?? 0),
          evidence: Array.isArray(row["evidence"]) ? row["evidence"] as ReadonlyArray<string> : []
        })
      }
      const pass: OffTierPass = {
        at: typeof held["at"] === "string" ? held["at"] : "",
        captures: Number(held["captures"] ?? 0),
        evaluated: Number(held["evaluated"] ?? 0),
        unreadable: Number(held["unreadable"] ?? 0),
        findings: Array.isArray(held["findings"]) ? held["findings"].length : 0
      }
      return { byRule, pass }
    })

    /** The bucket every family's headline count is read from. */
    const HEADLINE: Readonly<Record<string, ReadonlyArray<string>>> = {
      tolerance: ["accepted", "known-bug", "unlisted", "unknown"],
      bless: ["fired"]
    }

    const view = Effect.fn("Rules.view")(function*(requested?: string) {
      const triage = config.triage
      const entries = yield* pack.roster.pipe(Effect.provideService(FileSystem, fs))
      if (triage === undefined) {
        return {
          available: false,
          detail: `no "triage" block in ${config.configPath}`,
          lanes: [],
          population: {
            refusalCaptures: 0,
            refusalRecords: 0,
            standingRuns: 0,
            measuredRuns: 0,
            cutCaptures: 0,
            cutCases: 0
          },
          rows: []
        } satisfies RulesView
      }
      const lane = requested ?? triage.lane
      const lanes = yield* runs.lanes.pipe(Effect.orElseSucceed(() => []))
      const refusals = yield* refusalsOf(triage.sessions)
      const offTier = yield* offTierOf(triage.sessions)
      const totals = yield* ruleHits.standingTotals(lane).pipe(Effect.orElseSucceed(() => []))
      const coverage = yield* ruleHits.standingCoverage(lane).pipe(
        Effect.orElseSucceed(() => ({ runs: 0, measured: 0 }))
      )
      const declarations = yield* declared.declared.pipe(Effect.orElseSucceed(() => []))
      const population = yield* declared.population.pipe(
        Effect.orElseSucceed(() => ({ captures: 0, cases: 0 }))
      )

      const byRule = new Map<string, Array<RuleHits.Total>>()
      for (const total of totals) {
        const key = `${total.family} ${total.rule}`
        const at = byRule.get(key) ?? []
        at.push(total)
        byRule.set(key, at)
      }
      const declaredBy = new Map(declarations.map((d) => [d.rule, d]))

      const rows: Array<RuleRow> = entries.map((entry) => {
        const buckets = (byRule.get(`${entry.family} ${entry.id}`) ?? []).map((total) => ({
          bucket: total.bucket,
          hits: total.hits,
          cells: total.cells
        }))
        const samples = (byRule.get(`${entry.family} ${entry.id}`) ?? [])
          .flatMap((total) => total.samples)
          .slice(0, SAMPLES)
        const headline = HEADLINE[entry.family] ?? []
        let hits = 0
        let cells = 0
        for (const bucket of buckets) {
          if (!headline.includes(bucket.bucket)) continue
          hits += bucket.hits
          cells = Math.max(cells, bucket.cells)
        }
        if (entry.family === "refusal") {
          const refused = refusals.byRule.get(entry.id)
          hits = refused?.hits ?? 0
          cells = refused?.captures.size ?? 0
          samples.push(...(refused?.samples ?? []))
        }
        if (entry.family === "declaration") {
          const held = declaredBy.get(entry.id)
          hits = held?.entries ?? 0
          cells = held?.cases ?? 0
        }
        const standing: Standing = hits > 0
          ? "measured"
          : entry.inert === undefined
          ? "unexercised"
          : "declared-zero"
        return {
          family: entry.family,
          id: entry.id,
          stage: entry.stage,
          tier: entry.tier,
          subject: entry.subject,
          disposition: entry.disposition,
          ...(entry.inert === undefined ? {} : { inert: entry.inert }),
          ...(entry.unfalsifiable === undefined ? {} : { unfalsifiable: entry.unfalsifiable }),
          lanes: entry.lanes,
          standing,
          hits,
          cells,
          buckets,
          samples: samples.slice(0, SAMPLES),
          ...(offTier?.byRule.get(entry.id) === undefined
            ? {}
            : { offTier: offTier.byRule.get(entry.id)! })
        } satisfies RuleRow
      })

      // A token on disk that no rule mints: either the corpus predates a rename
      // or the roster is short a rule. Never dropped — a count nothing claims is
      // the loudest reading on the page.
      const rostered = new Set(entries.map((entry) => `${entry.family} ${entry.id}`))
      for (const [reason, at] of refusals.byRule) {
        if (rostered.has(`refusal ${reason}`)) continue
        rows.push({
          family: "refusal",
          id: reason,
          stage: "pre-run",
          tier: "capture",
          subject: "unrostered",
          disposition: "unrostered",
          lanes: [],
          standing: "unrostered",
          hits: at.hits,
          cells: at.captures.size,
          buckets: [],
          samples: at.samples
        })
      }
      for (const [key, at] of byRule) {
        if (rostered.has(key)) continue
        const [family = "", ...rest] = key.split(" ")
        const rule = rest.join(" ")
        if (family !== "tolerance" && family !== "bless") continue
        rows.push({
          family,
          id: rule,
          stage: "post-run",
          tier: "run-cell",
          subject: "unrostered",
          disposition: "unrostered",
          lanes: [],
          standing: "unrostered",
          hits: at.reduce((sum, total) => sum + total.hits, 0),
          cells: at.reduce((wide, total) => Math.max(wide, total.cells), 0),
          buckets: at.map((total) => ({
            bucket: total.bucket,
            hits: total.hits,
            cells: total.cells
          })),
          samples: at.flatMap((total) => total.samples).slice(0, SAMPLES)
        })
      }

      return {
        available: true,
        detail: coverage.measured === 0
          ? `no standing run carries per-rule counts — run \`${pack.triage.ruleHits}\``
          : offTier === undefined
          ? `no cheap-falsifier pass has left a report — run \`${pack.triage.offTier}\``
          : "",
        lane,
        lanes,
        population: {
          refusalCaptures: refusals.captures,
          refusalRecords: refusals.records,
          standingRuns: coverage.runs,
          measuredRuns: coverage.measured,
          cutCaptures: population.captures,
          cutCases: population.cases
        },
        ...(offTier === undefined ? {} : { offTierPass: offTier.pass }),
        rows
      } satisfies RulesView
    })

    return Service.of({ view })
  })
)

export * as RulesCatalog from "./rules.ts"
