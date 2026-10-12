/**
 * The filter bar, the ticked rows and the predicates as ONE selection the
 * server takes; the knobs held to numbers over the defaults; the lane's
 * ceilings read against them; the preview's line.
 */
import { describe, expect, it } from "vitest"
import type { TriageCallRow } from "../src/api.ts"
import { EMPTY } from "../src/queue-filter.ts"
import {
  ceilingNotes,
  createLine,
  describeSelection,
  knobsOf,
  NO_KNOBS,
  NO_PREDICATES,
  previewLine,
  rowKey,
  selectionOf
} from "../src/request-builder.ts"

const row = (capture: string, caseId: string, over: Partial<TriageCallRow> = {}): TriageCallRow => ({
  capture,
  caseId,
  kind: "case",
  callKey: `${caseId}@key`,
  runId: 1,
  setKind: "full",
  prepId: null,
  bundlePath: null,
  documentHash: null,
  passed: false,
  skipped: null,
  failedStep: null,
  rfcGating: null,
  reasons: [],
  features: [],
  mainError: null,
  finishedAt: null,
  family: null,
  dialled: null,
  outcome: null,
  legs: null,
  attempts: null,
  bundle: true,
  ...over
})

const A = "a.pcap.gz"
const B = "b.pcap.gz"
const shown = [row(A, `${A}-case1`), row(A, `${A}-case2`), row(B, `${B}-case1`, { callKey: null })]

describe("the selection", () => {
  it("is the filter bar as it stands, chips and excludes included, blanks left out", () => {
    const bar = {
      lane: "",
      scope: "red",
      q: "",
      filter: { ...EMPTY, include: { ...EMPTY.include, rule: "died" }, exclude: { ...EMPTY.exclude, refusal: ["x"] } }
    }
    const selection = selectionOf("filter", bar, [], shown, NO_PREDICATES)
    expect(selection).toEqual({ kind: "filter", scope: "red", rule: "died", "not-refusal": ["x"] })
    expect(describeSelection(selection as never)).toBe("--scope red --rule died --not-refusal x")
  })

  it("carries the bar's lane on a filter, and the panel's line states both lanes", () => {
    const bar = { lane: "fake", scope: "failed", q: "", filter: EMPTY }
    const selection = selectionOf("filter", bar, [], shown, NO_PREDICATES)
    expect(selection).toEqual({ kind: "filter", lane: "fake", scope: "failed" })
    expect(createLine("request", ["lab"], selection as never)).toBe("request --lane lab --on-lane fake --scope failed")
    expect(createLine("request", [], selection as never)).toBe("request --lane <lane> --on-lane fake --scope failed")
    // The other forms know no bar lane.
    expect(selectionOf("captures", bar, [], shown, NO_PREDICATES)).toEqual({ kind: "captures", captures: [A, B] })
  })

  it("is the ticked rows as cells: by key, by case id where the row has none", () => {
    const bar = { lane: "", scope: "failed", q: "", filter: EMPTY }
    expect(selectionOf("ticked", bar, [], shown, NO_PREDICATES)).toBe("tick a row first")
    expect(selectionOf("ticked", bar, [shown[0]!, shown[2]!], shown, NO_PREDICATES)).toEqual({
      kind: "cells",
      cells: [{ capture: A, callKey: `${A}-case1@key` }, { capture: B, caseId: `${B}-case1` }]
    })
  })

  it("is the ticked rows' captures whole, or every capture on screen where none is ticked", () => {
    const bar = { lane: "", scope: "failed", q: "", filter: EMPTY }
    expect(selectionOf("captures", bar, [shown[1]!], shown, NO_PREDICATES)).toEqual({ kind: "captures", captures: [A] })
    expect(selectionOf("captures", bar, [], shown, NO_PREDICATES)).toEqual({ kind: "captures", captures: [A, B] })
    expect(selectionOf("captures", bar, [], [], NO_PREDICATES)).toBe("no capture on screen")
  })

  it("is the population under its predicates, unstated ones left out", () => {
    const bar = { lane: "", scope: "failed", q: "", filter: EMPTY }
    const selection = selectionOf("population", bar, [], shown, {
      universe: "all",
      maxSpan: "120",
      greenOn: "",
      sample: "x",
      limit: "5"
    })
    expect(selection).toEqual({ kind: "universe", universe: "all", maxSpan: 120, limit: 5 })
    expect(describeSelection(selection as never)).toBe("--all --max-span 120 --limit 5")
  })

  it("keys a row by capture and case", () => {
    expect(rowKey(shown[0]!)).toBe(`${A}#${A}-case1`)
  })
})

describe("the knobs", () => {
  const defaults = { concurrency: 8, batchSize: 1000, memoryMax: "4G", startsPerSecond: 0, cleanup: true }

  it("take what was typed over the defaults, held to whole positive numbers", () => {
    expect(knobsOf(NO_KNOBS, defaults)).toEqual(defaults)
    expect(knobsOf({ ...NO_KNOBS, concurrency: "0", batchSize: "2.7", memoryMax: " 3G ", startsPerSecond: "-1" }, defaults))
      .toEqual({ concurrency: 1, batchSize: 2, memoryMax: "3G", startsPerSecond: 0, cleanup: true })
  })

  it("read a lane's ceilings against them", () => {
    const real = { lane: "local-real", exclusive: true, cellCeiling: 600, startCeiling: 50 }
    expect(ceilingNotes({ ...defaults, concurrency: 800 }, real)).toEqual([
      "local-real clamps concurrency to 600",
      "local-real paces starts at 50/s",
      "local-real runs one replay at a time; a second waits by name"
    ])
    expect(ceilingNotes({ ...defaults, startsPerSecond: 10 }, real)).toEqual([
      "local-real runs one replay at a time; a second waits by name"
    ])
    expect(ceilingNotes(defaults, { lane: "fake", exclusive: false, cellCeiling: null, startCeiling: null })).toEqual([])
  })
})

describe("the preview", () => {
  it("reads as one line per lane", () => {
    const fake = {
      lane: "fake",
      cells: 41,
      captures: [{ capture: A, cells: 41, cut: 4862, kind: "cases" as const }],
      whole: [],
      unkeyed: 2
    }
    expect(previewLine(fake)).toBe("41 cell(s) over 1 capture(s) — 0 full, 1 as cases; 2 row(s) no cut names a key for")
  })
})
