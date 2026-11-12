/**
 * Which judges a batch runs over its exports alone (ADR-0060 appendix §4):
 * on a lane where no collector of a name reads what a scope did, at the
 * batch's or the job's scope, or at a cell's on a real-clock lane (a
 * pre-check never does), the name's exports are judged as the path's
 * collector of that name, wired or not, states: by the judge it names,
 * answering what it answers, its records joined on its `[join] key`. A name
 * no collector of the path bears, or whose collector names no judge, is judged
 * by the judge of the path of that name, answering its name, joined on its
 * collector's `[join] key`, else on {@link Cell.CALL_ID}.
 */
import * as Cell from "./cell.js"
import * as Collector from "./collector.js"
import * as CollectorPlan from "./collector-plan.js"
import type * as Lanes from "./lanes.js"
import type * as RigPath from "./rig-path.js"
import type * as ScopeJudging from "./scope-judging.js"

/** The export judge of the name `name` on `rig`, where the path holds one. */
const exportJudgeOf = (rig: RigPath.RigPath, name: string): ScopeJudging.ExportJudge | undefined => {
  const collector = rig.collectors.get(name)?.manifest
  const key = collector?.join?.key ?? Cell.CALL_ID
  const named = collector?.judge === undefined ? undefined : rig.judges.get(collector.judge)
  if (collector !== undefined && named !== undefined) return { name, judge: named, key, answers: Collector.answeredBy(collector) }
  const own = rig.judges.get(name)
  return own === undefined ? undefined : { name, judge: own, key, answers: [name] }
}

/** The judges of `rig` a batch on `lane`, whose directory is `holder`, runs over its exports alone, by the name they judge. */
export const exportJudgesOn = (rig: RigPath.RigPath, lane: Lanes.Loaded, holder: string): ScopeJudging.ExportJudges => {
  const scopes = lane.lane.clock === "real" ? (["cell", "batch", "job"] as const) : (["batch", "job"] as const)
  const running = new Set(
    scopes.flatMap((scope) =>
      CollectorPlan.plannedOn(rig, scope, lane).filter((defined) => defined.manifest.at !== "before").map((defined) => defined.name)
    )
  )
  const names = [...new Set([...rig.collectors.keys(), ...rig.judges.keys()])]
    .filter((name) => !running.has(name))
    .sort()
  const judges = names.flatMap((name) => exportJudgeOf(rig, name) ?? [])
  return { judges, holder, lane }
}
