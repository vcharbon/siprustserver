/**
 * The provisioned end-to-end campaigns as the summary pages read them: every
 * campaign this server knows ({@link listOf}), those the deployment defines read
 * anew at each request (the site pack's campaign facts), and one campaign's
 * latest runs with each lane's results and their failed checks
 * (`campaign-runs.ts`).
 * A case not ported is counted per lane, never listed. The pack names the
 * side vocabulary of a failed check, the label of what the decision said and
 * the family each case is listed under.
 *
 * The latest full runs and the latest partial ones are read apart, so the
 * replays of one case never push the full runs out of the page. The page may
 * start runs of a campaign this deployment runs ({@link startsIn}) where it
 * reads the server's own environment, which it is told: it is handed every
 * lane, its group and why this environment does not run it, if it does not
 * (`@sip/campaign-runner` `runnable.ts` over the pack's refusal), and the
 * lanes and tags the campaign states.
 *
 * One environment's runs are read at a time: the one asked, else this
 * server's own where it starts the campaign or wrote any, else the latest writer. A run asked for by
 * id is among them whatever its age.
 *
 * A case reads as the newest run that compiled it says: its title,
 * description, labels, the objects it uses and the case file it was compiled
 * from. Each run states every check each case it compiled makes, so a run's
 * marks read against its own checks. Each object is linked by the newest run
 * whose seed left links, on the config's link base where it states one, else
 * on the base that run stored; an object that seed has no link of is not
 * linked.
 *
 * A cell that ran states its status (`Results.statusOf`) and its
 * verification lines; a lane counts the cells that did not run by status. A
 * cell replayed more than once on its lane lists every attempt, each by its
 * bundle directory, the kept one (the result) marked.
 *
 * A cell's files are its attachments (`attachments.ts`), read by the run and
 * case the lane stores it under; the view names the one a check's `said` was
 * read from. Where the cell holds no such log, the store's mark says why.
 */
import { CampaignResults as Results } from "@sip/store"
import type { Checked } from "@sip/campaign-runner/cell"
import * as CampaignLanes from "@sip/campaign-runner/lanes"
import * as Runnable from "@sip/campaign-runner/runnable"
import { excludedOn } from "@sip/campaign-runner/selection"
import { Identity } from "@sip/corpus"
import type { LaneCatalog, LaneView } from "@sip/rig"
import { Sessions } from "@sip/store"
import type { CampaignTags } from "@sip/workspace"
import { Effect, Layer } from "effect"
import * as Context from "effect/Context"
import { FileSystem } from "effect/FileSystem"
import * as path from "node:path"
import { type CampaignListed, type CampaignView, lineViewOf } from "./api.ts"
import { CampaignRuns } from "./campaign-runs.ts"
import { ViewerConfig } from "./config.ts"
import { SitePack } from "./site-pack.ts"

/** The most runs a campaign read answers. */
export const MAX_RUNS = 60

/** How many runs a campaign read answers where it is asked no number. */
export const DEFAULT_RUNS = 30

export type View = typeof CampaignView.Type

/** What the view reads of the pack: its sides, its decision's label and log, the family of a case. */
export type Facts = Pick<SitePack.CampaignFacts, "sides" | "saidLabel" | "saidLog" | "familyOf">

/** What a campaign read is asked: how many runs, of which environment, which run among them. */
export interface Query {
  readonly limit?: number | undefined
  readonly env?: string | undefined
  readonly run?: number | undefined
}

/** Every campaign this server knows: the `runnable` ones, whether or not they ever ran, and the `stored` ones whose bundles it reads, each once, sorted. */
export const listOf = (runnable: ReadonlyArray<string>, stored: ReadonlyArray<string>): ReadonlyArray<string> =>
  [...new Set([...runnable, ...stored])].sort()

/** The environment the server starts runs of `capture` in: its `own`, for a `runnable` campaign; null where it starts none. */
export const startsIn = (capture: string, own: string | null, runnable: ReadonlyArray<string>): string | null =>
  own !== null && runnable.includes(capture) ? own : null

/**
 * The campaigns a page may start: those `defined`; where the file does not read,
 * the one `asked`, so its start stays offered and the launch refuses naming the file.
 */
export const runnableOf = (defined: ReadonlyArray<{ readonly name: string }> | undefined, asked: string): ReadonlyArray<string> =>
  defined === undefined ? [asked] : defined.map((one) => one.name)

/** A campaign as the list of campaigns reads it. */
export type Listed = typeof CampaignListed.Type

