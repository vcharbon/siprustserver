/**
 * Which collectors run for a job and for a batch (ADR-0060 appendix §5, §10),
 * and as which instances. A collector is wired on a lane that has every part
 * it needs and does not exclude it (`[collectors]`); the job's collection
 * level (`Collection.Settings`) sets each one, a required one `always`. The
 * ones planned here are those set `always`, run over their scope's cells
 * where `at = "after"` and before any call of it where a pre-check
 * (`at = "before"`, `pre-check.ts`); and those set `on-failure` and
 * `at = "after"`, run over the failed cells alone. A job also runs each
 * job-scope collector it asks for by name, marked asked, whatever the level
 * sets it: a level cannot switch off what the job asks for.
 * Their parameters are the manifest's after the lane's overrides.
 *
 * A batch runs one instance of each collector wired on its lane. A job runs
 * one instance of a job-scope collector that needs no part, over the cells of
 * every lane wiring it, and one per distinct set of the part files it needs
 * among the job's lanes, over the cells of the lanes naming that set, its
 * context the first of them. Instances open and close in the order of their
 * `after`, else of their names (`collector-order.ts`); the `on-failure` ones
 * collect after the others where their `after` lets them (`CollectorRun`).
 * Each runs with the judge its manifest names.
 *
 * An `on-demand` collector is planned when the site or the CLI asks for it,
 * one instance over the cells it names on one lane ({@link onDemandOf}).
 */
import * as Result from "effect/Result"
import * as path from "node:path"
import * as Collection from "./collection.js"
import type * as Collector from "./collector.js"
import * as CollectorOrder from "./collector-order.js"
import type * as CollectorRun from "./collector-run.js"
import type * as Lanes from "./lanes.js"
import type * as RigPath from "./rig-path.js"

type Defined = RigPath.Defined<Collector.Collector>

/** The lane's `[collectors]` as read; nothing excluded or overridden where it does not hold, which the job's wiring check refused. */
const laneWiringOf = (lane: Lanes.Loaded) => {
  const read = Collection.laneOf(lane.lane.collectors)
  return Result.isSuccess(read) ? read.success : { exclude: [], overrides: new Map<string, Readonly<Record<string, unknown>>>() }
}

/**
 * Whether `collector` runs on its own under `settings`: set `always`, before
 * or after its scope's cells, or set `on-failure` and `at = "after"`; a
 * pre-check reads no failure, so none set `on-failure` runs.
 */
const runsOnItsOwn = (collector: Collector.Collector, settings: Collection.Settings): boolean => {
  const setting = settings(collector)
  return setting === "always" || (setting === "on-failure" && (collector.at ?? "after") === "after")
}

/** Whether `defined` is wired on `lane`: the lane has every part it needs and does not exclude it. */
export const isWiredOn = (defined: Defined, lane: Lanes.Loaded): boolean =>
  (defined.manifest.needs ?? []).every((kind) => lane.parts.has(kind)) && !laneWiringOf(lane).exclude.includes(defined.name)

/** Whether `defined` runs because its scope asked for it: named in `asked`, whatever the level sets it. */
const isAsked = (defined: Defined, asked: ReadonlyArray<string>): boolean => asked.includes(defined.name)

/**
 * The collectors of `scope` planned on `lane` under `settings`, in the order
 * they run (`collector-order.ts`): those that run on their own, and those
 * `asked` names.
 */
export const plannedOn = (
  rig: RigPath.RigPath,
  scope: Collector.Scope,
  lane: Lanes.Loaded,
  asked: ReadonlyArray<string> = [],
  settings: Collection.Settings = Collection.STANDARD_SETTINGS
): ReadonlyArray<Defined> =>
  CollectorOrder.ordered(
    [...rig.collectors.values()]
      .filter((defined) =>
        defined.manifest.scope === scope &&
        (runsOnItsOwn(defined.manifest, settings) || isAsked(defined, asked)) &&
        isWiredOn(defined, lane)
      )
  )

/** `collector`'s parameters: its manifest's, each overridden where `lane` overrides it. */
export const paramsOf = (collector: Collector.Collector, lane: Lanes.Loaded | undefined): Readonly<Record<string, unknown>> => ({
  ...collector.params,
  ...(lane === undefined ? {} : laneWiringOf(lane).overrides.get(collector.name))
})

/** The instance of `defined` named `id`, under `root`, for `lane` over the cells of `lanes`; `onFailure` where it collects only failed cells. */
const instanceOf = (
  defined: Defined,
  id: string,
  label: string,
  scope: "job" | "batch",
  lane: Lanes.Loaded | undefined,
  lanes: ReadonlyArray<string> | undefined,
  root: string,
  holder: string,
  rig: RigPath.RigPath,
  onFailure: boolean
): CollectorRun.Instance => ({
  collector: defined,
  id,
  label,
  scope,
  lane,
  lanes,
  params: paramsOf(defined.manifest, lane),
  dir: path.join(root, id),
  holder,
  judge: defined.manifest.judge === undefined ? undefined : rig.judges.get(defined.manifest.judge),
  ...(onFailure ? { onFailure } : {})
})

