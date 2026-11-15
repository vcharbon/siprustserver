/**
 * The Sources page's replay selection: the ticked captures whole, held
 * across pages, else every capture shown; the header box ticks every shown
 * capture, or unticks them all when they all were, leaving the other pages'
 * ticks as they are.
 */
import { describe, expect, it } from "vitest"
import { sourcesSelection, tickAllShown } from "../src/sources-selection.ts"

describe("the Sources replay selection", () => {
  it("replays the ticked captures whole, those of other pages included", () => {
    expect(sourcesSelection(new Set(["c-2", "a-9"]), ["b-1", "c-2"])).toEqual({ kind: "captures", captures: ["a-9", "c-2"] })
  })

  it("replays every capture shown where none is ticked", () => {
    expect(sourcesSelection(new Set(), ["b-1", "a-1"])).toEqual({ kind: "captures", captures: ["a-1", "b-1"] })
  })

  it("says why there is nothing to replay on an empty page", () => {
    expect(sourcesSelection(new Set(), [])).toBe("no capture on screen")
  })
})

describe("the tick-all box", () => {
  it("ticks every shown capture and keeps the ticks of other pages", () => {
    expect([...tickAllShown(new Set(["x"]), ["a", "b"])].sort()).toEqual(["a", "b", "x"])
  })

  it("unticks the shown captures when they all were ticked, and only them", () => {
    expect([...tickAllShown(new Set(["a", "b", "x"]), ["a", "b"])]).toEqual(["x"])
  })

  it("leaves the ticks as they are on an empty page", () => {
    expect([...tickAllShown(new Set(["x"]), [])]).toEqual(["x"])
  })
})
