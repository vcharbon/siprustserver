import { describe, expect, it } from "vitest"
import { isDocked, wheelTo } from "../src/components/dock.ts"

describe("dock", () => {
  it("counts the workbench docked with its top on the bar's bottom, within a few px", () => {
    expect(isDocked(41, 41)).toBe(true)
    expect(isDocked(43, 41)).toBe(true)
    expect(isDocked(39, 41)).toBe(true)
  })

  it("counts it undocked below the bar or scrolled past it", () => {
    expect(isDocked(752, 41)).toBe(false)
    expect(isDocked(-200, 41)).toBe(false)
  })
})

describe("a wheel over the undocked workbench", () => {
  it("scrolls the page by the wheel, stopping where the workbench docks", () => {
    expect(wheelTo(0, 120, 752, 41)).toBe(120)
    expect(wheelTo(650, 120, 102, 41)).toBe(711)
  })

  it("scrolls back up to the dock from past it, no further", () => {
    expect(wheelTo(1000, -120, -200, 41)).toBe(880)
    expect(wheelTo(800, -120, -30, 41)).toBe(729)
  })

  it("leaves a wheel moving away from the dock to the page", () => {
    expect(wheelTo(0, -120, 752, 41)).toBeUndefined()
    expect(wheelTo(1000, 120, -200, 41)).toBeUndefined()
  })
})
