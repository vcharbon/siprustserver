import { describe, expect, it } from "vitest"
import { applyTheme, nextTheme, recallTheme, rememberTheme, THEME_KEY, type ThemeStore } from "../src/theme.ts"

const memory = (initial: Record<string, string> = {}): ThemeStore & { readonly data: Record<string, string> } => {
  const data = { ...initial }
  return {
    data,
    getItem: (key) => data[key] ?? null,
    setItem: (key, value) => {
      data[key] = value
    }
  }
}

const throwing: ThemeStore = {
  getItem: () => {
    throw new Error("denied")
  },
  setItem: () => {
    throw new Error("denied")
  }
}

describe("theme", () => {
  it("cycles auto → light → dark → auto", () => {
    expect(nextTheme("auto")).toBe("light")
    expect(nextTheme("light")).toBe("dark")
    expect(nextTheme("dark")).toBe("auto")
  })

  it("reads auto where nothing, an unknown value, or a throwing store", () => {
    expect(recallTheme(memory())).toBe("auto")
    expect(recallTheme(memory({ [THEME_KEY]: "sepia" }))).toBe("auto")
    expect(recallTheme(throwing)).toBe("auto")
    expect(recallTheme(undefined)).toBe("auto")
  })

  it("remembers what it stores, and survives a store that refuses", () => {
    const store = memory()
    rememberTheme(store, "dark")
    expect(recallTheme(store)).toBe("dark")
    expect(() => rememberTheme(throwing, "light")).not.toThrow()
  })

  it("sets data-theme for light and dark, clears it for auto", () => {
    const attributes = new Map<string, string>()
    const root = {
      setAttribute: (name: string, value: string) => void attributes.set(name, value),
      removeAttribute: (name: string) => void attributes.delete(name)
    }
    applyTheme(root, "dark")
    expect(attributes.get("data-theme")).toBe("dark")
    applyTheme(root, "light")
    expect(attributes.get("data-theme")).toBe("light")
    applyTheme(root, "auto")
    expect(attributes.has("data-theme")).toBe(false)
  })
})
