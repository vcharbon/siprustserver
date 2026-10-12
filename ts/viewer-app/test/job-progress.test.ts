/**
 * The bar's arithmetic: landed over planned, the landed part split by what
 * the results say, the unresulted remainder its own run, the gone cells at
 * the end; a plan of nothing is an empty bar; the pace reads off the row's
 * instants and says nothing once the job has ended.
 */
import { describe, expect, it } from "vitest"
import type { JobProgress } from "../src/api.ts"
import { barOf, durationText, paceOf, progressOfListed, unitOf } from "../src/job-progress.ts"

const progress = (rest: Partial<JobProgress>): JobProgress => ({
  total: 0,
  landed: 0,
  pending: 0,
  goneRefused: 0,
  goneMissing: 0,
  passed: 0,
  failed: 0,
  skipped: 0,
  runs: 0,
  openRuns: 0,
  ...rest
})

describe("barOf", () => {
  it("splits the landed cells by result and rides the rest as landed, the gone ones last", () => {
    const bar = barOf(progress({ total: 100, landed: 60, pending: 30, goneRefused: 8, goneMissing: 2, passed: 30, failed: 10 }))
    expect(bar.percent).toBe(60)
    expect(bar.gone).toBe(10)
    expect(bar.segments.map((one) => [one.kind, one.count, one.percent])).toEqual([
      ["passed", 30, 30],
      ["failed", 10, 10],
      ["landed", 20, 20],
      ["gone", 10, 10]
    ])
  })

  it("is empty over a plan of nothing, never a division", () => {
    const bar = barOf(progress({}))
    expect(bar.segments).toEqual([])
    expect(bar.percent).toBe(0)
  })

  it("never counts more results than landed cells", () => {
    // Results written for cells landed under an earlier count: the remainder is clamped at zero.
    const bar = barOf(progress({ total: 10, landed: 4, passed: 4, failed: 1 }))
    expect(bar.segments.find((one) => one.kind === "landed")).toBeUndefined()
  })
})

describe("paceOf", () => {
  const started = "2026-09-20T10:00:00.000Z"
  const now = new Date("2026-09-20T10:01:40.000Z").getTime()

  it("reads the rate off the start instant and says what is left while the job runs", () => {
    const pace = paceOf(progress({ landed: 200, pending: 100 }), started, null, now)!
    expect(pace.elapsedSeconds).toBe(100)
    expect(pace.rate).toBe(2)
    expect(pace.leftSeconds).toBe(50)
  })

  it("reads the elapsed off the finish instant and says nothing is left once ended", () => {
    const pace = paceOf(progress({ landed: 200 }), started, "2026-09-20T10:00:50.000Z", now)!
    expect(pace.elapsedSeconds).toBe(50)
    expect(pace.rate).toBe(4)
    expect(pace.leftSeconds).toBeUndefined()
  })

  it("has no pace before the job started, nor any left before anything landed", () => {
    expect(paceOf(progress({}), null, null, now)).toBeUndefined()
    expect(paceOf(progress({ pending: 5 }), started, null, now)!.leftSeconds).toBeUndefined()
  })
})

describe("the list row's progress and words", () => {
  it("draws a row's counts as landed with the red among them and the gone at the end", () => {
    const bar = barOf(progressOfListed({ landed: 50, total: 100, failed: 5, gone: 10 }))
    expect(bar.segments.map((one) => [one.kind, one.count])).toEqual([["failed", 5], ["landed", 45], ["gone", 10]])
    expect(progressOfListed({ landed: 50, total: 100, failed: 5, gone: 10 }).pending).toBe(40)
  })

  it("names the unit by kind and a duration in the biggest two units", () => {
    expect([unitOf("replay"), unitOf("delete"), unitOf("cut"), unitOf("ingest")]).toEqual(["cell", "run", "capture", "capture"])
    expect([durationText(12), durationText(245), durationText(3720)]).toEqual(["12s", "4m 05s", "1h 02m"])
  })
})
