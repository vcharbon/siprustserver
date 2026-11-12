/**
 * A cell's lane timers under the pack's lane-file keys: resolved to the first
 * lane of the run admitting the case's needs and stating the timer, refused
 * naming the lanes that state none, and a cell skipped on a lane stating
 * another value than it was compiled with.
 */
import { LaneView } from "@sip/rig"
import { describe, expect, it } from "vitest"
import type { Cell } from "../src/cell.js"
import { laneTimer, unrunnableOf } from "../src/timers.js"

const KEYS = { idle: "idle_ms" }
const NONE = LaneView.simulatedBuiltIn("bare", {})
const SHORT = LaneView.simulatedBuiltIn("short", { idle_ms: 500 })
const LONG = LaneView.simulatedBuiltIn("long", { idle_ms: 9000 })

describe("a lane timer", () => {
  it("resolves to the first lane admitting the case's needs that states it", () => {
    expect(laneTimer(KEYS, [NONE, SHORT, LONG])("idle", [])).toEqual({ ms: 500 })
  })

  it("is unresolved naming the admitted lanes that state none, or no lane admitting the needs", () => {
    expect(laneTimer(KEYS, [NONE])("idle", [])).toEqual({ unresolved: "lane bare states no timers.idle_ms" })
    expect(laneTimer(KEYS, [SHORT])("idle", ["no-such-capability"]))
      .toEqual({ unresolved: "no lane of the run admits the case's needs" })
  })
})

describe("a cell compiled with a lane timer", () => {
  const cell = { id: "c", laneTimers: { idle: 500 } } as unknown as Cell

  it("runs on a lane stating the same value", () => {
    expect(unrunnableOf(KEYS, SHORT, cell)).toBeUndefined()
  })

  it("is not runnable on a lane stating another value or none, naming it", () => {
    expect(unrunnableOf(KEYS, LONG, cell)).toBe(
      "not-runnable: long states timers.idle_ms 9000; the cell was compiled with idle 500 ms (run it on such a lane alone)"
    )
    expect(unrunnableOf(KEYS, NONE, cell)).toBe(
      "not-runnable: bare states no timers.idle_ms; the cell was compiled with idle 500 ms (run it on such a lane alone)"
    )
  })
})
