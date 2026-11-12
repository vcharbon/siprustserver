/**
 * Which judges a batch runs over its exports alone (ADR-0060 appendix §4):
 * on a lane where no collector of a name reads what a scope did, at the
 * batch's or the job's scope, or at a cell's on a real-clock lane (a
 * pre-check never does), the judge of the path of that name judges the name's exports. Its
 * records join on the `[join] key` of the path's collector of that name,
 * wired or not, else on {@link Cell.CALL_ID}.
 */
import * as Cell from "./cell.js"
import * as CollectorPlan from "./collector-plan.js"
import type * as Lanes from "./lanes.js"
import type * as RigPath from "./rig-path.js"
import type * as ScopeJudging from "./scope-judging.js"

/** The judges of `rig` a batch on `lane`, whose directory is `holder`, runs over its exports alone. */
export const exportJudgesOn = (rig: RigPath.RigPath, lane: Lanes.Loaded, holder: string): ScopeJudging.ExportJudges => {
  const scopes = lane.lane.clock === "real" ? (["cell", "batch", "job"] as const) : (["batch", "job"] as const)
  const running = new Set(
    scopes.flatMap((scope) =>
      CollectorPlan.plannedOn(rig, scope, lane).filter((defined) => defined.manifest.at !== "before").map((defined) => defined.name)
    )
  )
  const judges = [...rig.judges.values()]
    .filter((judge) => !running.has(judge.name))
    .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0))
    .map((judge) => ({ judge, key: rig.collectors.get(judge.name)?.manifest.join?.key ?? Cell.CALL_ID }))
  return { judges, holder, lane }
}