export interface Interface {
  /** Every campaign this server knows ({@link listOf}). */
  readonly list: Effect.Effect<ReadonlyArray<string>>
  /** The same campaigns, each with its title. */
  readonly listed: Effect.Effect<ReadonlyArray<Listed>>
  readonly read: (capture: string, query: Query) => Effect.Effect<View>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/Campaign") {}

const short = (sha: string | null): string | null => (sha === null ? null : sha.slice(0, 10))

type CaseView = View["cases"][number]

/**
 * What the newest run in `runs` (newest first) that compiled `ref` says of it,
 * its objects linked by the newest run whose seed left links, on the link
 * `base` where stated; nothing where none compiled it.
 */
const describedOf = (
  ref: string,
  runs: ReadonlyArray<CampaignRuns.Run>,
  base: string | undefined
): Omit<CaseView, "caseId" | "ref" | "subcase" | "family"> => {
  const about = runs.find((r) => r.about[ref] !== undefined)?.about[ref]
  if (about === undefined) return { title: null, description: null, groups: [], tags: [], links: [], source: null }
  const linkOf = runs.find((r) => r.linkOf !== undefined)?.linkOf
  return {
    title: about.title,
    description: about.description,
    groups: about.groups,
    tags: about.tags,
    source: about.source,
    links: about.uses.map((used) => ({
      role: used.role,
      label: used.label,
      url: used.object === null || linkOf === undefined ? null : linkOf(used.object, base)
    }))
  }
}

/** Every check each case `run` compiled makes, by case id. */
const checkedOf = (run: CampaignRuns.Run): Record<string, ReadonlyArray<Checked>> =>
  Object.fromEntries(
    Object.entries(run.about).flatMap(([ref, about]) =>
      Object.entries(about.checked).map(([subcase, list]) => [Results.caseIdOf(ref, subcase), list] as const)
    )
  )

/** One stored run as the pages read it, in the pack's terms `facts`, its object links on the link `base` where stated. */
export const viewOf = (
  facts: Facts,
  capture: string,
  runs: ReadonlyArray<CampaignRuns.Run>,
  env: string | null = null,
  envs: ReadonlyArray<string> = [],
  launch: View["launch"] = null,
  own: string | null = null,
  base: string | undefined = undefined,
  title: string | null = null
): View => {
  const cases = new Map<string, View["cases"][number]>()
  const lanes = new Set<string>()
  const viewed = runs.map((run) => {
    const components = run.lanes[0]?.components ?? []
    const labels = [
      ["env", run.env],
      ["branch", run.branch],
      ["seed", run.seed],
      ...run.labels,
      ...components.map((c) => [c.name, short(c.sha)] as const)
    ].filter((pair): pair is readonly [string, string] => pair[1] !== null)
    return {
      id: run.id,
      env: run.env,
      startedAt: run.startedAt,
      finishedAt: run.finishedAt,
      labels: labels.map(([name, value]) => ({ name, value })),
      checked: checkedOf(run),
      scope: run.scope,
      selection: run.selection,
      lanes: run.lanes.map((lane) => {
        lanes.add(lane.lane)
        const ran = lane.results.filter((r) => Results.outcomeOf(r) !== "skipped")
        const excluded = lane.results.filter(Results.isExcluded)
        for (const r of [...ran, ...excluded]) {
          if (cases.has(r.caseId)) continue
          const { ref, subcase } = Results.caseNameOf(r.caseId)
          cases.set(r.caseId, {
            caseId: r.caseId,
            ref,
            subcase: subcase ?? null,
            family: facts.familyOf(ref),
            ...describedOf(ref, runs, base)
          })
        }
        const notRun: Record<string, number> = {}
        const didNotRun = lane.results.flatMap((r) =>
          r.skipped === null || Results.ranUnchecked(r.skipped) ? [] : [Results.notRunOf(r.skipped)]
        )
        for (const status of didNotRun) {
          notRun[status] = (notRun[status] ?? 0) + 1
        }
        return {
          lane: lane.lane,
          runId: lane.runId,
          session: path.basename(lane.bundlePath),
          status: lane.status,
          detail: lane.detail,
          skipped: lane.results.length - ran.length,
          excluded: excluded.map((r) => ({ caseId: r.caseId, reason: r.skipped ?? "" })),
          notRun,
          results: ran.map((r) => ({
            caseId: r.caseId,
            outcome: Results.outcomeOf(r) as "pass" | "failure" | "error",
            cell: r.cell,
            log: r.log === null ? null : "records" in r.log ? { records: r.log.records } : { missing: r.log.missing },
            detail: r.errorDetail,
            status: Results.statusOf(r),
            lines: r.verifications.map(lineViewOf),
            ...(r.skipped !== null && Results.ranUnchecked(r.skipped) ? { unchecked: r.skipped } : {}),
            checks: r.checks,
            ...(r.attempts === undefined ? {} : { attempts: r.attempts })
          }))
        }
      })
    }
  })
  return {
    capture,
    title,
    env,
    envs: [...envs],
    saidLabel: facts.saidLabel,
    saidLog: facts.saidLog,
    sides: [...facts.sides],
    lanes: [...lanes].sort(),
    runs: viewed,
    cases: [...cases.values()].sort((a, b) => (a.caseId < b.caseId ? -1 : a.caseId > b.caseId ? 1 : 0)),
    launch,
    own
  }
}

/** The lanes of `catalog` a page start may name, under the pack's `refusal` here: each lane's group and its refusal. */
export const launchLanes = (
  refusal: Runnable.Refusal<LaneView.View>,
  catalog: LaneCatalog.Catalog
): NonNullable<View["launch"]>["lanes"] =>
  Runnable.standingsOf(refusal, catalog).map((one) => ({ lane: one.id, group: one.group, refused: one.refused }))

/** The tags `declared` names, each with what the page offers it as, the lanes of `catalog` excluding it, and whether the campaign's `allow` opts into it. */
export const launchTags = (
  declared: CampaignTags.Declared,
  catalog: LaneCatalog.Catalog,
  allow: ReadonlyArray<string> = []
): NonNullable<View["launch"]>["tags"] =>
  Object.entries(declared.tags).map(([tag, stated]) => ({
    tag,
    label: stated.label,
    warning: stated.warning ?? null,
    excludedOn: catalog.lanes.filter((lane) => excludedOn(declared, lane).includes(tag)).map((lane) => lane.id),
    allowed: allow.includes(tag)
  }))

/** The runs a read answers: the latest full ones and, apart, the latest partial ones, newest first, each once. */
export const mergedRuns = <R extends { readonly id: number }>(full: ReadonlyArray<R>, partial: ReadonlyArray<R>): ReadonlyArray<R> =>
  [...new Map([...full, ...partial].map((run) => [run.id, run])).values()].sort((a, b) => b.id - a.id)

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const results = yield* CampaignRuns.Service
    const pack = yield* SitePack.Service
    const facts = pack.campaigns
    const laneCatalog = pack.lanes
    const root = yield* Sessions.Service
    const fs = yield* FileSystem

