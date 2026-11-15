/**
 * The Sources page's replay selection: the ticked captures whole, held
 * across pages, else every capture shown; a capture with no cut of cases
 * (never cut, broken, or empty) left out and named, so it cannot refuse the
 * request; the header box ticks every shown capture, or unticks them all
 * when they all were, leaving the other pages' ticks as they are.
 */
import { describe, expect, it } from "vitest"
import type { SourceRow } from "../src/api.ts"
import { sourcesSelection, tickAllShown } from "../src/sources-selection.ts"

const row = (capture: string, over: Partial<SourceRow> = {}): SourceRow => ({
  capture,
  sourcePath: null,
  addedAt: "2026-11-15T00:00:00Z",
  ingestedAt: "2026-11-15T00:00:00Z",
  ingest: "current",
  ingestFailed: null,
  prepId: 1,
  cutAt: "2026-11-15T00:00:00Z",
  cut: "current",
  cutBroke: null,
  cases: 3,
  refusals: 0,
  spanSeconds: 1,
  standings: [],
  ...over
})

const ticks = (...rows: SourceRow[]): ReadonlyMap<string, SourceRow> => new Map(rows.map((one) => [one.capture, one]))

describe("the Sources replay selection", () => {
  it("replays the ticked captures whole, those of other pages included", () => {
    expect(sourcesSelection(ticks(row("c-2"), row("a-9")), [row("b-1"), row("c-2")])).toEqual({
      selection: { kind: "captures", captures: ["a-9", "c-2"] },
      left: []
    })
  })

  it("replays every capture shown where none is ticked, a stale cut included", () => {
    expect(sourcesSelection(new Map(), [row("b-1", { cut: "stale" }), row("a-1")])).toEqual({
      selection: { kind: "captures", captures: ["a-1", "b-1"] },
      left: []
    })
  })

  it("leaves out and names a capture never cut, broken, or of no case", () => {
    const shown = [row("a"), row("n", { cut: "never" }), row("b", { cut: "broke" }), row("z", { cases: 0 })]
    expect(sourcesSelection(new Map(), shown)).toEqual({
      selection: { kind: "captures", captures: ["a"] },
      left: ["b", "n", "z"]
    })
  })

  it("says why there is nothing to replay", () => {
    expect(sourcesSelection(new Map(), []).selection).toBe("no capture on screen")
    expect(sourcesSelection(ticks(row("n", { cut: "never" })), []).selection).toBe("no capture asked for has a cut with cases")
  })
})

describe("the tick-all box", () => {
  it("ticks every shown capture and keeps the ticks of other pages", () => {
    expect([...tickAllShown(ticks(row("x")), [row("a"), row("b")]).keys()].sort()).toEqual(["a", "b", "x"])
  })

  it("unticks the shown captures when they all were ticked, and only them", () => {
    expect([...tickAllShown(ticks(row("a"), row("b"), row("x")), [row("a"), row("b")]).keys()]).toEqual(["x"])
  })

  it("leaves the ticks as they are on an empty page", () => {
    expect([...tickAllShown(ticks(row("x")), []).keys()]).toEqual(["x"])
  })
})