/** The instances a batch on `lane` runs under `settings`, each with its directory under `root`; `holder` is the batch's directory. */
export const batchPlanOf = (
  rig: RigPath.RigPath,
  lane: Lanes.Loaded,
  root: string,
  holder: string,
  settings: Collection.Settings = Collection.STANDARD_SETTINGS
): ReadonlyArray<CollectorRun.Instance> =>
  plannedOn(rig, "batch", lane, [], settings).map((defined) =>
    instanceOf(defined, defined.name, defined.name, "batch", lane, undefined, root, holder, rig, settings(defined.manifest) === "on-failure")
  )

/**
 * The instances a job over `lanes` runs under `settings`, each with its
 * directory under `root`; `holder` is the job's directory, `asked` the
 * `on-demand` collectors the job asks for.
 */
export const jobPlanOf = (
  rig: RigPath.RigPath,
  lanes: ReadonlyArray<Lanes.Loaded>,
  root: string,
  holder: string,
  asked: ReadonlyArray<string> = [],
  settings: Collection.Settings = Collection.STANDARD_SETTINGS
): ReadonlyArray<CollectorRun.Instance> => {
  const planned: Array<CollectorRun.Instance> = []
  const names = new Set(lanes.flatMap((lane) => plannedOn(rig, "job", lane, asked, settings).map((defined) => defined.name)))
  for (const defined of CollectorOrder.ordered([...names].map((name) => rig.collectors.get(name)!))) {
    const name = defined.name
    const needs = defined.manifest.needs ?? []
    const isAskedFor = isAsked(defined, asked)
    const onFailure = !isAskedFor && settings(defined.manifest) === "on-failure"
    const groups = new Map<string, Array<Lanes.Loaded>>()
    for (const lane of lanes.filter((lane) => isWiredOn(defined, lane))) {
      const key = needs.map((kind) => lane.parts.get(kind)!.file).join("\n")
      groups.set(key, [...(groups.get(key) ?? []), lane])
    }
    for (const group of groups.values()) {
      const first = needs.length === 0 ? undefined : group[0]
      const id = first === undefined ? name : `${name}-${first.id}`
      const label = first === undefined ? name : `${name} for ${first.id}`
      const instance = instanceOf(defined, id, label, "job", first, group.map((lane) => lane.id), root, holder, rig, onFailure)
      planned.push(isAskedFor ? { ...instance, asked: true } : instance)
    }
  }
  return planned
}

/** The collectors `settings` sets `on-demand` that an on-demand run on `lane` may ask for, by name. */
export const onDemandOn = (
  rig: RigPath.RigPath,
  lane: Lanes.Loaded,
  settings: Collection.Settings = Collection.STANDARD_SETTINGS
): ReadonlyArray<string> =>
  [...rig.collectors.values()]
    .filter((defined) => onDemandRefusal(defined, lane, settings) === undefined)
    .map((defined) => defined.name)
    .sort()

/** Why `defined` cannot run on demand on `lane` under `settings`, or `undefined`. */
const onDemandRefusal = (defined: Defined, lane: Lanes.Loaded, settings: Collection.Settings): string | undefined => {
  const manifest = defined.manifest
  if ((manifest.at ?? "after") !== "after") return `${defined.name} is a pre-check, which no on-demand run serves`
  if (manifest.scope === "cell") return `${defined.name} is of cell scope, which no on-demand run serves`
  if (!isWiredOn(defined, lane)) return `${defined.name} is not wired on the lane ${lane.id}`
  const setting = settings(manifest)
  return setting === "on-demand" ? undefined : `${defined.name} is set ${setting}, not on-demand`
}

/**
 * The instance an on-demand run of the collector `name` takes over cells of
 * `lane`, its directory under `root`, its own attachments under `holder`; or
 * why it cannot run: no collector of the path is so named, it is a
 * pre-check or of cell scope, it is not wired on the lane, or `settings` does
 * not set it `on-demand`. Its scope is its manifest's; its context's lane is
 * `lane`.
 */
export const onDemandOf = (
  rig: RigPath.RigPath,
  name: string,
  lane: Lanes.Loaded,
  root: string,
  holder: string,
  settings: Collection.Settings = Collection.STANDARD_SETTINGS
): Result.Result<CollectorRun.Instance, string> => {
  const defined = rig.collectors.get(name)
  if (defined === undefined) return Result.fail(`no collector of the path is named ${name}`)
  const refused = onDemandRefusal(defined, lane, settings)
  if (refused !== undefined) return Result.fail(refused)
  const scope = defined.manifest.scope === "job" ? "job" : "batch"
  return Result.succeed(instanceOf(defined, name, `${name} on demand`, scope, lane, [lane.id], root, holder, rig, false))
}