    const definitions = Effect.suspend(() => facts.defined(process.env)).pipe(
      Effect.provideService(FileSystem, fs),
      Effect.orElseSucceed(() => undefined)
    )

    const own = (yield* Identity.Service).environment

    // The store is box-wide; a workspace's bundles live under its own root, out of this server's reach.
    const stored = results.captures.pipe(
      Effect.flatMap((captures) =>
        Effect.filter(captures, (capture) => fs.exists(path.join(root.dir, capture)).pipe(Effect.orElseSucceed(() => false)))
      ),
      Effect.orElseSucceed(() => [] as ReadonlyArray<string>)
    )

    const listed = Effect.gen(function*() {
      const defined = yield* definitions
      const names = listOf(runnableOf(defined, facts.defaultName), yield* stored)
      return names.map((capture) => ({ capture, title: defined?.find((one) => one.name === capture)?.title ?? null }))
    })

    const base = (yield* ViewerConfig.Service).linkBase

    const read = (capture: string, query: Query) =>
      Effect.gen(function*() {
        const envs = yield* results.envs(capture)
        const defined = yield* definitions
        const starts = startsIn(capture, own, runnableOf(defined, capture))
        const env = query.env ?? starts ?? (own !== null && envs.includes(own) ? own : envs[0])
        const asked = {
          limit: Math.min(MAX_RUNS, Math.max(1, query.limit ?? DEFAULT_RUNS)),
          ...(env === undefined ? {} : { env }),
          ...(query.run === undefined ? {} : { include: query.run })
        }
        // Partial runs are read apart, so replays of one case never push the full runs out.
        const runs = mergedRuns(
          yield* results.read(capture, { ...asked, partial: false }),
          yield* results.read(capture, { ...asked, partial: true })
        )
        const mine = defined?.find((one) => one.name === capture)
        const launch = starts !== null && (env === undefined || env === starts)
          ? {
            lanes: launchLanes(yield* facts.refusal(process.env).pipe(Effect.provideService(FileSystem, fs)), laneCatalog),
            // Declarations that do not read offer nothing: the launch itself refuses, naming the file.
            tags: launchTags(
              yield* facts.tags(process.env).pipe(
                Effect.provideService(FileSystem, fs),
                Effect.orElseSucceed((): CampaignTags.Declared => ({ file: "", tags: {}, tagCases: [] }))
              ),
              laneCatalog,
              mine?.allow
            ),
            defaultLanes: mine?.lanes?.map((lane) => CampaignLanes.resolve(facts.lanePrefix, laneCatalog, lane)) ?? null,
            collection: yield* facts.levels(process.env).pipe(Effect.provideService(FileSystem, fs))
          }
          : null
        return viewOf(facts, capture, runs, env ?? null, envs, launch, starts, base, mine?.title ?? null)
      }).pipe(Effect.orDie)

    return Service.of({
      list: Effect.map(listed, (rows) => rows.map((row) => row.capture)),
      listed,
      read
    })
  })
)

export * as Campaign from "./campaign.ts"
