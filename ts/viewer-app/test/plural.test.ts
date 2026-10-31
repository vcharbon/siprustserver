import { describe, expect, it } from "vitest"
import { plural } from "../src/plural.ts"

describe("plural", () => {
  it("keeps the noun singular for one and plural otherwise", () => {
    expect(plural(1, "run")).toBe("1 run")
    expect(plural(0, "run")).toBe("0 runs")
    expect(plural(3, "run")).toBe("3 runs")
  })

  it("takes an irregular plural", () => {
    expect(plural(2, "entry", "entries")).toBe("2 entries")
    expect(plural(1, "entry", "entries")).toBe("1 entry")
  })
})
