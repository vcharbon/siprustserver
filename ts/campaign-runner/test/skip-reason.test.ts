/**
 * Why a cell is skipped on a lane, over a fixture pack's refusals, in the
 * order the runner asks: a tag excluding it first (unless the case runs on its
 * own), then the pack's refusal of the lane, then an operation the lane binds
 * none of, then a need the lane lacks (unless forced), then a lane timer the
 * lane does not state; the pack's refusal of the target last, and alone where
 * the lane is none of the catalog's.
 */
import { LaneView } from "@sip/rig"
import { describe, expect, it } from "vitest"
import type { Cell } from "../src/cell.js"
import { skipReasonOf } from "../src/phases.js"

/** A pack refusing the lane for a cell tagged `lane-refused`, and the target for one tagged `target-refused`. */
const PACK = {
  laneUnrunnable: (lane: LaneView.View, cell: Cell) =>
    cell.tags.includes("lane-refused") ? `not-runnable: the pack refuses ${lane.id}` : undefined,
  targetUnrunnable: (cell: Cell) => (cell.tags.includes("target-refused") ? "not-runnable: the pack's target is down" : undefined)
}

const LANE: LaneView.View = {
  ...LaneView.simulatedBuiltIn("sim", {}),
  tags: { run: [], exclude: ["slow"] },
  capabilities: new Set(["sip"])
}

const TAGGING = {
  declared: { file: "tool.toml", tags: { slow: { label: "Slow", default: "run" as const } }, tagCases: [] },
  allow: []
}

const OPTIONS = { force: false, individually: false, tagging: TAGGING }

const cell = (over: Partial<Cell> = {}): Cell => ({
  id: "c",
  ref: "Routing/c",
  subcase: "s",
  retries: 0,
  source: { file: "cases/c.ts", text: "" },
  checked: [],
  locks: [],
  needs: [],
  tags: [],
  alone: false,
  heal: { healMs: 0, healRealClockOnSimulatedMs: 0 },
  dir: "/cells/c",
  endpoints: ["alice", "bob"],
  egress: "bob",
  ...over
})

const reason = (c: Cell, options: Partial<typeof OPTIONS> = {}) => skipReasonOf(PACK, {}, { ...OPTIONS, ...options }, LANE, c)

describe("why a cell is skipped on a lane", () => {
  it("is none where nothing refuses it", () => {
    expect(reason(cell())).toBeUndefined()
  })

  it("is the tag excluding it first, before the pack's lane refusal and a missing need", () => {
    expect(reason(cell({ tags: ["slow", "lane-refused"], needs: ["video"] }))).toMatch(/^excluded: slow on sim /)
  })

  it("is the pack's lane refusal where no tag excludes it, before a need the lane lacks", () => {
    expect(reason(cell({ tags: ["slow", "lane-refused"], needs: ["video"] }), { individually: true }))
      .toBe("not-runnable: the pack refuses sim")
  })

  it("is an operation the lane binds none of where the pack runs it, before a need the lane lacks, forced or not", () => {
    const asking = cell({ operations: ["block-route", "block-route"], needs: ["video"] })
    const unbound = "not-runnable: sim binds no operation block-route the case Routing/c asks for"
    expect(reason(asking)).toBe(unbound)
    expect(reason(asking, { force: true })).toBe(unbound)
    expect(reason(cell({ operations: ["block-route"], tags: ["lane-refused"] }))).toBe("not-runnable: the pack refuses sim")
  })

  it("is a need the lane lacks where the pack runs it, and none where the run is forced", () => {
    expect(reason(cell({ needs: ["video"] }))).toMatch(/^not-runnable: .*video/)
    expect(reason(cell({ needs: ["video"] }), { force: true })).toBeUndefined()
  })

  it("is a lane timer the lane does not state, after every need is met", () => {
    expect(reason(cell({ laneTimers: { ring: 5000 } }))).toMatch(/^not-runnable: sim states no timers\.ring; .* ring 5000 ms/)
  })

  it("is the pack's target refusal where the lane runs it, and alone where the lane is no catalog lane", () => {
    expect(reason(cell({ tags: ["target-refused"] }))).toBe("not-runnable: the pack's target is down")
    expect(skipReasonOf(PACK, {}, OPTIONS, undefined, cell({ tags: ["slow", "lane-refused", "target-refused"] })))
      .toBe("not-runnable: the pack's target is down")
  })
})
