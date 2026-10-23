/**
 * What the replay bench reads off its children's logs: the prep a cut
 * recorded, and the sweep and run bundles a replay opened.
 */
import { describe, expect, it } from "vitest"
import * as BenchLines from "../../src/bench/bench-lines.js"

describe("BenchLines.prepOf", () => {
  it("is the prep the cut's closing line names", () => {
    const log = [
      "[cut] prep61: 1 capture(s) in 1 batch(es) of 1000 -> /x/_preps/prep61 (census c.json)",
      "[cut] prep61 capture-a.pcap: cut (64 case(s))",
      "[cut] prep61 done: cut=1 no-cases=0 errors=0 rows=1 rows-failed=0"
    ].join("\n")
    expect(BenchLines.prepOf(log)).toBe(61)
  })

  it("is undefined where the cut never closed", () => {
    expect(BenchLines.prepOf("[cut] refused: no census run")).toBeUndefined()
  })
})

describe("BenchLines.replayOf", () => {
  it("reads the sweep and every run bundle with its cell count", () => {
    const log = [
      "[replay] sweep 17: 2 capture(s), 65 cell(s) on lane fake from prep61 in 2 batch(es)",
      "capture-a.pcap  run4242: cells=64 failed=3 accepted=1 known-bug=0 unlisted=0 unknown=0",
      "capture_2.pcap.gz  run324243: cells=1 failed=0 skipped=1 accepted=0 known-bug=0 unlisted=0 unknown=0",
      "capture_3.pcap.gz  0 cases (the cut refused every call)",
      "",
      "[replay] sweep 17: 2 batch(es), done=2 no-cases=1 errors=0"
    ].join("\n")
    expect(BenchLines.replayOf(log)).toEqual({
      sweep: 17,
      runs: [
        { capture: "capture-a.pcap", run: "run4242", cells: 64 },
        { capture: "capture_2.pcap.gz", run: "run324243", cells: 1 }
      ]
    })
  })

  it("has no sweep where the replay never opened one", () => {
    expect(BenchLines.replayOf("[replay] refused: no prep")).toEqual({ sweep: undefined, runs: [] })
  })
})
