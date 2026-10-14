/**
 * The `[timing]` line: what a phase of the cut or the ingest says about its own
 * duration, and what the replay bench reads back off the logs.
 */
import { describe, expect, it } from "vitest"
import * as Timing from "../src/timing.js"

describe("Timing.format and Timing.parse", () => {
  it("round-trip a line with its optional fields", () => {
    const line: Timing.TimingLine = {
      phase: "generate3.capture",
      ms: 1234,
      capture: "capture_a.pcap.gz",
      cases: 3,
      maxRssKb: 81_920,
      steps: { read: 100, cut: 200, assemble: 800, write: 134 }
    }
    expect(Timing.parse(Timing.format(line))).toEqual([line])
  })

  it("reads the tagged lines out of a log and skips everything else", () => {
    const log = [
      "=== generate3 exit 0 ===",
      "[v3] census: 92 rule(s) covered",
      Timing.format({ phase: "generate3.load", ms: 50 }),
      "[v3] capture_a.pcap.gz: SUT 10.0.0.1",
      "[timing] {not json",
      "[timing] {\"phase\":\"no-ms\"}",
      `  ${Timing.format({ phase: "cut.mirror", ms: 7, prep: 9 })}`
    ].join("\n")
    expect(Timing.parse(log)).toEqual([
      { phase: "generate3.load", ms: 50 },
      { phase: "cut.mirror", ms: 7, prep: 9 }
    ])
  })
})

describe("Timing.stopwatch", () => {
  it("charges each lap to its step, sums a repeated step, and totals from the start", () => {
    let now = 1000
    const watch = Timing.stopwatch(() => now)
    now = 1100.4
    watch.lap("read")
    now = 1400
    watch.lap("cut")
    now = 1450
    watch.lap("read")
    expect(watch.steps()).toEqual({ read: 150, cut: 300 })
    now = 1500
    expect(watch.total()).toBe(500)
  })
})

describe("Timing.selfPeakKb", () => {
  it("is this process's own peak resident set, in kilobytes", () => {
    expect(Timing.selfPeakKb()).toBeGreaterThan(1024)
  })
})
