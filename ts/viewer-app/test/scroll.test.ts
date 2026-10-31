import { describe, expect, it } from "vitest"
import { anchorOf, scrollTopFor } from "../src/components/scroll.ts"

const box = (scrollTop: number) => ({ scrollTop, clientHeight: 400, scrollHeight: 2000 })

describe("row alignment", () => {
  it("puts the row at the anchor's offset in the box", () => {
    expect(scrollTopFor(box(0), 900, 150)).toBe(750)
    expect(scrollTopFor(box(300), 900, 150)).toBe(750)
  })

  it("leaves a row already at the anchor where it is", () => {
    expect(scrollTopFor(box(750), 900, 150)).toBeUndefined()
    expect(scrollTopFor(box(750.4), 900, 150)).toBeUndefined()
  })

  it("clamps to what the box can scroll", () => {
    expect(scrollTopFor(box(500), 100, 150)).toBe(0)
    expect(scrollTopFor(box(0), 1990, 10)).toBe(1600)
  })

  it("pulls an anchor past the box's edges back inside it", () => {
    expect(scrollTopFor(box(0), 900, 600)).toBe(900 - (400 - 48))
    expect(scrollTopFor(box(0), 900, 10)).toBe(900 - 48)
  })

  it("centres for a selection made off the ladders", () => {
    expect(scrollTopFor(box(0), 900, "centre")).toBe(700)
  })

  it("without an anchor, leaves a visible row and centres a hidden one", () => {
    expect(scrollTopFor(box(0), 200, undefined)).toBeUndefined()
    expect(scrollTopFor(box(0), 900, undefined)).toBe(700)
  })

  it("anchors a clicked row at its offset, a row off screen at the centre", () => {
    expect(anchorOf(box(600), 750)).toBe(150)
    expect(anchorOf(box(600), 1200)).toBe("centre")
  })
})
