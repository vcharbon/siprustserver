/**
 * Which collectors run for a job and for a batch (ADR-0060 appendix §5), and
 * as which instances. A collector is wired on a lane that has every part it
 * needs and does not exclude it (`[collectors]`); the ones planned here run
 * `always`, as a required one does: those `at = "after"` over their scope's
 * cells, the pre-checks (`at = "before"`, `pre-check.ts`) before any call of
 * it. Their parameters are the manifest's after the lane's overrides.
 *
 * A batch runs one instance of each collector wired on its lane. A job runs
 * one instance of a job-scope collector that needs no part, over the cells of
 * every lane wiring it, and one per distinct set of the part files it needs
 * among the job's lanes, over the cells of the lanes naming that set, its
 * context the first of them. Instances run in the order of their `after`,
 * else of their names (`collector-order.ts`), each with the judge its
 * manifest names.
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

/** Whether `collector` runs on its own, before or after its scope's cells: `always` or required. */
const runsAlways = (collector: Collector.Collector): boolean =>
  collector.required === true || (collector.default ?? "always") === "always"

/** Whether `defined` is wired on `lane`: the lane has every part it needs and does not exclude it. */
export const isWiredOn = (defined: Defined, lane: Lanes.Loaded): boolean =>
  (defined.manifest.needs ?? []).every((kind) => lane.parts.has(kind)) && !laneWiringOf(lane).exclude.includes(defined.name)

/** The collectors of `scope` planned on `lane`, in the order they run (`collector-order.ts`). */
export const plannedOn = (rig: RigPath.RigPath, scope: Collector.Scope, lane: Lanes.Loaded): ReadonlyArray<Defined> =>
  CollectorOrder.ordered(
    [...rig.collectors.values()]
      .filter((defined) => defined.manifest.scope === scope && runsAlways(defined.manifest) && isWiredOn(defined, lane))
  )

/** `collector`'s parameters: its manifest's, each overridden where `lane` overrides it. */
export const paramsOf = (collector: Collector.Collector, lane: Lanes.Loaded | undefined): Readonly<Record<string, unknown>> => ({
  ...collector.params,
  ...(lane === undefined ? {} : laneWiringOf(lane).overrides.get(collector.name))
})

/** The instance of `defined` named `id`, under `root`, for `lane` over the cells of `lanes`. */
const instanceOf = (
  defined: Defined,
  id: string,
  label: string,
  scope: "job" | "batch",
  lane: Lanes.Loaded | undefined,
  lanes: ReadonlyArray<string> | undefined,
  root: string,
  holder: string,
  rig: RigPath.RigPath
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
  judge: defined.manifest.judge === undefined ? undefined : rig.judges.get(defined.manifest.judge)
})

/** The instances a batch on `lane` runs, each with its directory under `root`; `holder` is the batch's directory. */
export const batchPlanOf = (
  rig: RigPath.RigPath,
  lane: Lanes.Loaded,
  root: string,
  holder: string
): ReadonlyArray<CollectorRun.Instance> =>
  plannedOn(rig, "batch", lane).map((defined) => instanceOf(defined, defined.name, defined.name, "batch", lane, undefined, root, holder, rig))

/** The instances a job over `lanes` runs, each with its directory under `root`; `holder` is the job's directory. */
export const jobPlanOf = (
  rig: RigPath.RigPath,
  lanes: ReadonlyArray<Lanes.Loaded>,
  root: string,
  holder: string
): ReadonlyArray<CollectorRun.Instance> => {
  const planned: Array<CollectorRun.Instance> = []
  const names = new Set(lanes.flatMap((lane) => plannedOn(rig, "job", lane).map((defined) => defined.name)))
  for (const defined of CollectorOrder.ordered([...names].map((name) => rig.collectors.get(name)!))) {
    const name = defined.name
    const needs = defined.manifest.needs ?? []
    const groups = new Map<string, Array<Lanes.Loaded>>()
    for (const lane of lanes.filter((lane) => isWiredOn(defined, lane))) {
      const key = needs.map((kind) => lane.parts.get(kind)!.file).join("\n")
      groups.set(key, [...(groups.get(key) ?? []), lane])
    }
    for (const group of groups.values()) {
      const first = needs.length === 0 ? undefined : group[0]
      const id = first === undefined ? name : `${name}-${first.id}`
      const label = first === undefined ? name : `${name} for ${first.id}`
      planned.push(instanceOf(defined, id, label, "job", first, group.map((lane) => lane.id), root, holder, rig))
    }
  }
  return planned
}
