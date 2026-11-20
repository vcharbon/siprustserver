/**
 * The lanes the triage queue offers: every lane of the catalog, a lane no run
 * was made on yet included with nothing counted, so a capture can be replayed
 * on it from the queue; a lane only the run database still knows comes after.
 */
import { describe, expect, it } from "vitest"
import { lanesOffered } from "../src/triage.ts"

describe("the lanes the triage queue offers", () => {
  it("are the catalog's in its order, counted where runs were made, then the run database's others", () => {
    expect(lanesOffered(
      ["simulated", "hosted-a", "hosted-b"],
      [{ lane: "retired", runs: 3, captures: 2 }, { lane: "simulated", runs: 1, captures: 1 }]
    )).toEqual([
      { lane: "simulated", runs: 1, captures: 1 },
      { lane: "hosted-a", runs: 0, captures: 0 },
      { lane: "hosted-b", runs: 0, captures: 0 },
      { lane: "retired", runs: 3, captures: 2 }
    ])
  })
})
